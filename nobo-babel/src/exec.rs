use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use nobo_core::config::settings::config_dir;
use crate::model::{BabelLanguage, ExecutionResult};

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// A Python string literal.
///
/// Not `format!("{value:?}")`. Rust's `str` Debug escapes anything outside
/// its own printable table as `\u{7}` / `\u{308}`, and Python's grammar
/// only knows `\uXXXX` with exactly four hex digits -- so that produced a
/// `SyntaxError`, not a wrong value. The realistic trigger is not an emoji
/// but NFD-decomposed text: `:var name="Müller"` from a macOS source
/// escapes the combining diaeresis as `\u{308}`.
///
/// Everything that is not a control character goes out verbatim; the
/// source is sent as UTF-8, so Python reads it back as written.
fn python_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn format_var_assignment(lang: BabelLanguage, key: &str, value: &str) -> String {
    let is_number = value.parse::<f64>().is_ok();
    match lang {
        BabelLanguage::Bash => {
            if is_number { format!("{key}={value}") } else { format!("{key}={}", shell_quote(value)) }
        }
        BabelLanguage::Python => {
            if is_number { format!("{key} = {value}") } else { format!("{key} = {}", python_quote(value)) }
        }
    }
}

/// Prepends `:var` assignments to the block's code, one statement per line,
/// in the target language's syntax. Numeric-looking values are emitted
/// unquoted; everything else as a quoted string -- the same literal-typing
/// inference org-babel applies to `:var` header args.
pub fn build_code(lang: BabelLanguage, vars: &[(String, String)], code: &str) -> String {
    if vars.is_empty() {
        return code.to_string();
    }
    let mut lines: Vec<String> = vars.iter().map(|(k, v)| format_var_assignment(lang, k, v)).collect();
    lines.push(code.to_string());
    lines.join("\n")
}

fn interpreter_program(lang: BabelLanguage, interpreter_path: Option<&str>) -> String {
    match (lang, interpreter_path) {
        (_, Some(path)) => path.to_string(),
        (BabelLanguage::Bash, None) => "bash".to_string(),
        (BabelLanguage::Python, None) => "python3".to_string(),
    }
}

/// How long a block may run before it is stopped.
///
/// A SRC block is somebody's code, so "it loops forever" is not an exotic
/// case -- it is a typo. Without a limit the process would sit there for
/// the life of the app, holding an interpreter and whatever it opened,
/// with nothing in the UI to end it (`babel_kill_session` only knows
/// about `:session` blocks).
///
/// Thirty seconds is the same limit a session waits for its marker
/// (`session::MARKER_TIMEOUT` takes this constant), because it is the
/// same question asked twice: how long may one block take.
pub const TIMEOUT: Duration = Duration::from_secs(30);

/// Runs `code` in a fresh, one-shot process -- the default when a SRC block
/// has no `:session`. Each call gets its own interpreter; no state carries
/// over to the next block (see `session.rs` for the stateful variant).
fn run_once(
    lang: BabelLanguage,
    interpreter_path: Option<&str>,
    code: &str,
    working_dir: Option<&Path>,
) -> std::io::Result<ExecutionResult> {
    run_once_within(lang, interpreter_path, code, working_dir, TIMEOUT)
}

