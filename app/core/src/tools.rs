//! Locating the external producer binaries (gphoto2, ffmpeg).
//!
//! macOS GUI apps do not inherit the shell `PATH`, so the usual package
//! manager prefixes are checked after it.

use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Directories checked after `PATH` (Homebrew arm64/x86_64, MacPorts, system).
pub const FALLBACK_DIRS: [&str; 4] = ["/opt/homebrew/bin", "/usr/local/bin", "/opt/local/bin", "/usr/bin"];

/// First executable `name` in the directories of `path_var`, then in `fallback_dirs`.
pub fn find_binary(name: &str, path_var: Option<OsString>, fallback_dirs: &[&str]) -> Option<PathBuf> {
    let from_path: Vec<PathBuf> = path_var.map(|p| env::split_paths(&p).collect()).unwrap_or_default();
    from_path
        .into_iter()
        .chain(fallback_dirs.iter().map(PathBuf::from))
        .map(|dir| dir.join(name))
        .find(|p| is_executable(p))
}

/// [`find_binary`] against the process `PATH` and [`FALLBACK_DIRS`].
pub fn locate(name: &str) -> Option<PathBuf> {
    find_binary(name, env::var_os("PATH"), &FALLBACK_DIRS)
}

/// Package manager command installing `package` on `os` (as in [`std::env::consts::OS`]).
pub fn install_hint(package: &str, os: &str) -> String {
    match os {
        "macos" => format!("brew install {package}"),
        "linux" => format!("sudo apt install {package}"),
        _ => format!("install {package} with your package manager"),
    }
}

/// Error message for a missing `name`, with the install command for the current OS.
pub fn not_found(name: &str) -> String {
    format!(
        "{name} not found in PATH or {} — install it with `{}`",
        FALLBACK_DIRS.join(", "),
        install_hint(name, env::consts::OS)
    )
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn prefers_path_then_fallbacks_and_requires_exec_bit() {
        use std::os::unix::fs::PermissionsExt;
        let root = env::temp_dir().join(format!("livestream-find-{}", std::process::id()));
        let (a, b) = (root.join("a"), root.join("b"));
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        let fallback = b.join("tool");
        fs::write(&fallback, "").unwrap();
        fs::set_permissions(&fallback, fs::Permissions::from_mode(0o755)).unwrap();
        let fallback_dir = b.to_str().unwrap();
        let path = env::join_paths([&a]).unwrap();

        assert_eq!(find_binary("tool", Some(path.clone()), &[fallback_dir]), Some(fallback.clone()));

        let not_exec = a.join("tool");
        fs::write(&not_exec, "").unwrap();
        assert_eq!(find_binary("tool", Some(path.clone()), &[fallback_dir]), Some(fallback.clone()));

        fs::set_permissions(&not_exec, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(find_binary("tool", Some(path), &[fallback_dir]), Some(not_exec));
        assert_eq!(find_binary("tool", None, &["/nonexistent"]), None);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn per_os_hints() {
        assert_eq!(install_hint("gphoto2", "macos"), "brew install gphoto2");
        assert_eq!(install_hint("ffmpeg", "linux"), "sudo apt install ffmpeg");
        assert_eq!(install_hint("ffmpeg", "freebsd"), "install ffmpeg with your package manager");
        assert!(not_found("ffmpeg").starts_with("ffmpeg not found in PATH or /opt/homebrew/bin"));
    }
}
