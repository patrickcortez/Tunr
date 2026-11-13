# Tunr

A fast, cross-platform command-line music player with a background playback daemon, `yt-dlp` search and download integration, and an interactive terminal UI (TUI).

> **Personal Use Only:** Tunr (including `tunr download`, `.link` batch downloading, and `tunr search`) is meant for personal, non-commercial use only and not for commercial distribution. See [`LEGAL.md`](LEGAL.md) for details; source code is licensed under the PolyForm Noncommercial License 1.0.0 ([`LICENSE`](LICENSE.md)).

## Prerequisites

- **Rust toolchain** (edition 2024) if building from source (`cargo install` / `cargo build`).
- **`yt-dlp`** in `PATH` (or `~/.local/bin`) for `tunr search` and `tunr download`.
- **`ffmpeg` / `ffprobe`** in `PATH` for audio extraction, format conversion, metadata tagging, and thumbnail embedding.

## Installation

### Option 1: Install from Source (Cargo)

Clone the repository and install `tunr` into `~/.cargo/bin` (ensure `~/.cargo/bin` is on your `PATH`):

```bash
git clone https://github.com/patrickcortez/Tunr.git
cd Tunr
cargo install --path . --locked
```

Alternatively, install directly from Git without cloning first:

```bash
cargo install --git https://github.com/patrickcortez/Tunr.git --locked
```

