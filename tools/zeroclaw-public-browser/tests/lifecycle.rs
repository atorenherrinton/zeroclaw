use std::process::Command;

#[test]
fn supervisor_owns_startup_cancellation_and_descendant_cleanup() {
    let helper = std::env::var_os("ZEROCLAW_PUBLIC_BROWSER_TEST_BINARY")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_zeroclaw-public-browser").into());
    let output = Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/lifecycle_fixture.py"
        ))
        .arg(helper)
        .output()
        .expect("Python synthetic process fixture starts");
    assert!(
        output.status.success(),
        "synthetic supervisor fixture failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}
