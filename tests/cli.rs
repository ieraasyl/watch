use std::{fs, io, process::Command};

fn command(root: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_wvd"));
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
fn list_exits_cleanly_when_stdout_pipe_closes() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("config.toml"), "").unwrap();
    fs::write(dir.path().join("Show - 01.mkv"), []).unwrap();
    let (reader, writer) = io::pipe().unwrap();
    drop(reader);

    let result = command(dir.path())
        .arg("--root")
        .arg(dir.path())
        .arg("list")
        .stdout(writer)
        .output()
        .unwrap();

    assert!(result.status.success(), "{result:?}");
    assert!(result.stderr.is_empty(), "{result:?}");
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

#[test]
fn exports_and_imports_history_between_data_directories() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("config.toml"), "").unwrap();
    let media = dir.path().join("media");
    fs::create_dir(&media).unwrap();
    fs::write(media.join("Show - 01.mkv"), "episode one").unwrap();
    let run = |data: &str, args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_wvd"))
            .arg("--config")
            .arg(dir.path().join("config.toml"))
            .arg("--data-dir")
            .arg(dir.path().join(data))
            .arg("--root")
            .arg(&media)
            .args(args)
            .output()
            .unwrap()
    };
    let history = |data: &str, args: &[&str]| {
        let result = run(data, args);
        assert!(result.status.success(), "{result:?}");
        String::from_utf8(result.stdout).unwrap()
    };
    history("old", &["list"]);
    let episode = fs::canonicalize(media.join("Show - 01.mkv")).unwrap();
    mark_watched(&dir.path().join("old/history.sqlite3"), &episode);

    let export = dir.path().join("backup.sqlite3");
    let exported = history("old", &["history", "export", export.to_str().unwrap()]);
    assert!(
        exported.contains("Exported 1 history entries"),
        "{exported}"
    );
    let again = run("old", &["history", "export", export.to_str().unwrap()]);
    assert!(!again.status.success());
    assert!(String::from_utf8_lossy(&again.stderr).contains("already exists"));

    let imported = history("new", &["history", "import", export.to_str().unwrap()]);
    assert!(
        imported.contains("Imported 1 new and 0 updated"),
        "{imported}"
    );
    assert!(history("new", &["list"]).contains("[Watched]"));
}

/// Mark one video watched directly in SQLite, as the browser would.
fn mark_watched(database: &std::path::Path, path: &std::path::Path) {
    use std::os::unix::ffi::OsStrExt;
    rusqlite::Connection::open(database)
        .unwrap()
        .execute(
            "INSERT INTO progress (path, position, duration, watched, updated)
             VALUES (?1, 0, 0, 1, 1)",
            [path.as_os_str().as_bytes()],
        )
        .unwrap();
}

#[test]
fn export_treats_uri_like_names_as_plain_files() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("config.toml"), "").unwrap();
    let occupied = dir.path().join("occupied.sqlite3");
    fs::write(&occupied, "keep me").unwrap();
    let result = command(dir.path())
        .current_dir(dir.path())
        .args(["history", "export", "file:occupied.sqlite3"])
        .output()
        .unwrap();
    assert!(result.status.success(), "{result:?}");
    assert_eq!(fs::read_to_string(&occupied).unwrap(), "keep me");
    assert!(dir.path().join("file:occupied.sqlite3").is_file());
}

#[test]
fn imports_despite_an_invalid_configuration() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("config.toml"), "").unwrap();
    let export = dir.path().join("export.sqlite3");
    let exported = command(dir.path())
        .args(["history", "export"])
        .arg(&export)
        .output()
        .unwrap();
    assert!(exported.status.success(), "{exported:?}");
    fs::write(dir.path().join("config.toml"), "completion_threshold = 2\n").unwrap();
    let imported = command(dir.path())
        .args(["history", "import"])
        .arg(&export)
        .output()
        .unwrap();
    assert!(imported.status.success(), "{imported:?}");
    assert!(String::from_utf8_lossy(&imported.stdout).contains("Imported 0 new"));
    assert!(String::from_utf8_lossy(&imported.stderr).contains("completion_threshold"));
}

#[test]
fn searches_for_titles_named_like_commands_after_a_separator() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("config.toml"), "").unwrap();
    let result = command(dir.path())
        .arg("--root")
        .arg(dir.path())
        .args(["--", "history"])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("interactive terminal"));
}
