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
    assert!(help.contains("--no-clipboard"));
    assert!(help.contains("--clipboard-only"));
    assert!(help.contains("--editor <EXE>"));
    assert!(help.contains("--editor-arg <ARG>"));
    assert!(help.contains("--repeat"));
    for option in ["--process", "--window-class", "--wait"] {
        assert!(help.contains(option), "Missing help for {option}");
    }
    assert!(help.contains("--client-size <WIDTHxHEIGHT>"));
    assert!(help.contains("--backend <BACKEND>"));
    assert!(help.contains("--no-shadow"));
    assert!(help.contains("--tiff-alpha <TIFF_ALPHA>"));
    for option in [
        "--preset",
        "--save-preset",
        "--list-presets",
        "--presets-file",
        "--out-dir",
    ] {
        assert!(help.contains(option));
    }
    assert!(!help.contains("Commands:"));
    assert!(!result.stdout.contains(&0x1b));
    let result = run(&["--raw-prefix", "../oops"]);
    assert_eq!(result.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&result.stderr).contains("--raw-prefix"));
    for command in ["capture", "recover", "windows", "monitors"] {
        assert_eq!(run(&[command]).status.code(), Some(2));
    }
}

#[test]
fn wgc_rejects_raw_composites_before_inspecting_the_target() {
    let directory = tempfile::tempdir().unwrap();
    let raw = directory.path().join("raw");
    let result = Command::new(env!("CARGO_BIN_EXE_alphashot"))
        .args(["--backend", "wgc", "--window-hwnd", "1", "--raw-dir"])
        .arg(&raw)
        .output()
        .unwrap();
    assert_eq!(result.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("--raw-dir requires --backend backdrop")
    );
    assert!(result.stdout.is_empty());
    assert!(!raw.exists());
}

#[test]
fn repeat_requires_interactive_input_and_rejects_listing() {
    let directory = tempfile::tempdir().unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_alphashot"))
        .args(["--repeat", "--window-hwnd", "1"])
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert_eq!(result.status.code(), Some(1));
    assert!(result.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&result.stderr)
            .contains("--repeat requires interactive stdin and stderr")
    );
    assert!(
        std::fs::read_dir(directory.path())
            .unwrap()
            .next()
            .is_none()
    );
    assert_eq!(run(&["--repeat", "--list-windows"]).status.code(), Some(2));
}

#[test]
fn clipboard_only_conflicts_fail_before_capture() {
    for args in [
        vec!["--clipboard-only", "--out", "capture.png"],
        vec!["--clipboard-only", "--out-dir", "captures"],
        vec!["--clipboard-only", "--tiff-alpha", "straight"],
        vec!["--clipboard-only", "--tiff-alpha", "premultiplied"],
        vec!["--clipboard-only", "--raw-dir", "raw"],
        vec!["--clipboard-only", "--no-clipboard"],
        vec!["--clipboard-only", "--editor", "paint.exe"],
        vec!["--clipboard-only", "--editor-arg=--wait"],
    ] {
        let result = run(&args);
        assert_eq!(result.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&result.stderr).contains("--clipboard-only"));
        assert!(result.stdout.is_empty());
    }
}

#[test]
fn script_selectors_reject_invalid_waits_and_conflicting_handles() {
    for args in [
        vec!["--wait", "0"],
        vec!["--list-windows", "--wait", "1"],
        vec!["--process", "test.exe", "--wait", "3601"],
        vec!["--window-class", "TestClass", "--wait", "NaN"],
        vec!["--window-title", "Test", "--wait=-1"],
        vec!["--process", " "],
        vec!["--window-class", ""],
        vec!["--window-hwnd", "1", "--process", "test.exe"],
        vec!["--window-hwnd", "1", "--window-class", "TestClass"],
        vec!["--window-hwnd", "1", "--window-title", "Test"],
        vec!["--window-hwnd", "1", "--wait", "0"],
    ] {
        let result = run(&args);
        assert_eq!(result.status.code(), Some(2), "{args:?}");
        assert!(result.stdout.is_empty(), "{args:?}");
        assert!(String::from_utf8_lossy(&result.stderr).contains("error:"));
    }
    let result = run(&["--wait", "0"]);
    let error = String::from_utf8_lossy(&result.stderr);
    assert!(error.contains("--window-title"), "{error}");
    assert!(error.contains("--process"), "{error}");
    assert!(error.contains("--window-class"), "{error}");
}

