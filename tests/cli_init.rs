use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

fn dispatch() -> Command {
    Command::cargo_bin("dispatch").unwrap()
}

#[test]
fn init_creates_config_and_prints_path_to_stdout() {
    let tmp = TempDir::new().unwrap();

    dispatch()
        .arg("init")
        .current_dir(tmp.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("dispatch.config.toml"))
        .stderr(predicate::str::contains("Created dispatch.config.toml"));

    let config = tmp.path().join("dispatch.config.toml");
    assert!(config.is_file());
}

#[test]
fn init_already_exists_errors_with_exit_code_2() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("dispatch.config.toml"), "").unwrap();

    dispatch()
        .arg("init")
        .current_dir(tmp.path())
        .assert()
        .code(2)
        .stderr(predicate::str::contains("already exists in this directory"));
}

#[test]
fn help_shows_init_subcommand() {
    dispatch()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("init"));
}
