# wvd

A terminal browser for your local movies, anime, and TV shows. Search your
library, play in mpv, and pick up where you left off.

[User guide](#user-guide) · [Development](#development)

## User guide

### Install

With [Homebrew](https://brew.sh), run the following command. On macOS it also
installs mpv for playback. Run `brew upgrade wvd` to update later.

```sh
brew install ieraasyl/tap/wvd
```

On Linux, install mpv with your distribution's package manager.
[mpv installation guide](https://mpv.io/installation/)

#### Install without Homebrew

Install mpv for playback. On macOS, use a build from the
[mpv installation guide](https://mpv.io/installation/). If you installed the
`mpv.app` bundle, set `player = "/Applications/mpv.app/Contents/MacOS/mpv"` in
your [configuration](#configuration), because the app does not put `mpv` on
your `PATH`. On Linux, use your distribution's package manager.

Download the archive for your computer. If you're unsure which CPU you have,
run `uname -m` in Terminal. Linux usually reports `aarch64` for ARM64.

| Computer | Download |
| --- | --- |
| macOS, Apple Silicon | [wvd-macos-arm64.tar.gz](https://github.com/ieraasyl/wvd/releases/latest/download/wvd-macos-arm64.tar.gz) |
| macOS, Intel | [wvd-macos-x86_64.tar.gz](https://github.com/ieraasyl/wvd/releases/latest/download/wvd-macos-x86_64.tar.gz) |
| Linux, ARM64 | [wvd-linux-arm64.tar.gz](https://github.com/ieraasyl/wvd/releases/latest/download/wvd-linux-arm64.tar.gz) |
| Linux, x86-64 | [wvd-linux-x86_64.tar.gz](https://github.com/ieraasyl/wvd/releases/latest/download/wvd-linux-x86_64.tar.gz) |

Extract the archive and open Terminal in the extracted folder. For example,
on Apple Silicon the folder is `~/Downloads/wvd-macos-arm64` if you
extracted it in Downloads. Run the following commands to install and start
the browser.

```sh
mkdir -p "$HOME/.local/bin"
install -m 755 wvd "$HOME/.local/bin/wvd"
"$HOME/.local/bin/wvd"
```

Add `~/.local/bin` to your `PATH` to run `wvd` from any directory.

If macOS asks you to approve the downloaded binary, follow the steps for
opening software from an unidentified developer. [Apple Support](https://support.apple.com/en-us/102445)

### Start watching

Run `wvd` to browse `~/Movies`. If your files live elsewhere, pass
`--root` with your media folder. Press Enter on a title to move to its
episodes or movies, then Enter again to play. Close the mpv window or press
`q` in it to return to the browser. Use mpv's usual controls for seeking,
subtitles, and audio tracks.

```sh
wvd --root /Volumes/Media    # Override library roots for this invocation
wvd "ao no hako"             # Open with a fuzzy title search
wvd resume                  # Pick up where you left off
wvd list                    # Print the library and watched status
wvd config                  # Show configuration and history file locations
wvd history export FILE     # Save a copy of your watch history
wvd history import FILE     # Merge history saved on this or another computer
```

The browser needs an interactive terminal. Use `wvd list` in scripts or pipes.
To search for a title named like a command, such as `history`, put `--` before
it: `wvd -- history`.

### Keyboard shortcuts

| Key | Action |
| --- | --- |
| Arrow keys or `h`/`j`/`k`/`l` | Move or change pane |
| Tab | Switch pane |
| Enter | Select a title or play the selected video |
| `/` | Edit fuzzy title search. Press Enter or Esc to leave search mode |
| `e` | Switch between episodes and specials/extras |
| `n` / `N` | Jump to the next or previous unwatched video in the episode pane |
| `w` | Toggle watched status and reset saved progress |
| `W` | Mark regular episodes in the selected season through this episode as watched |
| `r` | Play the selected video from the start |
| `R` | Rescan the media folders and keep the current search and selection if still visible |
| `c` | Clear search and select Continue watching, if available |
| Home/End or `g`/`G` | Jump to the first or last item |
| `q`, Esc, or Ctrl+C | Exit the browser |

From the library pane, `n` focuses the first unwatched video for the highlighted
title in the current episodes or extras view. In the episode pane, jumps stay
within the visible list and stop at its ends. Partially watched videos count as
unwatched. Press Enter to play the highlighted video.

`w`, `W`, and `r` act in the episode pane. `W` skips extras and works within the
selected season, or the unnumbered episode list for a series. In search mode,
letters including `q` are search text.

### Resume and watched status

**Continue watching** appears at the top of the unfiltered library when there
is something to resume or play next. For each series you started, it shows a
partly watched regular episode or the next unwatched one after the episode you
watched most recently. Unfinished movies and extras are also listed. Partly
watched videos appear first, and `wvd resume` plays the first item in this list.

Progress is saved every five seconds and when playback ends. Ctrl+C during
playback saves the last reported position and exits both the player and CLI.
A video becomes watched when it finishes naturally or reaches the completion
threshold, 95% by default. Replaying a watched video starts from the beginning
and preserves its watched status. After an episode finishes naturally, the
browser offers the next unwatched regular episode and waits for confirmation.
The prompt and Continue watching can advance to a later numbered season of
the same series. Unnumbered episodes continue within their own list.

### Back up or move your history

Run `wvd history export ~/wvd-history.sqlite3` to save a copy of your watch
history. The file must not exist yet. It is safe to export while another `wvd`
is playing.

To restore it, or to carry it to another computer, run
`wvd history import ~/wvd-history.sqlite3`. Importing merges with the history
already there. When both have the same video, the entry updated most recently
wins, so importing the same file twice changes nothing. Videos stored at
different paths on the new computer get their history back when `wvd` finds
them, as long as the files themselves are the same. An entry whose video is
not at its old path waits until `wvd` finds that video, even on a later run.
History saved by older versions of `wvd` has no fingerprint, so it applies only
to videos at the same path.

### Configuration

Configuration is optional. To save your media folders or change other
defaults, run `wvd config` and create a file at the printed config path.
Create the parent directory first if needed. You can start with the example
below or copy [config.example.toml](config.example.toml).

```toml
media_dirs = ["~/Movies", "/Volumes/Media"]
player = "mpv"
completion_threshold = 0.95

[titles]
"Kuroko's Basketball" = "Kuroko no Basket"
```

`player` is an executable name or path to an mpv-compatible executable, without
shell arguments. Title overrides change display/grouping only. For a title such
as `Kuroko's Basketball · Season 1`, use `Kuroko's Basketball` as the override key.

Use `--config <file>` for another configuration, `--data-dir <directory>` for
separate history, or repeat `--root <directory>` to override media locations.
Paths beginning with `~/` expand in the config; quote paths containing spaces
when supplying command-line flags.

On macOS the default configuration and SQLite history live under
`~/Library/Application Support/wvd/`. On Linux they use the standard XDG
configuration and data directories. `wvd config` prints exact paths.

### How files are organized

`wvd` rescans your media folders each time it starts and whenever you press
`R` in the browser. It uses filenames and folders to infer titles, seasons,
and episodes without looking up online metadata.

- Recognizes common video extensions, including MKV, MP4, MOV, AVI, and WebM.
- Handles release names such as `Show.S02E10`, `Show 2x10`, and anime
  `Show - 03v2`, including season information in collection folder names.
- Sorts episode numbers numerically. Decimal episodes such as `22.5`, OVAs,
  bloopers, and creditless openings/endings go in the extras view.
- Ignores hidden entries and Apple's `.tvlibrary` bundles. Directory symlinks
  are not followed; overlapping library roots are deduplicated.
- Reports unreadable or missing roots while keeping readable media available.

Ambiguous names can remain separate titles. Use `[titles]` in the config to
correct grouping.

History is stored per file path. If you move or rename a video, `wvd`
recognizes it by its size and a fingerprint of its contents on the next scan
and restores its progress. The entry for the old path is kept, so a video on
a disconnected drive still has its history when the drive returns. History
saved by older versions of `wvd` gets a fingerprint the next time `wvd` finds
the file, so a video moved before then starts fresh.

## Development

### Install with Cargo

With Rust 1.100.0 or later, you can build the published crate from source.
[Rust and Cargo installation](https://rust-lang.org/tools/install/) · [wvd on crates.io](https://crates.io/crates/wvd)

```sh
cargo install wvd --locked
```

Cargo installs `wvd` in `~/.cargo/bin` by default.

### Run from source

Clone the repository and run these commands from its root. The
[toolchain file](rust-toolchain.toml) pins Rust 1.100.0 for rustup users.

```sh
cargo run --locked -- list
cargo run --locked -- "ao no hako"
```

To install your checkout locally, run the following command and make sure
`~/.local/bin` is on your `PATH`.

```sh
cargo install --path . --locked --root "$HOME/.local"
```

### Checks

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo test --locked real_mpv -- --ignored  # Optional playback test, requires mpv
```

The mpv test generates a silent audio fixture and uses null audio/video outputs
to check quitting, saved progress, resuming, completion, rewatches, and playback
errors without opening a window or touching your library. [CI](.github/workflows/ci.yml)
runs the checks and the mpv test on macOS and Linux.
The [dependency audit](.github/workflows/audit.yml) runs weekly and when a pull
request changes the manifest, lockfile, toolchain pin, or audit workflow.

### Build release downloads

1. On an up-to-date `main`, push a tag such as `v0.2.0` that matches the
   version in `Cargo.toml`, and create a draft GitHub release for that tag.
   Only repository admins can create `v*` tags, and nobody can move or delete
   them.
2. Run **Release binaries** from `main` in GitHub Actions and enter the tag.
   The workflow builds and tests each archive from that tagged source.
3. Wait until the latest **Release binaries** run for the tag has succeeded,
   then check that the draft contains the four archives and `SHA256SUMS`, and
   publish it. A rerun replaces the files one at a time, so publishing during a
   run can pair new checksums with an old archive. Releases are immutable once
   published, so a fix needs a new version.
   Publishing runs **Homebrew formula**, which tests the formula on all four
   platforms and pushes it to [ieraasyl/homebrew-tap](https://github.com/ieraasyl/homebrew-tap).
   To retry, run it from `main` and enter the tag. Only `main` and release tags
   can use the `release` environment that holds the tap's deploy key. The
   workflow publishes only immutable releases and refuses to replace a newer
   formula with an older version.

## License

[MIT](LICENSE)
