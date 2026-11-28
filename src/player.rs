use crate::{
    config::Config,
    db::{self, Progress, Store},
    library::Media,
};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    fs::{File, Permissions},
    io::{Read, Write},
    mem::DropGuard,
    os::unix::{fs::PermissionsExt, net::UnixStream},
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

pub static INTERRUPTED: AtomicBool = AtomicBool::new(false);

pub fn play(media: &Media, previous: &Progress, config: &Config, store: &Store) -> Result<bool> {
    INTERRUPTED.store(false, Ordering::Relaxed);
    let temporary = tempfile::Builder::new()
        .prefix("wvd-")
        .permissions(Permissions::from_mode(0o700))
        .tempdir_in("/tmp")?;
    let socket_path = temporary.path().join("mpv.sock");
    let log_path = temporary.path().join("mpv.log");
    let log = File::create(&log_path)?;
    let start = if previous.watched {
        0.0
    } else {
        previous.position
    };
    let child = Command::new(&config.player)
        .arg(format!("--input-ipc-server={}", socket_path.display()))
        .arg(format!("--start={start}"))
        .args(["--idle=no", "--keep-open=no", "--force-window=yes"])
        .arg("--")
        .arg(&media.path)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .spawn()
        .with_context(|| {
            format!(
                "Could not start {}. Install mpv or set player in your config",
                config.player.display()
            )
        })?;
    // Always reap the player, including on IPC or database failures.
    let mut player = DropGuard::new(child, |mut child| {
        let _ = child.kill();
        let _ = child.wait();
    });

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut socket = loop {
        match UnixStream::connect(&socket_path) {
            Ok(socket) => break socket,
            Err(_) if Instant::now() < deadline => {
                if let Some(status) = player.try_wait()? {
                    bail!(
                        "mpv exited before playback ({status}): {}",
                        read_log(&log_path)
                    );
                }
                if INTERRUPTED.load(Ordering::Relaxed) {
                    return Ok(false);
                }
                thread::sleep(Duration::from_millis(30));
            }
            Err(error) => return Err(error).context("Timed out connecting to mpv IPC"),
        }
    };
    socket.set_read_timeout(Some(Duration::from_millis(200)))?;
    socket.set_write_timeout(Some(Duration::from_secs(2)))?;
    for (id, property) in [(1, "time-pos"), (2, "duration")] {
        writeln!(
            socket,
            "{}",
            json!({"command": ["observe_property", id, property]})
        )?;
    }
    let mut progress = Progress {
        position: start,
        duration: previous.duration,
        watched: false,
        updated: db::now(),
    };
    let mut received_position = false;
    let mut eof = false;
    let mut playback_error = None;
    let mut pending = Vec::new();
    let mut buffer = [0; 8192];
    let mut checkpoint = Instant::now();
    loop {
        if INTERRUPTED.load(Ordering::Relaxed) {
            let _ = writeln!(socket, "{}", json!({"command": ["quit"]}));
            break;
        }
        match socket.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                pending.extend_from_slice(&buffer[..count]);
                while let Some(end) = pending.iter().position(|byte| *byte == b'\n') {
                    let line: Vec<_> = pending.drain(..=end).collect();
                    if let Ok(event) = serde_json::from_slice::<Value>(&line) {
                        apply_event(
                            &event,
                            &mut progress,
                            &mut received_position,
                            &mut eof,
                            &mut playback_error,
                        );
                    }
                }
                if pending.len() > 1024 * 1024 {
                    playback_error = Some("mpv sent an oversized IPC message".to_owned());
                    break;
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) => {}
            Err(e) => {
                playback_error = Some(format!("mpv IPC connection failed: {e}"));
                break;
            }
        }
        if received_position && checkpoint.elapsed() >= Duration::from_secs(5) {
            save(
                store,
                media,
                &mut progress,
                config.completion_threshold,
                eof,
            )?;
            checkpoint = Instant::now();
        }
        // Drain remaining IPC events until the socket closes, even after the process exits.
        if eof || playback_error.is_some() {
            break;
        }
    }
    if received_position || eof {
        save(
            store,
            media,
            &mut progress,
            config.completion_threshold,
            eof,
        )?;
    }
    if let Some(error) = playback_error {
        bail!("Playback failed: {error}\n{}", read_log(&log_path));
    }
    if !eof
        && !INTERRUPTED.load(Ordering::Relaxed)
        && let Some(status) = player.try_wait()?
        && !status.success()
    {
        bail!("mpv exited with {status}: {}", read_log(&log_path));
    }
    Ok(eof)
}

