# Termusic

A fast, cross-platform command-line music player with a background playback daemon, `yt-dlp` search and download integration, and an interactive terminal UI (TUI).

## Prerequisites

- **Rust toolchain** (edition 2024) to build from source (`cargo build --release`).
- **`yt-dlp`** in `PATH` for `termusic search` and `termusic download`.
- **`ffmpeg` / `ffprobe`** in `PATH` for audio extraction, format conversion, metadata tagging, and thumbnail embedding.

## Quick Start

Initialize a Termusic working directory before running commands:

```bash
termusic init ~/Music/termusic-workspace
```

Then launch the interactive TUI or use CLI subcommands:

```bash
termusic                  # Launch interactive TUI (same as `termusic tui`)
termusic search "lofi"    # Search YouTube interactively
```

## Core Commands

| Command | Description |
| --- | --- |
| `termusic init <path>` | Initialize a workspace for config, library index, playlists, and downloads |
| `termusic tui` (or `termusic`) | Launch the interactive player TUI |
| `termusic search [OPTIONS] [QUERY...]` | Search supported providers via `yt-dlp`, copy URLs, or download tracks/videos |
| `termusic download [OPTIONS] <URL...>` | Download media via `yt-dlp` and auto-index audio tracks into the library |
| `termusic add [-f] [-p] <path> [playlist]` | Add an audio file or directory (`-f`) to the library (and optionally a playlist `-p`) |
| `termusic list [songs\|playlists]` | List indexed songs and/or playlists |
| `termusic play [target] [mode]` | Play a song or playlist (`--song`, `--playlist`, `--artist <NAME>`), or resume when omitted |
| `termusic pause` / `resume` / `stop` | Control daemon playback |
| `termusic next` / `prev` | Skip to next or previous track |
| `termusic rewind <secs>` / `advance <secs>` | Seek backward or forward |
| `termusic create <name> songs=<s1,s2,...>` | Create a playlist from existing library songs |
| `termusic delete <target>` | Remove a song or playlist from the library index (leaves files on disk untouched) |
| `termusic status` | Show current daemon playback status |
| `termusic daemon stop` | Gracefully stop the background playback daemon |
| `termusic version` | Print version information |

## `termusic search`

Search curated providers via `yt-dlp`, browse results in a TUI (or print to stdout with `--no-tui`), copy track URLs to the clipboard, or download results directly with `-d` / `--download`.

### Options

| Flag | Description |
| --- | --- |
| `-n`, `--limit <N>` | Maximum search results to fetch (`1..=100`) |
| `--set-limit <N>` | Persist default result limit (`1..=100`) to workspace `config.json` |
| `-p`, `--provider <URL>` | Search provider for this run (name, prefix, or URL) |
| `-pd`, `--provider-default <URL>` | Persist default search provider to workspace `config.json` |
| `-d`, `--download` | Enable download mode (`Enter` downloads in TUI; `--no-tui` downloads result `#1`) |
| `--dest <DIR>` | Override download destination directory when `-d` is enabled |
| `-f`, `--format <FORMAT>` | Download format for this run when `-d` is enabled |
| `-fd`, `-format-default`, `--format-default <FORMAT>` | Persist default download format for `search -d` to workspace `config.json` |
| `--no-tui` | Print search results to stdout (and download `#1` when `-d` is set) |

Note: You can run `termusic search` with `--set-limit`, `-pd`/`--provider-default`, or `-fd`/`--format-default` without a query to save defaults directly.

### Interactive Search TUI Controls

- `↑`/`↓` or `j`/`k` (plus `PgUp`/`PgDn`/`g`/`G`): navigate results
- `/`: start or edit search query
- `Enter`: copy selected URL (browse mode) or download selected item (`-d` / `--download` mode)
- `y`: copy selected URL (works in both browse and download modes)
- `q` / `Esc`: quit

### Examples

```bash
# Search YouTube interactively (defaults: youtube, limit 10)
termusic search "ambient synth"

# Search SoundCloud using website URL or short alias, limiting to 15 results
termusic search -p https://soundcloud.com -n 15 "chill beats"
termusic search -p sc "chill beats"

# Save default provider and default search download format without running a search
termusic search -pd soundcloud -fd .mp4

# Search and download interactively (Press Enter on a result to download, y to copy URL)
termusic search -d -f .mp3 --dest ~/Music/Downloads "jazz piano"

# Download top MP4 video result non-interactively
termusic search --no-tui -d -f mp4 --dest ~/Videos "live session"
```

### Configuration & Precedence

Settings resolve in the following order (highest priority first):

- **Search limit**: `-n`/`--limit` -> `--set-limit` -> `TERMUSIC_SEARCH_LIMIT` -> workspace `config.json` -> `10`
- **Search provider**: `-p`/`--provider` -> `-pd`/`--provider-default` -> `TERMUSIC_SEARCH_PROVIDER` -> workspace `config.json` -> `youtube`
- **Search download format**: `-f`/`--format` -> `-fd`/`-format-default`/`--format-default` -> `TERMUSIC_SEARCH_FORMAT` -> workspace `config.json` -> `mp3`
- **Download destination** (`search -d` and `download`): `--dest` -> saved destination (`download --set-dest`) -> `TERMUSIC_DEST` -> workspace `config.json`

## Download Formats

Supported formats for `termusic search -d` (`-f` / `-fd` / `TERMUSIC_SEARCH_FORMAT`) and `termusic download -f`:

| Format | Type | Notes |
| --- | --- | --- |
| `mp3` | Audio | Default format; auto-indexed into the music library when `download` auto-add is active |
| `mp4` | Video + Audio | Downloaded via `yt-dlp -t mp4` with metadata & thumbnail embedded; skipped during music library indexing |
| `flac` | Audio | Lossless audio; auto-indexed |
| `m4a` | Audio | AAC in MP4 container; auto-indexed |
| `ogg` | Audio | Vorbis (`--audio-format vorbis`); auto-indexed |
| `wav` | Audio | Uncompressed PCM audio; auto-indexed |
| `aac` | Audio | Raw AAC audio; auto-indexed |

Format arguments are case-insensitive and accept an optional leading dot (for example, `.mp3`, `MP4`, `.flac`).

## Supported Search Providers

**2 websites** are supported by `termusic search -p` / `-pd`:

| Provider | `yt-dlp` Prefix | Accepted `-p` / `-pd` / `TERMUSIC_SEARCH_PROVIDER` Values |
| --- | --- | --- |
| **YouTube** | `ytsearch` | `youtube`, `yt`, `ytsearch`, `youtube.com`, `www.youtube.com`, `m.youtube.com`, `youtu.be` (including `http://` / `https://` URLs) |
| **SoundCloud** | `scsearch` | `soundcloud`, `sc`, `scsearch`, `soundcloud.com`, `www.soundcloud.com`, `m.soundcloud.com` (including `http://` / `https://` URLs) |

> **Why only 2 search providers?**  
> Although `yt-dlp` (`2026.03.17`) ships 1,872 total extractors ([`supportedsites.md`](https://github.com/yt-dlp/yt-dlp/blob/master/supportedsites.md)), only 20 are `*search*` keyword-search extractors, and most of the others are niche video/archive search engines or defunct/broken without site-specific auth/API keys. Termusic curates **YouTube** (`ytsearch`) and **SoundCloud** (`scsearch`) because both reliably support fast metadata keyword search via `yt-dlp --flat-playlist --dump-single-json`.
>
> To download directly from any other website supported by `yt-dlp`, pass the media URL directly to `termusic download <URL>` instead.
