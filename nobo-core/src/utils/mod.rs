pub mod fs_utils;
pub mod note_utils;
pub mod string_utils;
pub mod search_utils;
pub mod tag_utils;
pub mod temp_file_utils;
pub mod diff_utils;
pub mod effort_utils;
pub(crate) mod timestamp_utils;
pub mod repeat_utils;
pub(crate) mod clock_utils;

/// Converts any `Result<T, impl Display>` into the `Result<T, String>` shape
/// every Tauri command returns, replacing the repeated
/// `.map_err(|e| e.to_string())` boilerplate at call sites.
pub trait ToCmdResult<T> {
    fn to_cmd(self) -> Result<T, String>;
}

impl<T, E: std::fmt::Display> ToCmdResult<T> for Result<T, E> {
    fn to_cmd(self) -> Result<T, String> {
        self.map_err(|e| e.to_string())
    }
}