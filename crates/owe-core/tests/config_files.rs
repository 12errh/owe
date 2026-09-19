//! File-level configuration tests: the loader, its error shapes, and the fact
//! that a broken config never half-applies (TRD FR-CORE-6, exit code 1).

use owe_core::{Config, ConfigError};

fn write(dir: &tempfile::TempDir, name: &str, contents: &str) -> std::path::PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, contents).expect("write temp config");
    path
}

#[test]
fn loads_a_valid_file_from_disk() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(
        &dir,
        "config.toml",
        "[shell]\nbackend = \"hyprland\"\n\n[library]\npaths = [\"/data/walls\"]\n",
    );

    let config = Config::load(&path).expect("valid config");
    assert_eq!(config.shell.backend, "hyprland");
    assert_eq!(config.library.paths, vec!["/data/walls".to_string()]);
}

#[test]
fn missing_file_reports_the_path_it_tried() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nope.toml");

    let err = Config::load(&path).unwrap_err();
    match &err {
        ConfigError::Io { path: reported, .. } => assert_eq!(reported, &path),
        other => panic!("unexpected error: {other:?}"),
    }
    assert!(err.to_string().contains("nope.toml"), "{err}");
    assert_eq!(err.exit_code(), 1);
}

#[test]
fn broken_toml_reports_a_parse_error_with_position() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(&dir, "config.toml", "[shell\nbackend = \"hyprland\"\n");

    let err = Config::load(&path).unwrap_err();
    assert!(matches!(err, ConfigError::Toml(_)), "{err:?}");
    assert!(err.to_string().contains("invalid TOML"), "{err}");
}

#[test]
fn validation_lists_every_problem_in_one_message() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(
        &dir,
        "config.toml",
        r#"
[shell]
backend = "gnome"

[render.buffering]
max_in_flight = 42

[library]
paths = ["relative/path"]
"#,
    );

    let err = Config::load(&path).unwrap_err();
    let text = err.to_string();
    assert!(text.contains("3 problem(s)"), "{text}");
    assert!(text.contains("shell.backend"), "{text}");
    assert!(text.contains("max_in_flight"), "{text}");
    assert!(text.contains("library.paths[0]"), "{text}");
}

#[test]
fn a_config_that_only_differs_by_a_typo_is_rejected_not_ignored() {
    // The whole point of deny_unknown_fields: silent misconfiguration is worse
    // than a loud failure for a daemon that runs unattended 24/7.
    let dir = tempfile::tempdir().unwrap();
    let path = write(&dir, "config.toml", "[governor]\neco_mod = true\n");

    let err = Config::load(&path).unwrap_err();
    assert!(err.to_string().contains("eco_mod"), "{err}");
}
