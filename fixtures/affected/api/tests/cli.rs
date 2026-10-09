use std::process::Command;

#[test]
fn prints_the_total() {
    let output = Command::new(env!("CARGO_BIN_EXE_api"))
        .arg("1000")
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "12.00");
}
