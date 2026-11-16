use std::{fs, process::Command};

fn command(root: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_watch"));
    command
        .arg("--config")
        .arg(root.join("config.toml"))
        .arg("--data-dir")
        .arg(root.join("history"));
    command
}

#[test]
fn lists_configured_library_with_title_overrides_and_natural_order() {
    let dir = tempfile::tempdir().unwrap();
    let media = dir.path().join("Media's folder");
    fs::create_dir(&media).unwrap();
    for name in ["Show - 10.mkv", "Show - 02.mkv", "notes.txt", ".hidden.mp4"] {
        fs::write(media.join(name), []).unwrap();
    }
    fs::write(
        dir.path().join("config.toml"),
        format!(
            "media_dirs = [{}]\n[titles]\nShow = 'Pretty name'\n",
            toml::Value::String(media.to_string_lossy().into_owned())
        ),
    )
    .unwrap();
    let result = command(dir.path()).arg("list").output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let output = String::from_utf8(result.stdout).unwrap();
    assert!(output.contains("Pretty name"));
    assert!(output.find("Episode 02").unwrap() < output.find("Episode 10").unwrap());
    assert!(output.contains("2 media files"));
    let browser = command(dir.path()).output().unwrap();
    assert!(!browser.status.success());
    assert!(String::from_utf8_lossy(&browser.stderr).contains("interactive terminal"));
    assert!(!browser.stdout.contains(&0x1b));
}

#[test]
fn rejects_invalid_and_missing_explicit_configuration() {
    let dir = tempfile::tempdir().unwrap();
    let missing = command(dir.path()).arg("list").output().unwrap();
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("config.toml"));
    fs::write(dir.path().join("config.toml"), "completion_threshold = 2\n").unwrap();
    let invalid = command(dir.path()).arg("list").output().unwrap();
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("completion_threshold"));
    fs::write(dir.path().join("config.toml"), "media_dir = ['/typo']\n").unwrap();
    let typo = command(dir.path()).arg("list").output().unwrap();
    assert!(!typo.status.success());
    assert!(String::from_utf8_lossy(&typo.stderr).contains("unknown field"));
}