#[test]
fn invalid_client_sizes_fail_during_argument_parsing() {
    for size in ["0x600", "800x0", "8000x5001", "800x600x2", "800"] {
        let result = run(&["--client-size", size]);
        assert_eq!(result.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&result.stderr).contains("--client-size"));
    }
}

#[test]
fn invalid_backend_and_shadow_values_fail_during_argument_parsing() {
    for args in [
        vec!["--backend", "unknown"],
        vec!["--backend", "WGC"],
        vec!["--no-shadow=yes"],
        vec!["--no-shadow=false", "false"],
        vec!["--tiff-alpha", "associated"],
    ] {
        let result = run(&args);
        assert_eq!(result.status.code(), Some(2));
        assert!(result.stdout.is_empty());
    }
}

#[test]
fn presets_save_list_and_override_defaults_without_capturing() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("settings/presets.ini");
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_alphashot"))
            .arg("--presets-file")
            .arg(&path)
            .args(args)
            .current_dir(directory.path())
            .output()
            .unwrap()
    };
    let result = run(&["--list-presets"]);
    assert!(result.status.success() && result.stdout.is_empty());
    assert!(!path.parent().unwrap().exists());
    let result = run(&[
        "--save-preset",
        "work",
        "--out-dir",
        "captures",
        "--out-format",
        "tiff",
        "--tiff-alpha",
        "premultiplied",
        "--include-cursor",
        "--backend",
        "wgc",
        "--no-shadow",
        "--focus-state",
        "unfocused",
        "--margin",
        "8",
        "--pad",
        "12",
        "--high-dpi",
        "--high-dpi-display",
        "DISPLAY2",
    ]);
    assert!(result.status.success(), "{result:?}");
    assert!(result.stdout.is_empty());
    assert!(path.is_file());
    assert!(!directory.path().join("captures").exists());
    let result = run(&[
        "--preset",
        "work",
        "--save-preset",
        "copy",
        "--include-cursor=false",
        "--backend",
        "backdrop",
        "--no-shadow=false",
        "--tiff-alpha",
        "straight",
        "--high-dpi=false",
        "--margin",
        "0",
        "--out-format",
        "auto",
    ]);
    assert!(result.status.success(), "{result:?}");
    let text = std::fs::read_to_string(&path).unwrap();
    let copy = text
        .split("[copy]\n")
        .nth(1)
        .unwrap()
        .split("[work]")
        .next()
        .unwrap();
    for value in [
        "include-cursor=false",
        "backend=backdrop",
        "no-shadow=false",
        "tiff-alpha=straight",
        "high-dpi=false",
        "high-dpi-display=\n",
        "margin=0",
        "out-format=auto",
    ] {
        assert!(copy.contains(value), "Missing {value} in {copy:?}");
    }
    let result = run(&[
        "--preset",
        "work",
        "--save-preset",
        "display",
        "--high-dpi-display",
        "DISPLAY3",
    ]);
    assert!(result.status.success(), "{result:?}");
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .contains("high-dpi-display=DISPLAY3")
    );
    let result = run(&["--save-preset", "auto", "--tiff-alpha", "premultiplied"]);
    assert!(result.status.success(), "{result:?}");
    let result = run(&["--list-presets"]);
    assert!(result.status.success());
    assert_eq!(result.stdout, b"auto\ncopy\ndisplay\nwork\n");
    let original = std::fs::read(&path).unwrap();
    for args in [
        vec!["--preset", "unknown"],
        vec!["--save-preset", "bad/name"],
        vec![
            "--save-preset",
            "missing-dpi",
            "--high-dpi-display",
            "DISPLAY3",
        ],
    ] {
        let result = run(&args);
        assert!(!result.status.success());
        assert!(result.stdout.is_empty());
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }
    std::fs::write(&path, "[broken]\nunknown=true\n").unwrap();
    let result = run(&["--list-presets"]);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("Unknown preset key unknown"));
}

#[test]
fn default_preset_path_loads_without_creating_directories() {
    let directory = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_alphashot"))
            .env("APPDATA", directory.path())
            .args(args)
            .output()
            .unwrap()
    };
    let result = run(&["--list-presets"]);
    assert!(result.status.success() && result.stdout.is_empty());
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    let result = run(&["--save-preset", "default"]);
    assert!(result.status.success(), "{result:?}");
    assert!(directory.path().join("alphashot/presets.ini").is_file());
}
