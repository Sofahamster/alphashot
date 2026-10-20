use std::process::{Command, Output};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_alphashot"))
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn root_help_describes_direct_capture_and_invalid_arguments_are_actionable() {
    let result = run(&["--help"]);
    assert!(result.status.success());
    let help = String::from_utf8_lossy(&result.stdout);
    assert!(help.contains("[OPTIONS]"));
    assert!(help.contains("Run without arguments"));
    assert!(!help.contains("Commands:"));
    assert!(!result.stdout.contains(&0x1b));
    let result = run(&["--raw-prefix", "../oops"]);
    assert_eq!(result.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&result.stderr).contains("--raw-prefix"));
    for command in ["capture", "recover", "windows", "monitors"] {
        assert_eq!(run(&[command]).status.code(), Some(2));
    }
}
