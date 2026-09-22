use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::Mutex;
use std::time::Duration;

use crate::model::{BabelLanguage, ExecutionResult};

/// (file_path, session name from `:session foo`) -- headings/blocks with
/// the same key share one long-lived interpreter, so variables/imports
/// from an earlier block are still there for a later one. Deliberately
/// doesn't include language: a session name is expected to mean one
/// interpreter for its lifetime, same as in Emacs org-babel.
pub type SessionKey = (String, String);

/// How long a send waits for its marker before giving up. The same limit
/// a one-shot block gets, and the same reason (see `exec::TIMEOUT`) --
/// two numbers here would mean the same block times out differently for
/// having `:session` on it.
const MARKER_TIMEOUT: Duration = crate::exec::TIMEOUT;
/// Extra grace period after the stdout marker to let stderr (a separate
/// pipe/thread, not synchronized with stdout) catch up. Best-effort --
/// there's no way to know for certain that a session's stderr for this
/// send is fully drained versus belonging to a slow background write.
const STDERR_DRAIN: Duration = Duration::from_millis(80);

static MARKER_COUNTER: AtomicU64 = AtomicU64::new(0);

fn next_marker() -> String {
    let n = MARKER_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("__NOBO_BABEL_DONE_{nanos}_{n}__")
}

struct Session {
    child: Child,
    stdin: ChildStdin,
    stdout_rx: Receiver<String>,
    stderr_rx: Receiver<String>,
}

fn spawn_reader(reader: impl BufRead + Send + 'static) -> Receiver<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = reader;
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break, // EOF: process exited or closed the stream
                Ok(_) => {
                    if tx.send(line.trim_end_matches(['\n', '\r']).to_string()).is_err() {
                        break; // receiver gone (session dropped)
                    }
                }
                Err(_) => break,
            }
        }
    });
    rx
}

impl Session {
    /// `working_dir` is fixed for the session's whole life -- a process
    /// has one working directory and it is set at spawn. That is why it
    /// comes from the session KEY (`(file_path, session_name)`) rather
    /// than from the call: every send to this session belongs to the same
    /// note by definition.
    fn spawn(
        lang: BabelLanguage,
        interpreter_path: Option<&str>,
        working_dir: Option<&std::path::Path>,
    ) -> std::io::Result<Self> {
        let program = match (lang, interpreter_path) {
            (_, Some(path)) => path.to_string(),
            (BabelLanguage::Bash, None) => "bash".to_string(),
            (BabelLanguage::Python, None) => "python3".to_string(),
        };

        let mut cmd = Command::new(program);
        // The note's directory, exactly like a one-shot block (see
        // exec::run_once_within) -- so a relative path means the same
        // thing whether or not the block has a `:session`.
        if let Some(dir) = working_dir {
            cmd.current_dir(dir);
        }
        match lang {
            // -u: unbuffered, so our marker print isn't stuck behind a
            //     stdio buffer waiting to fill up.
            // -i: forces Python to execute each statement as it arrives
            //     instead of buffering the whole stdin stream until EOF
            //     (verified empirically -- plain `python3 < pipe` produces
            //     no output at all until the pipe closes; `-i` doesn't).
            //     Interactive prompts/banner this also turns on land on
            //     stderr, not stdout, so they don't pollute captured output.
            BabelLanguage::Python => { cmd.args(["-u", "-i"]); }
            // bash already executes commands as complete lines arrive on
            // a piped stdin, no flag needed.
            BabelLanguage::Bash => {}
        }
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        Ok(Self {
            child,
            stdin,
            stdout_rx: spawn_reader(BufReader::new(stdout)),
            stderr_rx: spawn_reader(BufReader::new(stderr)),
        })
    }

    fn marker_command(lang: BabelLanguage, marker: &str) -> String {
        match lang {
            BabelLanguage::Bash => format!("echo {marker}\n"),
            BabelLanguage::Python => format!("print({marker:?})\n"),
        }
    }

    /// Writes `code` followed by a marker print, then reads stdout lines
    /// until the marker reappears -- everything before it is this send's
    /// output. Leftover stderr is drained best-effort (see `STDERR_DRAIN`).
    fn send(&mut self, lang: BabelLanguage, code: &str) -> std::io::Result<ExecutionResult> {
        let marker = next_marker();

        self.stdin.write_all(code.as_bytes())?;
        self.stdin.write_all(b"\n")?;
        self.stdin.write_all(Self::marker_command(lang, &marker).as_bytes())?;
        self.stdin.flush()?;

        let mut stdout_lines = Vec::new();
        loop {
            match self.stdout_rx.recv_timeout(MARKER_TIMEOUT) {
                Ok(line) if line == marker => break,
                Ok(line) => stdout_lines.push(line),
                Err(RecvTimeoutError::Timeout) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "babel session did not respond in time",
                    ));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "babel session process exited",
                    ));
                }
            }
        }

        std::thread::sleep(STDERR_DRAIN);
        let mut stderr_lines = Vec::new();
        while let Ok(line) = self.stderr_rx.try_recv() {
            stderr_lines.push(line);
        }

        Ok(ExecutionResult {
            stdout: stdout_lines.join("\n"),
            stderr: stderr_lines.join("\n"),
            exit_code: 0,
            from_cache: false,
        })
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Default)]
pub struct SessionRegistry {
    sessions: Mutex<HashMap<SessionKey, Session>>,
}

