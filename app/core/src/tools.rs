//! Locating and starting the external binaries (gphoto2, ffmpeg, Aravis,
//! GStreamer).
//!
//! macOS GUI apps do not inherit the shell `PATH`, so the usual package
//! manager prefixes are checked after it. An AppImage launcher prepends its
//! bundle to `LD_LIBRARY_PATH`, `GST_PLUGIN_SYSTEM_PATH_1_0`, `PYTHONHOME`
//! and similar variables; host tools started with those load the bundle's
//! older libraries and fail, so [`host_command`] strips them.

use std::env;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;

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

/// Edits restoring the host environment from `vars` when running from the
/// AppImage mounted at `appdir`: every `:`-separated entry inside `appdir` is
/// dropped, along with the empty entries left behind. A variable left with no
/// entry is removed (`None`); untouched variables are not listed.
pub fn host_env_edits<I>(vars: I, appdir: &str) -> Vec<(String, Option<String>)>
where
    I: IntoIterator<Item = (String, String)>,
{
    let appdir = appdir.trim_end_matches('/');
    if appdir.is_empty() {
        return Vec::new();
    }
    vars.into_iter()
        .filter(|(_, value)| value.split(':').any(|e| inside(e, appdir)))
        .map(|(key, value)| {
            let kept: Vec<&str> = value.split(':').filter(|e| !e.is_empty() && !inside(e, appdir)).collect();
            (key, (!kept.is_empty()).then(|| kept.join(":")))
        })
        .collect()
}

fn inside(entry: &str, appdir: &str) -> bool {
    entry.strip_prefix(appdir).is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// [`Command`] for a host tool, with the AppImage bundle (when `APPDIR` is
/// set) removed from its environment by [`host_env_edits`].
pub fn host_command(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    if let Ok(appdir) = env::var("APPDIR") {
        let vars = env::vars_os().filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)));
        for (key, value) in host_env_edits(vars, &appdir) {
            match value {
                Some(value) => command.env(key, value),
                None => command.env_remove(key),
            };
        }
    }
    command
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

    fn edits(vars: &[(&str, &str)], appdir: &str) -> Vec<(String, Option<String>)> {
        host_env_edits(vars.iter().map(|(k, v)| (k.to_string(), v.to_string())), appdir)
    }

    /// Values as set by the AppImage launcher (`AppRun.wrapped`) of the 0.2.1 AppImage.
    #[test]
    fn strips_the_appimage_bundle_from_child_environments() {
        let a = "/tmp/.mount_LivestAbC";
        let vars = [
            ("LD_LIBRARY_PATH", format!("{a}/usr/lib/:{a}/usr/lib/x86_64-linux-gnu/:{a}/lib64/:")),
            ("GST_PLUGIN_SYSTEM_PATH_1_0", format!("{a}/usr/lib/gstreamer-1.0:/opt/gst")),
            ("PYTHONHOME", format!("{a}/usr/")),
            ("GIO_MODULE_DIR", format!("{a}//usr/lib/gio/modules")),
            ("XDG_DATA_DIRS", format!("{a}/usr/share:/usr/share:/usr/local/share")),
            ("PATH", format!("{a}/usr/bin/:/usr/local/bin:/usr/bin")),
            ("DISPLAY", ":0".to_string()),
            ("HOME", "/home/asp".to_string()),
            ("OTHER", "/tmp/.mount_LivestAbCdef/x".to_string()),
        ];
        let vars: Vec<(&str, &str)> = vars.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let s = |v: &str| Some(v.to_string());
        assert_eq!(
            edits(&vars, &format!("{a}/")),
            vec![
                ("LD_LIBRARY_PATH".to_string(), None),
                ("GST_PLUGIN_SYSTEM_PATH_1_0".to_string(), s("/opt/gst")),
                ("PYTHONHOME".to_string(), None),
                ("GIO_MODULE_DIR".to_string(), None),
                ("XDG_DATA_DIRS".to_string(), s("/usr/share:/usr/local/share")),
                ("PATH".to_string(), s("/usr/local/bin:/usr/bin")),
            ]
        );
        assert_eq!(edits(&vars, "/"), vec![]);
    }

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
