use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn help_contains_expected_flags() {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("prsync"));
    cmd.arg("--help");
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("-v, --verbose"))
        .stdout(predicate::str::contains("-r, --recursive"))
        .stdout(predicate::str::contains("-P"))
        .stdout(predicate::str::contains("-l, --links"))
        .stdout(predicate::str::contains("-u, --update"));
}

#[test]
fn invalid_remote_spec_fails() {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("prsync"));
    cmd.args(["-r", "invalid-spec", "/tmp/dst"]);
    cmd.assert().failure().stderr(predicate::str::contains(
        "remote must be in format [user@]host:/path",
    ));
}
