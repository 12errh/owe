//! Pins the daemon-side and client-side socket-path rules to each other.
//!
//! The rule lives in two places on purpose (ARCHITECTURE §2): the daemon uses
//! `owe_core::path::XdgPaths` (it already owns XDG resolution), while clients use
//! `owe_ipc::socket_path_in` so the GUI can depend on `owe-ipc` alone. Duplication
//! like that is exactly how daemon and client silently stop finding each other —
//! so it gets a test rather than a comment.

use std::ffi::OsStr;

use owe_core::path::XdgPaths;

fn daemon_path(runtime_dir: Option<&str>) -> Option<std::path::PathBuf> {
    XdgPaths::resolve_with(|name| match name {
        "HOME" => Some("/home/owe".to_string()),
        "XDG_RUNTIME_DIR" => runtime_dir.map(str::to_string),
        _ => None,
    })
    .expect("xdg resolution")
    .socket_path()
    .ok()
}

#[test]
fn daemon_and_client_agree_on_the_socket_path() {
    for runtime in ["/run/user/1000", "/tmp/run", "/run/user/4242", "/x"] {
        let client = owe_ipc::socket_path_in(Some(OsStr::new(runtime)));
        assert_eq!(
            daemon_path(Some(runtime)),
            client,
            "runtime dir {runtime} resolved differently on the two sides"
        );
        assert_eq!(
            client,
            Some(std::path::PathBuf::from(format!("{runtime}/owe/socket")))
        );
    }
}

#[test]
fn both_sides_treat_a_missing_runtime_dir_as_no_socket() {
    assert_eq!(daemon_path(None), None);
    assert_eq!(owe_ipc::socket_path_in(None), None);
}

#[test]
fn both_sides_treat_an_empty_runtime_dir_as_no_socket() {
    // `XDG_RUNTIME_DIR=""` is common in containers and broken systemd units.
    // Both sides must agree that it means "no session".
    assert_eq!(daemon_path(Some("")), None);
    assert_eq!(owe_ipc::socket_path_in(Some(OsStr::new(""))), None);
}