If you have [`just`](https://github.com/casey/just) or `make` installed, you can also manage source builds with the repository recipes:

```bash
just install    # or: make install
just update     # rebuild and overwrite existing install (`--force`)
just uninstall  # uninstall `tunr`
```

To build locally without installing globally:

```bash
cargo build --release --locked
# Binary output: target/release/tunr (target/release/tunr.exe on Windows)
```

### Option 2: Prebuilt Release Binaries

Download the archive for your platform from [GitHub Releases](https://github.com/patrickcortez/Tunr/releases):

- **Linux (`x86_64`)**: `tunr-<version>-x86_64-unknown-linux-gnu.tar.gz`
- **Windows (`x86_64`)**: `tunr-<version>-x86_64-pc-windows-msvc.zip`
- **macOS (Apple Silicon)**: `tunr-<version>-aarch64-apple-darwin.tar.gz`
- **macOS (Intel)**: `tunr-<version>-x86_64-apple-darwin.tar.gz`
- **Android (`aarch64` / Termux)**: `tunr-<version>-aarch64-linux-android.tar.gz`

Extract the binary and place it in a directory on your `PATH`:

```bash
# Linux / macOS / Termux
tar -xzf tunr-<version>-<target>.tar.gz
install -m 755 tunr ~/.local/bin/tunr
```

```powershell
# Windows (PowerShell)
Expand-Archive tunr-<version>-x86_64-pc-windows-msvc.zip -DestinationPath "$env:LOCALAPPDATA\Programs\tunr"
# Add "$env:LOCALAPPDATA\Programs\tunr" to your User PATH
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

On Linux, install `yt-dlp` and `ffmpeg` via your distribution package manager (for example, `apt`, `dnf`, or `pacman`) or `~/.local/bin`, and verify with `tunr --version` and `yt-dlp --version`.

Tunr discovers external `yt-dlp` automatically across `~/.local/bin`, `PATH`, and platform package manager paths (Homebrew, WinGet, Scoop, Chocolatey), and forwards `deno` or `node` via `--js-runtimes` when present on your system.

## Quick Start

Initialize a Tunr working directory before running commands:

```bash
tunr init ~/Music/tunr-workspace
```

Then launch the interactive TUI or use CLI subcommands:

```bash
tunr                  # Launch interactive TUI (same as `tunr tui`)
tunr search "lofi"    # Search YouTube interactively
```

## Core Commands

| Command | Description |
| --- | --- |
| `tunr init <path>` | Initialize a workspace for config, library index, playlists, and downloads |
| `tunr tui` (or `tunr`) | Launch the interactive player TUI |
| `tunr search [OPTIONS] [QUERY...]` | Search supported providers via `yt-dlp`, copy URLs, or download tracks/videos |
| `tunr download [OPTIONS] <URL\|LINK_FILE...>` | Download media via `yt-dlp` from URLs or `.link` batch files and auto-index audio tracks into the library |
| `tunr add [-p <PLAYLIST>] [-f \| -e] [<PATH> \| -s <SONG>]` | Add an audio file, folder (`-f`), or existing library song (`-e` / `-s` with `-p`) to the library and/or a playlist |
| `tunr update` | Rescan workspace (and configured download directory) for new, moved, or removed audio files |
| `tunr list [songs\|playlists]` | List indexed songs and playlists with 1-based numbers |
| `tunr play [OPTIONS] [TARGET] [MODE]` | Play a song, playlist, `<playlist>[<index>]` (1-based start track), or 1-based `list` index, or resume paused playback when omitted |
| `tunr pause` / `resume` / `stop` | Control daemon playback |
| `tunr next` / `prev` | Skip to next or previous track |
| `tunr rewind [secs]` / `advance <secs>` | Seek backward (omitting `<secs>` rewinds to the beginning of the song) or forward |
| `tunr volume [n]` | Get or set playback volume percentage (`0`–`100`, default `50`; persisted in workspace config) |
| `tunr create <name> [songs=<s1,s2,...>]... [-s <song>]...` | Create a playlist from existing library songs |
| `tunr delete [-s \| -p] <target>` | Remove a song or playlist from the library index (`-p MyList[<index>]` removes only that track from `MyList`; leaves files on disk untouched) |
| `tunr status` | Show current daemon playback status |
| `tunr daemon stop` | Gracefully stop the background playback daemon |
| `tunr version` | Print version information |

## `tunr add`

Import an audio file or directory into the library index, or add a file or existing library song to a playlist (creating the playlist if it does not exist).

### Options

| Flag / Argument | Description |
| --- | --- |
| `[PATH]` | Path to an audio file (or a directory when `-f` / `--folder` is set), or existing library song name/1-based index when `-e` / `--existing` is set. Conflicts with `-s` / `--song` |
| `-f`, `--folder` | Recursively scan `<PATH>` as a directory for supported audio files. Conflicts with `-s` / `--song` and `-e` / `--existing` |
| `-e`, `--existing` | Add an existing library song (never imports from disk) to the target playlist. Accepts `<playlist>` by name (created if absent) or 1-based playlist index (`tunr add -p -e <playlist> <song>` or `tunr add -e <song> -p <playlist>`) |
| `-s`, `--song <SONG>` | Audio file path to import, or (when `-p` / `--playlist` or `-e` / `--existing` is set) an existing library song specified by title, `"Title - Artist"` display string, filename/stem, 1-based `tunr list` index, or unique substring. Conflicts with `<PATH>` and `-f` |
| `-p`, `--playlist <PLAYLIST>` | Target playlist name (created automatically if absent; `-e` also accepts a 1-based playlist index; duplicate song IDs are skipped) |

### Behavior

- **`-e` / `--existing`**: Requires `-p` / `--playlist` and matches `<SONG>` solely against songs already in the library (without reading disk paths). Works cleanly as `tunr add -p -e <playlist-name-or-index> <song-name-or-index>`, `tunr add -e <song> -p <playlist>`, or `tunr add -p <playlist> -e <song>`.
- **`-s` / `--song <SONG>`**: If `<SONG>` exists on disk (and `-e` is not set), `tunr` imports it as an audio file (and adds it to `<PLAYLIST>` if `-p` is provided). Otherwise, when `-p <PLAYLIST>` is provided, `<SONG>` is matched against existing library songs using the same specifiers as `tunr create` and appended to `<PLAYLIST>`.
- **`-p` / `--playlist <PLAYLIST>`**: Accepts `<PLAYLIST>` directly as its flag argument (so flags can appear in any order, such as `tunr add -p Favorites ~/Music/track.mp3` or `tunr add -fp Favorites ~/Music/Album`), and also pairs cleanly with `-p -e <playlist> <song>`.

### Examples

```bash
# Import a single audio file into the library
tunr add ~/Music/track.mp3
tunr add -s ~/Music/track.mp3

# Import a file and add it to a playlist (created automatically if absent)
tunr add ~/Music/track.mp3 -p Favorites
tunr add -p Favorites -s ~/Music/track.mp3

# Recursively import all supported audio files from a folder (and add to a playlist)
tunr add -f ~/Music/Album
tunr add -fp "Road Trip" ~/Music/Album

# Add an existing library song to a playlist using -e / --existing (playlist & song accept name or 1-based index)
tunr add -p -e Favorites "Mundo"
tunr add -p -e 1 5
tunr add -e "Mundo - IV Of Spades" -p Favorites

# Or via -s / --song with -p
tunr add -s "Mundo" -p Favorites
tunr add -p Favorites -s 5
```

## `tunr update`

Recheck your Tunr workspace against the filesystem to index newly added tracks, relink moved or renamed tracks, and prune deleted workspace files:

```bash
tunr update
```

### Behavior

- **Zero-argument command**: `tunr update` takes no positional arguments or flags.
- **Scanned roots**: Recursively scans your workspace `working_dir` (automatically skipping the internal `config` and `playlists` directories and `.tunr-dl-tmp` staging folders), plus your configured `download_dest` directory if it lives outside `working_dir`.
- **Move & rename relinking**: Uniquely matched moved or renamed audio files within the scanned roots are relinked in place, preserving their song IDs and existing playlist memberships.
- **Scoped missing-file pruning**: Missing audio files whose paths are inside the scanned roots are removed from the library index and playlists, while songs imported from external paths outside the scanned roots are left untouched.
- **Automatic TUI reload**: If the interactive `tunr` TUI is open, it detects changes to `library.json` and reloads the updated library automatically.

## `tunr play`

Play a song, playlist, or `<playlist>[<index>]` track by title/name or by its 1-based index from `tunr list`, or omit all arguments to resume paused playback.

### Options

| Flag / Argument | Description |
| --- | --- |
| `[TARGET]` | Song title, `"Title - Artist"` display string (with optional `[MM:SS]`), playlist name, `<playlist>[<index>]` (e.g., `'MyList[2]'`), or 1-based index from `tunr list` |
| `[MODE]` | Playback ordering mode: `next` (default) or `shuffle` |
| `-i`, `--index <INDEX>` | Force strict 1-based list index lookup (cannot be combined with `[TARGET]` or `--artist`) |
| `--song` | Force interpreting `[TARGET]` / `--index` as a song |
| `--playlist` | Force interpreting `[TARGET]` / `--index` as a playlist (also accepts `<playlist>[<index>]`) |
| `--artist <ARTIST>` | Disambiguate song title matches by artist name |
| `-l`, `--loop <BOOL>` | Loop back to the first song after the last song finishes (`true`/`false`, defaults to `false`) |

### Index Resolution Rules

- **Positional integers** (such as `tunr play 3` or `tunr play --playlist 2`) resolve **index-first** when `1 <= N <= len`.
- If a positional integer exceeds the list length (for example, playing a song literally titled `"1984"` in a smaller library), `tunr` checks for an exact title or playlist name match before reporting an out-of-bounds error.
- **`-i` / `--index <INDEX>`** always performs strict 1-based index lookup with no title or playlist fallback.

### Playlist Track Traversal (`<playlist>[<index>]`)

- Append `[<index>]` to a playlist name (for example, `tunr play 'MyList[2]'`) to start playback at the 1-based `<index>` track within that playlist (matching the track numbering in `tunr list`, where `MyList[1]` is the first playable song).
- **Full queue preserved**: Playback keeps all tracks of `<playlist>` in the active queue. In default `next` mode, `tunr next`, `tunr prev`, and `-l` / `--loop` traverse the entire playlist starting from `<index>`; in `shuffle` mode (`tunr play 'MyList[2]' shuffle`), `<index>` is pinned as the first track played and the remaining tracks are shuffled after it.
- Non-numeric bracket suffixes (such as `[Remix]` or `[2024]`) and literal playlist/song names continue to match normally. `[0]`, negative indices, or empty `[]` return an invalid-argument error, and out-of-range indices return a not-found error.
- **Shell quoting**: In `zsh` and other shells that expand `[` and `]` as glob patterns, quote the target (for example, `tunr play 'MyList[2]'` or `tunr play "MyList[2]"`).

### Loop Behavior (`-l` / `--loop`)

- Pass `-l <BOOL>` or `--loop <BOOL>` (`true` or `false`; defaults to `false`) with a song or playlist target (for example, `tunr play MyPlaylist --loop true` or `tunr play 1 -l true`). Passing `--loop` without a target returns an error.
- When `true`, finishing the last track in the active queue (naturally or by seeking past the end with `tunr advance`) wraps back to track 1 instead of ending the playback session. In `shuffle` mode, it wraps to the beginning of the active shuffled queue; single-song queues repeat the song.
- When loop playback is enabled, `tunr status` shows `loop: on` and the TUI header displays `[Loop]`.

### Examples

```bash
# List songs and playlists with 1-based indices
tunr list

# Play song #3 from `tunr list`, or play it in shuffle mode
tunr play 3
tunr play 3 shuffle

# Play playlist #2 from `tunr list`
tunr play --playlist 2

# Start playing a playlist from 1-based track #2 (keeps full queue for next/prev/loop)
tunr play 'MyList[2]'
tunr play 'MyList[2]' shuffle
tunr play --playlist 'MyList[2]' --loop true

# Play a playlist or song index and loop back to track 1 when it finishes
tunr play MyPlaylist --loop true
tunr play -i 1 shuffle -l true

# Explicitly disable looping (default)
tunr play "Echoes" --loop false

# Force strict index lookup
tunr play -i 3
tunr play --playlist -i 2 shuffle

# Play by title with artist disambiguation
tunr play "Echoes" --artist "Artist Two"
```

> **Tip:** Both `tunr play` and `tunr delete` accept `"Title - Artist"` (as well as `"Artist - Title"`, `—`/`–` dashes, and optional `[MM:SS]`/`[--:--]` suffixes from `tunr list`) to disambiguate tracks with identical titles. `tunr delete` intentionally excludes substring and bare numeric index auto-matching.

## `tunr create`

Create a new playlist from existing library songs using `songs=<s1,s2,...>`, repeatable `-s` / `--song` flags, or a combination of both (preserving the order given on the command line).

### Syntax & Options

| Flag / Argument | Description |
| --- | --- |
| `<name>` | Name of the new playlist (must be unique in the library) |
| `songs=<s1,s2,...>` | Comma-separated song specifiers (multiple `songs=` tokens allowed) |
| `-s`, `--song <SONG>` | Repeatable flag taking a single literal song specifier without comma splitting (`-s=-Track` or `--song="-Track"` for titles starting with `-`) |

### Escaping Commas & Quotes in `songs=`

- **Escaped commas**: prefix a literal comma inside a song title or multi-artist field with `\,` (for example, `songs=Sanctuary - Joji\, Justin Raisen,Circles`).
- **CSV double quotes**: wrap an item containing commas in `"..."` (for example, `'songs="Sanctuary - Joji, Justin Raisen",Circles'`); inside CSV quotes, represent literal `"` as `""` or `\"`.
- **Literal `-s` / `--song`**: `-s` never splits on commas or strips inner quotes, making it the simplest choice for titles or artist credits containing commas or quotes.

### Supported Song Specifiers

Each entry in `songs=` or `-s` resolves against indexed library songs using:

1. Exact or case-insensitive song title (e.g. `Mundo`)
2. Display format from `tunr list` / `tunr status` (`"Title - Artist"`, `"Artist - Title"`, `-`/`–`/`—` dashes, optional leading `N. `, and optional trailing `[MM:SS]`/`[--:--]`)
3. Exact audio filename or stem (e.g. `Mundo.mp3` or `Mundo`)
4. 1-based index from `tunr list` (e.g. `songs=5,6`)
5. Unique case-insensitive title substring (or `"Title - Artist"` substring when `" - "` is present)

### Examples

```bash
# Create a playlist by song titles or display strings from `tunr list`
tunr create Favorites songs="Mundo - IV Of Spades, After Dark - Mr.Kitty"

# Copy-paste full `tunr list` lines including duration tags, or pass 1-based list indices
tunr create QuickMix songs="5. Mundo - IV Of Spades [05:49], 6. After Dark - Mr.Kitty [04:19]"
tunr create QuickMix songs=5,6

# Pass multiple `songs=` tokens or escape commas inside artist credits
tunr create NightDrive songs="Mundo - IV Of Spades" songs="After Dark - Mr.Kitty"
tunr create Chill songs='Sanctuary - Joji\, Justin Raisen,Circles'
tunr create Chill 'songs="Sanctuary - Joji, Justin Raisen",Circles'

# Use repeatable -s / --song (commas and quotes are preserved literally)
tunr create Mix -s "Sanctuary - Joji, Justin Raisen" -s "Circles - Post Malone"
tunr create Mix -s "Mundo - IV Of Spades" songs=6,7 -s=-Track
```

## `tunr delete`

Remove a song or playlist from the library index, or remove a single track entry from a playlist. **Audio files on disk are never deleted.**

### Options

| Flag / Argument | Description |
| --- | --- |
| `<TARGET>` | Song specifier (title or `"Title - Artist"` display string), playlist name, or `<playlist>[<index>]` |
| `-s`, `--song` | Force interpreting `<TARGET>` as a library song (removes the song from the library index and strips it from all playlists). Conflicts with `-p` / `--playlist` |
| `-p`, `--playlist` | Force interpreting `<TARGET>` as a playlist name or `<playlist>[<index>]` entry. Conflicts with `-s` / `--song` |

### Behavior

- **Bare auto-detection**: Without `-s` or `-p`, `tunr delete <target>` checks literal song and playlist matches first (returning an ambiguity error if both match, which you can disambiguate with `-s` or `-p`). If neither matches literally and `<target>` uses `<playlist>[<index>]`, it removes that track entry from the playlist.
- **Strict song matching**: Song lookups match exact/case-insensitive titles and `"Title - Artist"` / `"Artist - Title"` display strings (with `-`/`–`/`—` dashes and optional `[MM:SS]`/`[--:--]` suffixes), intentionally excluding substring and bare numeric index auto-matching.
- **Removing a track from a playlist (`delete -p 'MyList[<index>]'`)**:
  - `<index>` is the 1-based track position within `MyList` (matching `tunr list`).
  - Removes **only** that track entry from `MyList` — the song stays in your library index and on disk, and `MyList` remains intact even if its last song is removed.
  - Quote `'MyList[2]'` in shells such as `zsh` where `[` and `]` are glob characters.

### Examples

```bash
# Auto-detect whether <target> is a song or playlist
tunr delete "Echoes"
tunr delete "Echoes - Artist Two"

# Explicitly delete a song from the library index (and strip it from all playlists)
tunr delete -s "Echoes"

# Explicitly delete a playlist (songs remain in the library)
tunr delete -p Favorites

# Remove only track #2 from MyList (keeps the song in the library and on disk)
tunr delete -p 'MyList[2]'
tunr delete 'MyList[2]'
```

## Playback & Seeking Controls

Control an active daemon playback session from the CLI (and query or persist volume even when the daemon is stopped):

```bash
tunr rewind          # Rewind to 00:00 (the beginning of the current song)
tunr rewind 15       # Rewind backward by 15 seconds
tunr advance 15      # Seek forward by 15 seconds
tunr volume          # Show current playback volume percentage (0-100)
tunr volume 75       # Set playback volume to 75% (persisted across restarts)
tunr pause           # Pause playback
tunr resume          # Resume paused playback
tunr next            # Skip to next track
tunr prev            # Return to previous track
tunr stop            # Stop playback and clear the active queue
tunr status          # Print current track, state, mode, loop, and volume status
```

- Calling `tunr rewind` without `<seconds>` seeks in place to `00:00`: if playback is `playing`, it restarts from the beginning; if `paused`, it resets to `00:00` while staying paused; if `stopped`, it returns an error.
- `tunr volume [n]` accepts an integer percentage from `0` to `100` (default `50`), persists it to workspace `config.json`, and updates the running daemon immediately when active (or takes effect on the next playback session when the daemon is stopped).

### Interactive Player TUI Controls

Launch the full-screen player UI with `tunr` (or `tunr tui`):

- `h`/`l` or `←`/`→` (plus `Tab`): switch focus between the Playlists and Tracks panes
- `j`/`k` or `↑`/`↓`: move selection up or down
- `Enter` or `p`: start playing the selected playlist or track
- `Space`: pause or resume playback
- `n` / `b`: next or previous track
- `[` / `]`: rewind or advance by `5` seconds
- `-` / `+` (`=`): decrease or increase volume by `5%` (persisted to workspace config)
- `s`: toggle playback mode (`Next` / `Shuffle`)
- `d`: delete selected song or playlist from the library index (with confirmation)
- `q` / `Esc`: quit the TUI (keeps background daemon playing)
- `Q`: stop playback and quit the TUI

## `tunr search`

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

Note: You can run `tunr search` with `--set-limit`, `-pd`/`--provider-default`, or `-fd`/`--format-default` without a query to save defaults directly.

### Interactive Search TUI Controls

- `↑`/`↓` or `j`/`k` (plus `PgUp`/`PgDn`/`g`/`G`): navigate results
- `/`: start or edit search query
- `Enter`: copy selected URL (browse mode) or download selected item (`-d` / `--download` mode)
- `y`: copy selected URL (works in both browse and download modes)
- `q` / `Esc`: quit

### Examples

```bash
# Search YouTube interactively (defaults: youtube, limit 10)
tunr search "ambient synth"

# Search SoundCloud using website URL or short alias, limiting to 15 results
tunr search -p https://soundcloud.com -n 15 "chill beats"
tunr search -p sc "chill beats"

# Save default provider and default search download format without running a search
tunr search -pd soundcloud -fd .mp4

# Search and download interactively (Press Enter on a result to download, y to copy URL)
tunr search -d -f .mp3 --dest ~/Music/Downloads "jazz piano"

# Download top MP4 video result non-interactively
tunr search --no-tui -d -f mp4 --dest ~/Videos "live session"
```

### Configuration & Precedence

Settings resolve in the following order (highest priority first):

- **Search limit**: `-n`/`--limit` -> `--set-limit` -> `TUNR_SEARCH_LIMIT` -> workspace `config.json` -> `10`
- **Search provider**: `-p`/`--provider` -> `-pd`/`--provider-default` -> `TUNR_SEARCH_PROVIDER` -> workspace `config.json` -> `youtube`
- **Search download format**: `-f`/`--format` -> `-fd`/`-format-default`/`--format-default` -> `TUNR_SEARCH_FORMAT` -> workspace `config.json` -> `mp3`
- **Download destination** (`search -d` and `download`): `--dest` -> saved destination (`download --set-dest`) -> `TUNR_DEST` -> workspace `config.json`

## `tunr download`

Download audio or video via `yt-dlp` from one or more media URLs, `.link` batch files, or a mix of both, and automatically index downloaded audio tracks into your music library.

> **Legal Reminder:** `tunr download` and `.link` batch downloads are intended strictly for personal, private, non-commercial use and not for commercial distribution. See [`LEGAL.md`](LEGAL.md).

### Options

| Flag / Argument | Description |
| --- | --- |
| `[URL\|LINK_FILE...]` | One or more media URLs or `.link` file paths (required unless `--set-dest` is passed alone) |
| `-d`, `--dest <DIR>` | Destination directory for this download run |
| `--set-dest <DIR>` | Persist default download destination directory in workspace `config.json` |
| `-f`, `--format <FORMAT>` | Target audio/video format (`mp3`, `mp4`, `flac`, `m4a`, `ogg`, `wav`, `aac`; defaults to `mp3`) |
| `--playlist` | Allow downloading playlists (by default, single-track downloads are requested) |
| `--no-embed-thumbnail` | Do not embed thumbnail artwork into downloaded media |
| `--no-add` | Do not add downloaded audio tracks to the library index (conflicts with `-p` / `--playlist-name`) |
| `-p`, `--playlist-name <PLAYLIST>` | Add downloaded audio tracks to `<PLAYLIST>` (created automatically if absent; conflicts with `--no-add`) |
| `-j`, `--jobs <N>` | Maximum number of concurrent download statements (defaults to `4`) |
| `--no-tui` | Disable the interactive download TUI and print line-by-line progress |

### `.link` Batch Files

Any positional argument without `://` ending in `.link` (case-insensitive, such as `test-links/test.link`) is parsed as a `.link` file containing one download statement per line. Statements run concurrently up to `-j` / `--jobs`, while conditional operators within a statement execute sequentially.

- **Blank lines & comments**:
  - Blank and whitespace-only lines are ignored.
  - `#` starts a full-line comment, and whitespace followed by `#` starts an inline comment (for example, `https://example.com/track # backup mirror`).
  - `#` immediately after non-whitespace is preserved so URL fragments (such as `https://example.com/watch#t=30s`) remain intact.
- **Conditional operators (`||` and `&&`)**:
  - **`<first> || <other>` (fallback)**: Attempts `<first>`; if it succeeds, `<other>` is skipped. If `<first>` fails, `<other>` is attempted as a fallback—if `<other>` succeeds, `<first>`'s error is reported as a warning and the run still exits `0`.
  - **`<first> && <other>` (sequential dependency)**: Attempts `<first>`; only if `<first>` succeeds is `<other>` downloaded. If `<first>` fails, `<other>` is skipped and the statement fails.
  - **Precedence & associativity**: Arbitrary chains of `&&` and `||` on a single line are supported, with `&&` binding tighter than `||` and both operators associating left-to-right. Parentheses `(` `)` are not supported.

### Examples

```bash
# Download a single URL as MP3 and index it into the library
tunr download https://youtube.com/example-youtube-link

# Run a .link batch/conditional download file (see test-links/test.link)
tunr download test-links/test.link

# Mix .link files and direct URLs with custom format, concurrency, and playlist
tunr download -f flac -j 2 -p Favorites test-links/test.link https://example.com/track

# Save a default download destination without starting a download
tunr download --set-dest ~/Music/Downloads

# Print line progress instead of launching the interactive download TUI
tunr download --no-tui test-links/test.link
```

## Download Formats

Supported formats for `tunr search -d` (`-f` / `-fd` / `TUNR_SEARCH_FORMAT`) and `tunr download -f`:

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

**2 websites** are supported by `tunr search -p` / `-pd`:

| Provider | `yt-dlp` Prefix | Accepted `-p` / `-pd` / `TUNR_SEARCH_PROVIDER` Values |
| --- | --- | --- |
| **YouTube** | `ytsearch` | `youtube`, `yt`, `ytsearch`, `youtube.com`, `www.youtube.com`, `m.youtube.com`, `youtu.be` (including `http://` / `https://` URLs) |
| **SoundCloud** | `scsearch` | `soundcloud`, `sc`, `scsearch`, `soundcloud.com`, `www.soundcloud.com`, `m.soundcloud.com` (including `http://` / `https://` URLs) |

> **Why only 2 search providers?**  
> Although `yt-dlp` (`2026.03.17`) ships 1,872 total extractors ([`supportedsites.md`](https://github.com/yt-dlp/yt-dlp/blob/master/supportedsites.md)), only 20 are `*search*` keyword-search extractors, and most of the others are niche video/archive search engines or defunct/broken without site-specific auth/API keys. Tunr curates **YouTube** (`ytsearch`) and **SoundCloud** (`scsearch`) because both reliably support fast metadata keyword search via `yt-dlp --flat-playlist --dump-single-json`.
>
> To download directly from any other website supported by `yt-dlp`, pass the media URL directly to `tunr download <URL>` instead.

## Legal Notice

Tunr — including `tunr download`, `.link` batch downloading, and `tunr search` — is meant for **personal, private, non-commercial use only** and **not for commercial distribution**, resale, or commercial hosting. You are solely responsible for complying with applicable copyright laws and third-party platform terms of service when using `yt-dlp` or other external tools.

Please read [`LEGAL.md`](LEGAL.md) for the full legal notice, acceptable-use terms, and warranty disclaimer.

## License

Tunr's source code is licensed under the **PolyForm Noncommercial License 1.0.0** (`Copyright (c) 2026 Tezzzieee`). See [`LICENSE`](LICENSE.md) for the full license text.

[`LICENSE`](LICENSE.md) governs the Tunr software itself, while [`LEGAL.md`](LEGAL.md) covers the project's personal-use, non-commercial-distribution, and acceptable-use terms (including user responsibility for third-party copyright and platform compliance).
