# Termusic

A fast, cross-platform command-line music player with a background playback daemon, `yt-dlp` search and download integration, and an interactive terminal UI (TUI).

## Prerequisites

- **Rust toolchain** (edition 2024) if building from source (`cargo install` / `cargo build`).
- **`yt-dlp`** in `PATH` (or `~/.local/bin`) for `termusic search` and `termusic download`.
- **`ffmpeg` / `ffprobe`** in `PATH` for audio extraction, format conversion, metadata tagging, and thumbnail embedding.

## Installation

### Option 1: Install from Source (Cargo)

Clone the repository and install `termusic` into `~/.cargo/bin` (ensure `~/.cargo/bin` is on your `PATH`):

```bash
git clone https://github.com/patrickcortez/Termusic.git
cd Termusic
cargo install --path . --locked
```

Alternatively, install directly from Git without cloning first:

```bash
cargo install --git https://github.com/patrickcortez/Termusic.git --locked
```

If you have [`just`](https://github.com/casey/just) or `make` installed, you can also manage source builds with the repository recipes:

```bash
just install    # or: make install
just update     # rebuild and overwrite existing install (`--force`)
just uninstall  # uninstall `termusic`
```

To build locally without installing globally:

```bash
cargo build --release --locked
# Binary output: target/release/termusic (target/release/termusic.exe on Windows)
```

### Option 2: Prebuilt Release Binaries

Download the archive for your platform from [GitHub Releases](https://github.com/patrickcortez/Termusic/releases):

- **Linux (`x86_64`)**: `termusic-<version>-x86_64-unknown-linux-gnu.tar.gz`
- **Windows (`x86_64`)**: `termusic-<version>-x86_64-pc-windows-msvc.zip`
- **macOS (Apple Silicon)**: `termusic-<version>-aarch64-apple-darwin.tar.gz`
- **macOS (Intel)**: `termusic-<version>-x86_64-apple-darwin.tar.gz`
- **Android (`aarch64` / Termux)**: `termusic-<version>-aarch64-linux-android.tar.gz`

Extract the binary and place it in a directory on your `PATH`:

```bash
# Linux / macOS / Termux
tar -xzf termusic-<version>-<target>.tar.gz
install -m 755 termusic ~/.local/bin/termusic
```

```powershell
# Windows (PowerShell)
Expand-Archive termusic-<version>-x86_64-pc-windows-msvc.zip -DestinationPath "$env:LOCALAPPDATA\Programs\termusic"
# Add "$env:LOCALAPPDATA\Programs\termusic" to your User PATH
```

Verify checksums with `SHA256SUMS` from the release assets (`sha256sum -c SHA256SUMS --ignore-missing`).

### Installing `yt-dlp` and `ffmpeg`

```bash
# macOS (Homebrew)
brew install yt-dlp ffmpeg

# Windows (WinGet)
winget install yt-dlp.yt-dlp Gyan.FFmpeg

# Android (Termux)
pkg install rust yt-dlp ffmpeg pulseaudio
```

On Linux, install `yt-dlp` and `ffmpeg` via your distribution package manager (for example, `apt`, `dnf`, or `pacman`) or `~/.local/bin`, and verify with `termusic --version` and `yt-dlp --version`.

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
| `termusic list [songs\|playlists]` | List indexed songs and playlists with 1-based numbers |
| `termusic play [target] [mode]` | Play a song or playlist via `termusic play <target\|index>`, `-i`/`--index <INDEX>`, or `--playlist <index>` (`--song`, `--playlist`, `--artist <NAME>`), or resume when omitted |
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