impl SessionRegistry {
    /// Sends `code` to the session for `key`, spawning a fresh interpreter
    /// on first use. Subsequent calls with the same key reuse the same
    /// process, so state (variables, imports, function defs) persists.
    pub fn send(
        &self,
        key: SessionKey,
        lang: BabelLanguage,
        interpreter_path: Option<&str>,
        code: &str,
    ) -> std::io::Result<ExecutionResult> {
        let mut sessions = self.sessions.lock().unwrap_or_else(|poison| poison.into_inner());

        if !sessions.contains_key(&key) {
            // The key's first half is the note's path.
            // `parent()` of a bare filename is `Some("")`, which is not a
            // directory anything can chdir into -- that is "wherever we
            // already are", i.e. `None`.
            let working_dir = std::path::Path::new(&key.0)
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .map(|p| p.to_path_buf());
            sessions.insert(key.clone(), Session::spawn(lang, interpreter_path, working_dir.as_deref())?);
        }

        let session = sessions.get_mut(&key).expect("just inserted or already present");
        let result = session.send(lang, code);

        // A dead session (timed out/exited) can't serve future sends
        // either -- drop it so the next call starts a fresh one instead
        // of repeating the same failure forever.
        if result.is_err() {
            sessions.remove(&key);
        }

        result
    }

    pub fn kill(&self, key: &SessionKey) {
        let mut sessions = self.sessions.lock().unwrap_or_else(|poison| poison.into_inner());
        sessions.remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bash_session_keeps_variables_across_sends() {
        let registry = SessionRegistry::default();
        let key = ("test.org".to_string(), "s1".to_string());

        registry.send(key.clone(), BabelLanguage::Bash, None, "x=42").unwrap();
        let result = registry.send(key, BabelLanguage::Bash, None, "echo $x").unwrap();

        assert_eq!(result.stdout.trim(), "42");
    }

    #[test]
    fn python_session_keeps_variables_across_sends() {
        let registry = SessionRegistry::default();
        let key = ("test.org".to_string(), "py1".to_string());

        registry.send(key.clone(), BabelLanguage::Python, None, "x = 10").unwrap();
        let result = registry.send(key, BabelLanguage::Python, None, "print(x * 2)").unwrap();

        assert_eq!(result.stdout.trim(), "20");
    }

    #[test]
    fn different_session_keys_do_not_share_state() {
        let registry = SessionRegistry::default();
        let a = ("test.org".to_string(), "a".to_string());
        let b = ("test.org".to_string(), "b".to_string());

        registry.send(a, BabelLanguage::Bash, None, "x=1").unwrap();
        let result = registry.send(b, BabelLanguage::Bash, None, "echo ${x:-unset}").unwrap();

        assert_eq!(result.stdout.trim(), "unset");
    }

    #[test]
    fn kill_ends_the_process_so_next_send_starts_fresh() {
        let registry = SessionRegistry::default();
        let key = ("test.org".to_string(), "s2".to_string());

        registry.send(key.clone(), BabelLanguage::Bash, None, "x=99").unwrap();
        registry.kill(&key);
        let result = registry.send(key, BabelLanguage::Bash, None, "echo ${x:-unset}").unwrap();

        assert_eq!(result.stdout.trim(), "unset");
    }

    #[test]
    fn stdout_only_contains_marker_free_output_for_python() {
        let registry = SessionRegistry::default();
        let key = ("test.org".to_string(), "py2".to_string());

        let result = registry
            .send(key, BabelLanguage::Python, None, "print('hello')\nprint('world')")
            .unwrap();

        assert_eq!(result.stdout, "hello\nworld");
    }

    #[test]
    fn python_session_survives_multiple_sequential_sends() {
        let registry = SessionRegistry::default();
        let key = ("test.org".to_string(), "py3".to_string());

        registry.send(key.clone(), BabelLanguage::Python, None, "def double(n):\n    return n * 2\n").unwrap();
        registry.send(key.clone(), BabelLanguage::Python, None, "y = double(5)").unwrap();
        let result = registry.send(key, BabelLanguage::Python, None, "print(y)").unwrap();

        assert_eq!(result.stdout.trim(), "10");
    }
}
