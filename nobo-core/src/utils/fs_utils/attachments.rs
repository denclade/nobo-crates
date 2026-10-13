//! Placing an attachment next to a note under a name nothing else has.

use std::path::{Path, PathBuf};
use std::{fs, io};

/// Creates DIR if needed, resolves a collision-free path for FILENAME inside
/// it (a numeric suffix on conflict, never overwriting), then hands that
/// final path to WRITE to actually create the file. Shared by
/// commands_notes::copy_attachment_file (`fs::copy` from a real OS path,
/// e.g. an OS-level drag-and-drop) and save_attachment_bytes (`fs::write`,
/// e.g. a pasted clipboard image) -- the editor's attach-a-file flow.
pub fn place_attachment(dir: &Path, filename: &str, write: impl FnOnce(&Path) -> io::Result<()>) -> io::Result<String> {
    fs::create_dir_all(dir)?;
    let dest = unique_path_in(dir, filename);
    write(&dest)?;
    Ok(dest.to_string_lossy().into_owned())
}

pub(super) fn unique_path_in(dir: &Path, filename: &str) -> PathBuf {
    let candidate = dir.join(filename);
    if !candidate.exists() {
        return candidate;
    }

    let stem = Path::new(filename).file_stem().and_then(|s| s.to_str()).unwrap_or("file");
    let ext = Path::new(filename).extension().and_then(|e| e.to_str());
    let mut n = 2;
    loop {
        let name = match ext {
            Some(e) => format!("{stem}-{n}.{e}"),
            None => format!("{stem}-{n}"),
        };
        let candidate = dir.join(&name);
        if !candidate.exists() {
            return candidate;
        }
        n += 1;
    }
}
