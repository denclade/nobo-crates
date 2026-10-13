use std::collections::HashSet;
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::model::InterpreterInfo;

/// GUI-launched apps (as opposed to a terminal) often inherit a minimal
/// PATH that's missing a user's shell-profile customizations -- these are
/// checked in addition to PATH so discovery doesn't come up empty just
/// because the app wasn't started from a shell.
const FALLBACK_DIRS: &[&str] = &["/usr/local/bin", "/usr/bin", "/bin", "/opt/homebrew/bin"];

fn path_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = env::var_os("PATH").map(|p| env::split_paths(&p).collect()).unwrap_or_default();

    for fallback in FALLBACK_DIRS {
        let path = PathBuf::from(fallback);
        if !dirs.contains(&path) {
            dirs.push(path);
        }
    }

    dirs
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    path_dirs().into_iter().map(|dir| dir.join(name)).find(|p| p.is_file())
}

fn read_version(path: &Path) -> Option<String> {
    let output = Command::new(path).arg("--version").output().ok()?;
    // Python 2 printed --version to stderr; python3 uses stdout. Accept
    // either so the reported version doesn't silently come up empty.
    let bytes = if !output.stdout.is_empty() { output.stdout } else { output.stderr };
    let text = String::from_utf8_lossy(&bytes).trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// First `bash`, falling back to `sh`, found on PATH.
pub fn discover_shell() -> Option<PathBuf> {
    find_in_path("bash").or_else(|| find_in_path("sh"))
}

/// `uv` (<https://github.com/astral-sh/uv>), which creates venvs much
/// faster than `python -m venv` when it is available.
pub fn discover_uv() -> Option<PathBuf> {
    find_in_path("uv")
}

/// Every distinct `python3`/`python` found on PATH (+ fallback dirs),
/// deduped by resolved path since the two names often point at the same
/// binary.
pub fn discover_pythons() -> Vec<InterpreterInfo> {
    let mut seen = HashSet::new();
    let mut results = Vec::new();

    for name in ["python3", "python"] {
        for dir in path_dirs() {
            let candidate = dir.join(name);
            if !candidate.is_file() {
                continue;
            }
            let canonical = candidate.canonicalize().unwrap_or_else(|_| candidate.clone());
            if !seen.insert(canonical) {
                continue;
            }

            results.push(InterpreterInfo { path: candidate.to_string_lossy().into_owned(), version: read_version(&candidate) });
        }
    }

    results
}

/// Resolves a freshly created venv's own interpreter binary, covering both
/// the Unix (`bin/python3`) and Windows (`Scripts/python.exe`) layouts.
fn venv_python_path(venv_dir: &Path) -> Option<PathBuf> {
    for candidate in
        [venv_dir.join("bin").join("python3"), venv_dir.join("bin").join("python"), venv_dir.join("Scripts").join("python.exe")]
    {
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Creates a virtual environment at `target_dir`. Uses `uv venv` when `uv`
/// is on PATH (much faster), otherwise falls back to `<python> -m venv`
/// with `python_path` (or the first auto-discovered interpreter). Returns
/// the venv's own interpreter path, ready to save as
/// `Config::python_interpreter_path`.
pub fn create_venv(target_dir: &Path, python_path: Option<&str>) -> std::io::Result<PathBuf> {
    let status = if let Some(uv) = discover_uv() {
        let mut cmd = Command::new(uv);
        cmd.arg("venv").arg(target_dir);
        if let Some(python) = python_path {
            cmd.arg("--python").arg(python);
        }
        cmd.status()?
    } else {
        let python = python_path
            .map(PathBuf::from)
            .or_else(|| discover_pythons().into_iter().next().map(|i| PathBuf::from(i.path)))
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no python interpreter found to create a venv with"))?;
        Command::new(python).arg("-m").arg("venv").arg(target_dir).status()?
    };

    if !status.success() {
        return Err(std::io::Error::new(std::io::ErrorKind::Other, "venv creation failed"));
    }

    venv_python_path(target_dir)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "venv created but its interpreter binary wasn't found"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // These assert against the real dev machine's PATH (bash/python3 are
    // safe to assume present on any Linux CI/dev box this repo builds on).

    #[test]
    fn finds_a_shell() {
        assert!(discover_shell().is_some());
    }

    #[test]
    fn finds_at_least_one_python() {
        assert!(!discover_pythons().is_empty());
    }

    #[test]
    fn python_results_have_no_duplicate_paths() {
        let pythons = discover_pythons();
        let unique: HashSet<_> = pythons.iter().map(|p| p.path.clone()).collect();
        assert_eq!(unique.len(), pythons.len());
    }

    #[test]
    fn unknown_binary_is_not_found() {
        assert!(find_in_path("definitely-not-a-real-binary-xyz").is_none());
    }

    /// Needs a Python that can actually build a venv. Some environments
    /// ship an interpreter without the `venv` module (Debian splits it
    /// into python3-venv, and this project's own devcontainer is one of
    /// them) -- there the test can only fail, and a suite with one
    /// permanently red test is a suite nobody reads. Skipped there,
    /// loudly, and run wherever it can say something.
    #[test]
    fn create_venv_produces_a_working_interpreter() {
        let dir = TempDir::new().unwrap();
        let venv_dir = dir.path().join("venv");

        let python = match create_venv(&venv_dir, None) {
            Ok(python) => python,
            Err(e) => {
                eprintln!("skipped: no Python that can create a venv here ({e})");
                return;
            }
        };
        assert!(python.is_file());

        // The venv's own interpreter should actually run.
        let output = Command::new(&python).arg("-c").arg("print(1 + 1)").output().unwrap();
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "2");
    }
}
