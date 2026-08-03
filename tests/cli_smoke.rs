use std::process::Command;

#[test]
fn cli_help_returns_zero() {
    let output = Command::new(env!("CARGO_BIN_EXE_waitagent"))
        .arg("--help")
        .output()
        .expect("binary exists");
    assert!(
        output.status.success(),
        "stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn cli_version_returns_zero() {
    let output = Command::new(env!("CARGO_BIN_EXE_waitagent"))
        .arg("version")
        .output()
        .expect("binary exists");
    assert!(
        output.status.success(),
        "stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("waitagent"),
        "expected version output, got: {stdout}"
    );
}