fn save(
    store: &Store,
    media: &Media,
    progress: &mut Progress,
    threshold: f64,
    eof: bool,
) -> Result<()> {
    progress.updated = db::now();
    progress.watched =
        eof || (progress.duration > 0.0 && progress.position / progress.duration >= threshold);
    store.save_playback(&media.path, progress)
}

fn apply_event(
    event: &Value,
    progress: &mut Progress,
    received: &mut bool,
    eof: &mut bool,
    error: &mut Option<String>,
) {
    match event["event"].as_str() {
        Some("property-change") => {
            if let Some(value) = event["data"]
                .as_f64()
                .filter(|v| v.is_finite() && *v >= 0.0)
            {
                match event["name"].as_str() {
                    Some("time-pos") => {
                        progress.position = value;
                        *received = true;
                    }
                    Some("duration") => progress.duration = value,
                    _ => {}
                }
            }
        }
        Some("end-file") => match event["reason"].as_str() {
            Some("eof") => *eof = true,
            Some("error") => {
                *error = Some(
                    event["file_error"]
                        .as_str()
                        .unwrap_or("Could not decode this file")
                        .to_owned(),
                )
            }
            _ => {}
        },
        _ => {}
    }
}

fn read_log(path: &std::path::Path) -> String {
    let log = std::fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<_> = log.lines().rev().take(8).collect();
    lines.into_iter().rev().collect::<Vec<_>>().join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires mpv on PATH; plays a generated audio fixture with null outputs"]
    fn real_mpv_saves_on_quit_then_resumes_to_completion() {
        let dir = tempfile::tempdir().unwrap();
        let audio = dir.path().join("episode.wav");
        let samples = 48_000_u32 * 5;
        let size = samples * 2;
        let mut wave = Vec::new();
        wave.extend_from_slice(b"RIFF");
        wave.extend_from_slice(&(36 + size).to_le_bytes());
        wave.extend_from_slice(b"WAVEfmt ");
        wave.extend_from_slice(&16_u32.to_le_bytes());
        wave.extend_from_slice(&1_u16.to_le_bytes());
        wave.extend_from_slice(&1_u16.to_le_bytes());
        wave.extend_from_slice(&48_000_u32.to_le_bytes());
        wave.extend_from_slice(&96_000_u32.to_le_bytes());
        wave.extend_from_slice(&2_u16.to_le_bytes());
        wave.extend_from_slice(&16_u16.to_le_bytes());
        wave.extend_from_slice(b"data");
        wave.extend_from_slice(&size.to_le_bytes());
        wave.resize(44 + size as usize, 0);
        std::fs::write(&audio, wave).unwrap();
        let wrapper = dir.path().join("headless-mpv");
        std::fs::write(
            dir.path().join("quit.lua"),
            // mpv starts paused, so the position read after the initial seek cannot run ahead.
            // Playback resumes once it is recorded, then quits after advancing a full second,
            // which leaves wvd time to subscribe over IPC and record a checkpoint.
            "local start\n\
             mp.register_event('playback-restart', function()\n\
                 if start then return end\n\
                 start = mp.get_property_number('time-pos', 0)\n\
                 mp.set_property_bool('pause', false)\n\
             end)\n\
             mp.observe_property('time-pos', 'number', function(_, pos)\n\
                 if start and pos and pos > start + 1 then mp.commandv('quit') end\n\
             end)\n",
        )
        .unwrap();
        let quit_early = "#!/bin/sh\nexec mpv --no-config --vo=null --ao=null --pause --script=\"${0%/*}/quit.lua\" \"$@\"\n";
        std::fs::write(&wrapper, quit_early).unwrap();
        std::fs::set_permissions(&wrapper, Permissions::from_mode(0o700)).unwrap();
        let media = crate::library::parse(audio, None);
        let config = Config {
            player: wrapper.clone(),
            ..Config::default()
        };
        let store = Store::open(&dir.path().join("history.sqlite3")).unwrap();
        // Far enough in that a launch from zero would quit before passing it.
        let previous = Progress {
            position: 1.0,
            ..Progress::default()
        };
        assert!(!play(&media, &previous, &config, &store).unwrap());
        let partial = store.all().unwrap()[&media.path].clone();
        assert!(!partial.watched);
        assert!(partial.position > previous.position + 0.5, "{partial:?}");
        // Resume from the saved checkpoint and advance about a second past it.
        let checkpoint = partial;
        assert!(!play(&media, &checkpoint, &config, &store).unwrap());
        let partial = store.all().unwrap()[&media.path].clone();
        assert!(!partial.watched);
        assert!(
            partial.position > checkpoint.position + 0.5,
            "{checkpoint:?} -> {partial:?}"
        );
        let play_through = "#!/bin/sh\nexec mpv --no-config --vo=null --ao=null \"$@\"\n";
        std::fs::write(&wrapper, play_through).unwrap();
        assert!(play(&media, &partial, &config, &store).unwrap());
        let history = store.all().unwrap();
        let saved = &history[&media.path];
        assert!(saved.watched);
        assert!(saved.position >= previous.position);
        assert!(saved.duration > 0.9);

        // Replays start from zero, save their new position, and keep the watched status.
        std::fs::write(&wrapper, quit_early).unwrap();
        let replayed_early = |previous: &Progress| {
            let before = store.all().unwrap()[&media.path].position;
            assert!(!play(&media, previous, &config, &store).unwrap());
            let replay = store.all().unwrap()[&media.path].clone();
            assert!(replay.watched);
            assert!(replay.position < before, "{replay:?}");
        };
        replayed_early(saved);
        store
            .save_playback(
                &media.path,
                &Progress {
                    position: 2.5,
                    ..saved.clone()
                },
            )
            .unwrap();
        replayed_early(&Progress::default());

        std::fs::write(&wrapper, play_through).unwrap();
        let missing = Media {
            path: dir.path().join("missing.mkv"),
            ..media
        };
        assert!(play(&missing, &Progress::default(), &config, &store).is_err());
        assert_eq!(store.all().unwrap().len(), 1);
    }

    #[test]
    fn distinguishes_quitting_from_finishing_and_handles_invalid_positions() {
        let mut progress = Progress::default();
        let (mut received, mut eof, mut error) = (false, false, None);
        for event in [
            json!({"event":"property-change", "name":"duration", "data":120}),
            json!({"event":"property-change", "name":"time-pos", "data":45.5}),
            json!({"event":"property-change", "name":"time-pos", "data":null}),
            json!({"event":"property-change", "name":"time-pos", "data":-1}),
            json!({"event":"end-file", "reason":"quit"}),
        ] {
            apply_event(&event, &mut progress, &mut received, &mut eof, &mut error);
        }
        assert!(received);
        assert_eq!(progress.position, 45.5);
        assert_eq!(progress.duration, 120.0);
        assert!(!eof);
        assert!(error.is_none());
        apply_event(
            &json!({"event":"end-file", "reason":"eof"}),
            &mut progress,
            &mut received,
            &mut eof,
            &mut error,
        );
        assert!(eof);
    }
}