/// The same with the limit spelled out, so a test does not have to wait
/// half a minute to prove that waiting ends.
fn run_once_within(
    lang: BabelLanguage,
    interpreter_path: Option<&str>,
    code: &str,
    // The directory of the note the block lives in -- see `current_dir`
    // below. `None` only where there is no owning file (tests).
    working_dir: Option<&Path>,
    limit: Duration,
) -> std::io::Result<ExecutionResult> {
    let mut command = Command::new(interpreter_program(lang, interpreter_path));
    // Relative paths resolve against the note, the way org-babel does it
    // -- and the way `:tangle` in this same project already did.
    //
    // Inheriting the app's own working directory meant `echo x > out.txt`
    // landed somewhere the user has no reason to look, and somewhere
    // different depending on how the app was started. Not a sandbox: the
    // block runs with the user's full account either way and an absolute
    // path does the same thing. The defect was that the answer was
    // unpredictable.
    if let Some(dir) = working_dir {
        command.current_dir(dir);
    }
    let mut child = command
        .arg("-c")
        .arg(code)
        // No inherited stdin: a block that reads from it would otherwise
        // wait on a terminal that is not there -- a hang the user cannot
        // even see, rather than an interpreter that stops at once.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    // Both pipes drained in their own threads, and that is not a detail:
    // a block that prints more than the pipe buffer holds blocks on its
    // own write until somebody reads. Waiting for the exit first would
    // deadlock exactly there -- and look precisely like the endless loop
    // this timeout exists for.
    let mut stdout = child.stdout.take().expect("stdout was piped");
    let mut stderr = child.stderr.take().expect("stderr was piped");
    let reading_out = std::thread::spawn(move || read_all(&mut stdout));
    let reading_err = std::thread::spawn(move || read_all(&mut stderr));

    let Some(status) = wait_until(&mut child, limit)? else {
        // Killed rather than left behind: an abandoned interpreter keeps
        // whatever it holds (a file, a port, a lock in the vault) for as
        // long as the app runs.
        let _ = child.kill();
        let _ = child.wait();
        return Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("the block ran longer than {} seconds and was stopped", limit.as_secs().max(1)),
        ));
    };

    Ok(ExecutionResult {
        stdout: reading_out.join().unwrap_or_default(),
        stderr: reading_err.join().unwrap_or_default(),
        exit_code: status.code().unwrap_or(-1),
        from_cache: false,
    })
}

/// As much output as is worth keeping, and not a byte more.
///
/// `read_to_end` had no bound at all, with a 30-second timeout above it:
/// a `yes` loop or an accidental log dump fills memory long before the
/// timeout fires, and whatever finishes inside those 30 seconds then goes
/// over IPC, into the note, onto disk, and with `:cache yes` into
/// `babel_cache.json` as well.
///
/// Capping the READ is the whole fix, and it needs no second termination
/// path: once nothing is draining the pipe the child blocks on its own
/// write, and the existing `TIMEOUT`/`kill` path collects it. The
/// alternative -- sharing the `Child` with the reader threads to kill it
/// from here -- is what `exec.rs`'s own comments rule out.
const OUTPUT_CAP: u64 = 1024 * 1024;

fn read_all(source: &mut impl Read) -> String {
    let mut buffer = Vec::new();
    let _ = source.take(OUTPUT_CAP).read_to_end(&mut buffer);
    let mut text = String::from_utf8_lossy(&buffer).into_owned();
    if buffer.len() as u64 >= OUTPUT_CAP {
        // Said out loud: silently cut output is worse than none, because
        // the half that is there looks complete.
        text.push_str(&nobo_core::t!(
            "\n[Ausgabe nach 1 MB abgeschnitten]",
            "\n[output truncated at 1 MB]"
        ));
    }
    text
}

/// Waits for `child` to end, giving up after `limit`.
///
/// Polled rather than blocked on, because the point is to stop waiting:
/// `Child::wait` has no deadline in std, and a process that has to be
/// killed cannot be killed by the thread stuck inside `wait`. The naps
/// double up to 25 ms, so a block that finishes instantly is answered
/// within a millisecond and a long one costs a handful of wakeups a
/// second.
fn wait_until(child: &mut Child, limit: Duration) -> std::io::Result<Option<ExitStatus>> {
    let deadline = Instant::now() + limit;
    let mut nap = Duration::from_millis(1);
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(None);
        }
        std::thread::sleep(nap.min(left));
        nap = (nap * 2).min(Duration::from_millis(25));
    }
}

// --- :cache -- hash of (language, interpreter, code) -> last result.
// Skips re-running unchanged code, which matters for anything slow
// (data processing, network calls) re-evaluated just because the
// surrounding document was touched elsewhere.

#[derive(Debug, Default, Serialize, Deserialize)]
struct CacheFile {
    #[serde(default)]
    entries: HashMap<String, ExecutionResult>,
}

fn cache_store_path() -> PathBuf {
    config_dir().join("babel_cache.json")
}

fn cache_key(lang: BabelLanguage, interpreter_path: Option<&str>, code: &str, working_dir: Option<&Path>) -> String {
    let mut hasher = DefaultHasher::new();
    lang.hash(&mut hasher);
    interpreter_path.hash(&mut hasher);
    code.hash(&mut hasher);
    working_dir.hash(&mut hasher);
    format!("{:x}", hasher.finish())
}

