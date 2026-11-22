//! `host_command` against a real child process, in its own test binary since
//! it changes the process environment.

use livestream_core::tools::host_command;

#[test]
fn child_does_not_inherit_the_appimage_bundle() {
    let appdir = "/tmp/.mount_LivestTest";
    std::env::set_var("APPDIR", appdir);
    std::env::set_var("LD_LIBRARY_PATH", format!("{appdir}/usr/lib/:"));
    std::env::set_var("GST_PLUGIN_SYSTEM_PATH_1_0", format!("{appdir}/usr/lib/gstreamer-1.0:/opt/gst"));
    let output = host_command("/usr/bin/env").output().expect("env runs");
    let env = String::from_utf8(output.stdout).unwrap();
    assert!(!env.lines().any(|l| l.starts_with("LD_LIBRARY_PATH=")), "{env}");
    assert!(env.lines().any(|l| l == "GST_PLUGIN_SYSTEM_PATH_1_0=/opt/gst"), "{env}");
}
