# watch

A Rust terminal browser for your local movies, anime, and TV shows. Search your
library, play in mpv, and pick up where you left off.

## Install

Requires macOS or Linux, a recent stable Rust toolchain, and
[mpv](https://mpv.io/installation/). On macOS:

```sh
brew install rust mpv
cargo install --path . --locked --root "$HOME/.local"
```

Make sure `~/.local/bin` is on your `PATH`. If you already use the Unix `watch`
utility, invoke this binary by its full path or give it a different local name.

## Use

```sh
watch                         # Browse ~/Movies
watch kuroko                  # Open with a fuzzy title search
watch "ao no hako"             # Quote searches containing spaces
watch resume                  # Resume the latest unfinished video
watch list                    # Print titles, episodes, extras, and watched status
watch config                  # Show configuration and history file locations
watch --root /Volumes/Media    # Override library roots for this invocation
```

Press Enter on a title to move to its episodes, then Enter again to play.
Playback opens in an mpv window. Close that window or press `q` there to return
to the browser. mpv handles subtitles, audio tracks, seeking, and its usual keys.

| Key | Action |
| --- | --- |
| Arrow keys or `h`/`j`/`k`/`l` | Move or change pane |
| Tab | Switch pane |
| Enter | Select a title or play the selected video |
| `/` | Edit fuzzy title search; Enter or Esc leaves search mode |
| `e` | Switch between episodes and specials/extras |
| `n` / `N` | Jump to the next or previous unwatched video in the episode pane |
| `w` | Toggle the selected video as watched; clearing it resets progress |
| `r` | Play the selected video from the start |
| `c` | Clear search and select Continue watching, if available |
| Home/End or `g`/`G` | Jump to the first or last item |
| `q`, Esc, or Ctrl+C | Exit the browser |

From the library pane, `n` focuses the first unwatched video for the highlighted
title in the current episodes or extras view. In the episode pane, jumps stay
within the visible list and stop at its ends. Partially watched videos count as
unwatched. Press Enter to play the highlighted video.

`w` and `r` act in the episode pane. In search mode, letters including `q` are
search text. Ctrl+C during playback saves the last reported position and exits
both the player and CLI.

Continue watching appears first when there are unfinished videos. Progress is
saved every five seconds and when playback ends. A video becomes watched at
natural completion or at 95% of its duration, whichever comes first. Replaying
a watched video starts from the beginning. After natural completion, the browser
offers the next unwatched regular episode in the same season; it does not
automatically start playback. Specials are excluded from that sequence.

`watch resume` picks the most recently updated unfinished video still present
in the library. If none exists, it plays the next unwatched episode
after the most recently updated entry. The library is rescanned on each launch.

## Configuration

Defaults work without creating a configuration file. To customize them, run
`watch config` and copy `config.example.toml` to the printed config path.
Create its parent directory first if necessary.

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
`~/Library/Application Support/watch-cli/`. On Linux they use the standard XDG
configuration and data directories. `watch config` prints exact paths.

## Library behavior

- Recognizes common video extensions, including MKV, MP4, MOV, AVI, and WebM.
- Handles release names such as `Show.S02E10`, `Show 2x10`, and anime
  `Show - 03v2`, including season information in collection folder names.
- Sorts episode numbers numerically. Decimal episodes such as `22.5`, OVAs,
  bloopers, and creditless openings/endings go in the extras view.
- Ignores hidden entries and Apple's `.tvlibrary` bundles. Directory symlinks
  are not followed; overlapping library roots are deduplicated.
- Reports unreadable or missing roots while keeping readable media available.

Filename interpretation is heuristic. Ambiguous names can remain separate
titles; use title overrides to correct grouping. There are no network metadata
lookups. History is keyed by the canonical file path, so moving or renaming a
file gives it a fresh history entry. The application does not rename media.

The interactive browser needs a terminal; use `watch list` in scripts or pipes.

## Development

```sh
cargo run -- kuroko
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo test real_mpv -- --ignored  # Optional real playback test; requires mpv
```

The mpv test generates a silent audio fixture and uses null audio/video outputs
to check quitting, saved progress, resuming, completion, and playback errors
without opening a window or touching your library. CI runs checks on macOS and
Linux, plus the mpv integration test on Linux.

MIT licensed.