fn load_cache(path: &Path) -> CacheFile {
    fs::read_to_string(path)
        .ok()
        .and_then(|content| serde_json::from_str(&content).ok())
        .unwrap_or_default()
}

fn save_cache(path: &Path, cache: &CacheFile) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(cache).unwrap_or_default();
    // Regenerable, so the stakes are low -- but there is now exactly one
    // way to replace a file in this codebase, and an exception nobody can
    // check is worth less than the line it saves.
    nobo_core::utils::fs_utils::write_atomic(path, &content)
}

fn run_with_cache_at(
    cache_path: &Path,
    lang: BabelLanguage,
    interpreter_path: Option<&str>,
    code: &str,
    working_dir: Option<&Path>,
    use_cache: bool,
) -> std::io::Result<ExecutionResult> {
    // The directory is part of the key: the same code run from two notes
    // can read two different `data.csv`.
    let key = cache_key(lang, interpreter_path, code, working_dir);

    if use_cache {
        if let Some(mut cached) = load_cache(cache_path).entries.remove(&key) {
            cached.from_cache = true;
            return Ok(cached);
        }
    }

    let result = run_once(lang, interpreter_path, code, working_dir)?;

    if use_cache {
        let mut cache = load_cache(cache_path);
        cache.entries.insert(key, result.clone());
        save_cache(cache_path, &cache)?;
    }

    Ok(result)
}

/// Runs `code` (already merged with any `:var` assignments via
/// `build_code`) with `:cache` semantics: skip execution and return the
/// last result if the exact (language, interpreter, code) combination was
/// run before and `use_cache` is set.
pub fn run(
    lang: BabelLanguage,
    interpreter_path: Option<&str>,
    code: &str,
    working_dir: Option<&Path>,
    use_cache: bool,
) -> std::io::Result<ExecutionResult> {
    run_with_cache_at(&cache_store_path(), lang, interpreter_path, code, working_dir, use_cache)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn var_numeric_stays_unquoted() {
        assert_eq!(format_var_assignment(BabelLanguage::Bash, "x", "42"), "x=42");
        assert_eq!(format_var_assignment(BabelLanguage::Python, "x", "42"), "x = 42");
    }

    #[test]
    fn var_string_gets_quoted_per_language() {
        assert_eq!(format_var_assignment(BabelLanguage::Bash, "name", "hi"), "name='hi'");
        assert_eq!(format_var_assignment(BabelLanguage::Python, "name", "hi"), "name = \"hi\"");
    }

    #[test]
    fn var_string_with_quote_is_escaped_for_bash() {
        let out = format_var_assignment(BabelLanguage::Bash, "x", "it's");
        // Round-trips through the real shell: `echo $x` must print `it's`.
        let result = run_once(
            BabelLanguage::Bash,
            None,
            &format!("{out}\necho \"$x\""),
            None,
        ).unwrap();
        assert_eq!(result.stdout.trim(), "it's");
    }

    #[test]
    fn build_code_prepends_vars_before_the_block() {
        let code = build_code(
            BabelLanguage::Bash,
            &[("x".to_string(), "5".to_string())],
            "echo $x",
        );
        assert_eq!(code, "x=5\necho $x");
    }

    #[test]
    fn build_code_with_no_vars_returns_code_unchanged() {
        assert_eq!(build_code(BabelLanguage::Bash, &[], "echo hi"), "echo hi");
    }

    /// The point of the limit: a block that never ends is ended.
    #[test]
    fn a_block_that_never_finishes_is_stopped_and_says_so() {
        let started = Instant::now();
        let error = run_once_within(BabelLanguage::Bash, None, "sleep 30", None, Duration::from_millis(200))
            .expect_err("a sleeping block must not be waited out");

        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(error.to_string().contains("stopped"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(5), "it waited for the block after all");
    }

    /// And the process goes with it -- a timeout that leaves the
    /// interpreter running would only move the problem out of sight.
    #[test]
    #[cfg(unix)]
    fn the_stopped_block_leaves_no_process_behind() {
        let dir = TempDir::new().unwrap();
        let marker = dir.path().join("still-here");
        // Writes the marker only if it survives its first second.
        let code = format!("sleep 1; touch {}", marker.display());

        let _ = run_once_within(BabelLanguage::Bash, None, &code, None, Duration::from_millis(100));
        std::thread::sleep(Duration::from_millis(1500));

        assert!(!marker.exists(), "the killed block kept running");
    }

    /// More output than a pipe buffer holds (64 KiB on Linux): the reader
    /// threads are what keep this from deadlocking.
    /// A relative path means the note's folder, not wherever the app was
    /// started from -- the same rule `:tangle` follows.
    #[test]
    fn a_block_resolves_relative_paths_against_the_notes_folder() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("daten.txt"), "aus der notiz").unwrap();

        let result = run_once(BabelLanguage::Bash, None, "cat daten.txt", Some(dir.path())).unwrap();

        assert_eq!(result.stdout.trim(), "aus der notiz");
    }

    /// Two notes with the same code are two different runs, so the cache
    /// must not answer one with the other's output.
    #[test]
    fn the_cache_key_includes_the_working_directory() {
        let a = tempfile::TempDir::new().unwrap();
        let b = tempfile::TempDir::new().unwrap();

        assert_ne!(
            cache_key(BabelLanguage::Bash, None, "cat daten.txt", Some(a.path())),
            cache_key(BabelLanguage::Bash, None, "cat daten.txt", Some(b.path()))
        );
    }

    #[test]
    fn a_block_that_prints_more_than_the_pipe_holds_still_returns() {
        let result = run_once_within(
            BabelLanguage::Bash,
            None,
            "for i in $(seq 1 20000); do echo 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'; done",
            None,
            Duration::from_secs(20),
        )
        .unwrap();

        assert_eq!(result.exit_code, 0);
        assert!(result.stdout.len() > 200_000, "only {} bytes came back", result.stdout.len());
    }

    #[test]
    fn run_once_bash_captures_stdout() {
        let result = run_once(BabelLanguage::Bash, None, "echo hello", None).unwrap();
        assert_eq!(result.stdout.trim(), "hello");
        assert_eq!(result.exit_code, 0);
        assert!(!result.from_cache);
    }

    #[test]
    fn run_once_bash_captures_nonzero_exit_and_stderr() {
        let result = run_once(BabelLanguage::Bash, None, "echo oops >&2; exit 3", None).unwrap();
        assert_eq!(result.exit_code, 3);
        assert_eq!(result.stderr.trim(), "oops");
    }

    #[test]
    fn run_once_python_captures_stdout() {
        let result = run_once(BabelLanguage::Python, None, "print(1 + 1)", None).unwrap();
        assert_eq!(result.stdout.trim(), "2");
    }

    #[test]
    fn run_once_with_vars_end_to_end() {
        let code = build_code(
            BabelLanguage::Python,
            &[("name".to_string(), "world".to_string())],
            "print(f'hello {name}')",
        );
        let result = run_once(BabelLanguage::Python, None, &code, None).unwrap();
        assert_eq!(result.stdout.trim(), "hello world");
    }

    #[test]
    fn cache_miss_then_hit_skips_second_execution() {
        let dir = TempDir::new().unwrap();
        let cache_path = dir.path().join("cache.json");

        // Marker file lets us prove the SECOND call didn't actually re-run.
        let marker = dir.path().join("marker.txt");
        let code = format!("touch {}", marker.to_str().unwrap());

        let first = run_with_cache_at(&cache_path, BabelLanguage::Bash, None, &code, None, true).unwrap();
        assert!(!first.from_cache);
        assert!(marker.exists());

        fs::remove_file(&marker).unwrap();
        let second = run_with_cache_at(&cache_path, BabelLanguage::Bash, None, &code, None, true).unwrap();
        assert!(second.from_cache);
        assert!(!marker.exists(), "second run should have been served from cache, not re-executed");
    }

    #[test]
    fn cache_disabled_always_executes() {
        let dir = TempDir::new().unwrap();
        let cache_path = dir.path().join("cache.json");

        let first = run_with_cache_at(&cache_path, BabelLanguage::Bash, None, "echo hi", None, false).unwrap();
        let second = run_with_cache_at(&cache_path, BabelLanguage::Bash, None, "echo hi", None, false).unwrap();
        assert!(!first.from_cache);
        assert!(!second.from_cache);
    }

    #[test]
    fn different_code_has_different_cache_key() {
        assert_ne!(
            cache_key(BabelLanguage::Bash, None, "echo a", None),
            cache_key(BabelLanguage::Bash, None, "echo b", None),
        );
    }
}
