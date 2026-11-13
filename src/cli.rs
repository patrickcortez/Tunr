use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use clap::error::ErrorKind;
use clap::{ArgMatches, Args, CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};

use crate::download::{DownloadFormat, DownloadOptions, normalize_urls, parse_download_format_str};
use crate::error::{Result, TunrError};
use crate::ipc::{PlayMode, TargetKind};
use crate::link::{DownloadTarget, is_link_file_arg};
use crate::paths::{self, AppPaths, IpcEndpoint};
use crate::search::{SearchOptions, SearchProvider};

/// Sub-target filter for `tunr list`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ListKind {
    Songs,
    Playlists,
}

/// Target query for `tunr play`: either a text name/number or an explicit 1-based list index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlayQuery {
    Text(String),
    Index(usize),
}

/// Optional path overrides accepted by the hidden `__daemon` subcommand.
#[derive(Debug, Clone, PartialEq, Eq, Args, Default)]
pub struct DaemonOverrides {
    #[arg(long, hide = true)]
    pub library_path: Option<PathBuf>,
    #[arg(long, hide = true)]
    pub config_path: Option<PathBuf>,
    #[arg(long, hide = true)]
    pub socket_path: Option<String>,
    #[arg(long, hide = true)]
    pub pid_path: Option<PathBuf>,
    #[arg(long, hide = true)]
    pub log_path: Option<PathBuf>,
}

impl DaemonOverrides {
    /// Resolve `AppPaths`, applying any explicit overrides passed to `__daemon`.
    pub fn resolve_paths(&self) -> Result<AppPaths> {
        if let (Some(library_path), Some(socket_path), Some(pid_path), Some(log_path)) = (
            &self.library_path,
            &self.socket_path,
            &self.pid_path,
            &self.log_path,
        ) {
            let data_root = library_path.parent().unwrap_or_else(|| Path::new("."));
            let run_root = pid_path.parent().unwrap_or_else(|| Path::new("."));
            let mut paths = AppPaths::from_roots(data_root, run_root);
            paths.library_path = library_path.clone();
            if let Some(p) = &self.config_path {
                paths.config_path = p.clone();
            }
            paths.ipc_endpoint = IpcEndpoint::from_override(socket_path)?;
            paths.pid_path = pid_path.clone();
            paths.log_path = log_path.clone();
            return Ok(paths);
        }

        let mut paths = AppPaths::resolve()?;
        if let Some(p) = &self.library_path {
            paths.library_path = p.clone();
        }
        if let Some(p) = &self.config_path {
            paths.config_path = p.clone();
        }
        if let Some(raw_ep) = &self.socket_path {
            paths.ipc_endpoint = IpcEndpoint::from_override(raw_ep)?;
        }
        if let Some(p) = &self.pid_path {
            paths.pid_path = p.clone();
        }
        if let Some(p) = &self.log_path {
            paths.log_path = p.clone();
        }
        Ok(paths)
    }
}

/// Raw clap CLI parser definition.
#[derive(Debug, Parser)]
#[command(
    name = "tunr",
    version = env!("CARGO_PKG_VERSION"),
    about = "Command-line music player with interactive TUI"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<RawCommand>,
}

#[derive(Debug, Subcommand)]
pub enum RawCommand {
    /// Initialize a Tunr working directory before using other commands
    Init {
        /// Working directory location for Tunr config, playlists, and music
        path: PathBuf,
    },

    /// Launch interactive TUI mode (also default when no subcommand is given)
    Tui,

    /// Print version information
    Version,

    /// Play a song or playlist, or resume paused playback when omitted
    Play {
        /// Song title, playlist name (optionally `<playlist>[<index>]` for 1-based start track), or 1-based list index (omit to resume currently paused playback)
        target: Option<String>,
        /// Playback ordering mode
        #[arg(value_enum, ignore_case = true)]
        mode: Option<PlayMode>,
        /// Force interpreting target as a song title
        #[arg(long, conflicts_with = "playlist")]
        song: bool,
        /// Force interpreting target as a playlist name
        #[arg(long, conflicts_with = "song")]
        playlist: bool,
        /// Disambiguate song target by artist name
        #[arg(long = "artist", value_name = "ARTIST", conflicts_with = "playlist")]
        artist: Option<String>,
        /// Play the 1-based index from `tunr list` (disambiguates numeric titles)
        #[arg(
            short = 'i',
            long = "index",
            value_name = "INDEX",
            allow_negative_numbers = true,
            value_parser = parse_one_based_index,
            conflicts_with = "artist"
        )]
        index: Option<usize>,
        /// Loop back to the first song after the last song finishes (true/false)
        #[arg(
            short = 'l',
            long = "loop",
            action = clap::ArgAction::Set,
            value_name = "BOOL"
        )]
        loop_playback: Option<bool>,
    },

    /// Add an audio file, folder, or existing library song to the library and/or a playlist
    Add {
        /// Recursively scan a folder for supported audio files
        #[arg(short = 'f', long = "folder", conflicts_with_all = ["song", "existing"])]
        folder: bool,
        /// Add an existing library song to a playlist (e.g. `add -p -e <PLAYLIST> <SONG>` or `add -e <SONG> -p <PLAYLIST>`)
        #[arg(short = 'e', long = "existing", conflicts_with = "folder")]
        existing: bool,
        /// Audio file path to import, or existing library song when `-p` / `-e` is set
        #[arg(short = 's', long = "song", value_name = "SONG")]
        song: Option<String>,
        /// Target playlist name (created if absent; `-e` also accepts a 1-based playlist index)
        #[arg(short = 'p', long = "playlist", value_name = "PLAYLIST")]
        playlist_name: Option<String>,
        /// Path to audio file/folder, or existing library song name/index when `-e` is set
        #[arg(value_name = "PATH", conflicts_with = "song")]
        path: Option<PathBuf>,
    },

    /// Skip to the next track
    Next,

    /// Return to the previous track
    Prev,

    /// Rewind playback by <seconds>, or back to the beginning when omitted
    Rewind {
        #[arg(allow_negative_numbers = true, value_parser = parse_non_negative_secs)]
        seconds: Option<f64>,
    },

    /// Advance playback by <seconds>
    Advance {
        #[arg(allow_negative_numbers = true, value_parser = parse_non_negative_secs)]
        seconds: f64,
    },

    /// Get or set playback volume percentage (0-100)
    Volume {
        /// Volume percentage (0-100); omit to display current volume
        #[arg(allow_negative_numbers = true, value_parser = parse_volume)]
        level: Option<u8>,
    },

    /// Remove a song, playlist, or indexed playlist entry (`-p <playlist>[<index>]`) from the library index (never deletes audio files on disk)
    Delete {
        /// Song title, playlist name, or `<playlist>[<index>]` (1-based playlist track)
        target: String,
        /// Force deleting a song
        #[arg(short = 's', long, conflicts_with = "playlist")]
        song: bool,
        /// Force deleting a playlist (or a 1-based entry via `<playlist>[<index>]`)
        #[arg(short = 'p', long, conflicts_with = "song")]
        playlist: bool,
    },

    /// Create a playlist from existing library songs: create <name> [songs=<s1,s2,...>]... [-s <SONG>]...
    Create {
        /// Name of the new playlist
        playlist: String,
        /// Add a song (repeatable; takes one literal song specifier without comma-splitting)
        #[arg(
            short = 's',
            long = "song",
            action = clap::ArgAction::Append,
            value_name = "SONG"
        )]
        song: Vec<String>,
        /// Comma-separated song list prefixed with `songs=` (supports multiple `songs=` tokens)
        #[arg(num_args = 0.., value_name = "songs=<s1,s2,...>")]
        raw_songs: Vec<String>,
    },

    /// Pause playback
    Pause,

    /// Resume playback
    Resume,

    /// Stop playback
    Stop,

    /// Show current daemon playback status
    Status,

    /// List library songs and/or playlists
    List {
        #[arg(value_enum, ignore_case = true)]
        kind: Option<ListKind>,
    },

    /// Recheck the music workspace for new, moved, or removed audio files
    Update,

    /// Download audio via yt-dlp in best quality and auto-index into the library
    Download {
        /// Destination directory for this download
        #[arg(short = 'd', long = "dest", value_name = "DIR")]
        dest: Option<PathBuf>,
        /// Persist default download destination in workspace config
        #[arg(long = "set-dest", value_name = "DIR")]
        set_dest: Option<PathBuf>,
        /// Target audio/video format
        #[arg(
            short = 'f',
            long = "format",
            value_name = "FORMAT",
            value_parser = parse_download_format_str,
            default_value_t = DownloadFormat::Mp3
        )]
        format: DownloadFormat,
        /// Allow downloading playlists (default downloads single track only)
        #[arg(long = "playlist")]
        playlist: bool,
        /// Do not embed thumbnail artwork
        #[arg(long = "no-embed-thumbnail")]
        no_embed_thumbnail: bool,
        /// Do not add downloaded track(s) to the library index
        #[arg(long = "no-add", conflicts_with = "playlist_name")]
        no_add: bool,
        /// Add downloaded track(s) to the named playlist (creating it if absent)
        #[arg(
            short = 'p',
            long = "playlist-name",
            value_name = "PLAYLIST",
            conflicts_with = "no_add"
        )]
        playlist_name: Option<String>,
        /// Maximum concurrent download workers
        #[arg(short = 'j', long = "jobs", value_name = "N", default_value_t = 4)]
        jobs: usize,
        /// Disable interactive TUI loading screen and print line progress
        #[arg(long = "no-tui")]
        no_tui: bool,
        /// One or more media URLs or .link files to download
        #[arg(value_name = "URL|LINK_FILE")]
        urls: Vec<String>,
    },

    /// Search YouTube or SoundCloud (2 supported providers) via yt-dlp and browse, copy, or download results
    Search {
        /// Maximum number of search results (1..=100)
        #[arg(short = 'n', long = "limit", value_name = "N")]
        limit: Option<usize>,
        /// Persist default search result limit in workspace config
        #[arg(long = "set-limit", value_name = "N")]
        set_limit: Option<usize>,
        /// Download selected result instead of copying URL (top result in --no-tui mode)
        #[arg(short = 'd', long = "download")]
        download: bool,
        /// Destination directory when downloading with -d / --download
        #[arg(long = "dest", value_name = "DIR")]
        dest: Option<PathBuf>,
        /// Search provider website/name (2 supported: YouTube, SoundCloud)
        #[arg(
            short = 'p',
            long = "provider",
            value_name = "URL",
            value_parser = SearchProvider::parse
        )]
        provider: Option<SearchProvider>,
        /// Persist default search provider in workspace config (-pd / --pd / --provider-default)
        #[arg(
            long = "provider-default",
            visible_alias = "pd",
            value_name = "URL",
            value_parser = SearchProvider::parse
        )]
        provider_default: Option<SearchProvider>,
        /// Download format for -d / --download (mp3, mp4, flac, m4a, ogg, wav, aac; optional leading '.')
        #[arg(
            short = 'f',
            long = "format",
            value_name = "FORMAT",
            value_parser = parse_download_format_str
        )]
        format: Option<DownloadFormat>,
        /// Persist default download format in workspace config (-fd / --fd / -format-default / --format-default)
        #[arg(
            long = "format-default",
            visible_alias = "fd",
            value_name = "FORMAT",
            value_parser = parse_download_format_str
        )]
        format_default: Option<DownloadFormat>,
        /// Disable interactive TUI and print search results to stdout
        #[arg(long = "no-tui")]
        no_tui: bool,
        /// Search query terms (optional in interactive TUI mode or when setting defaults)
        #[arg(value_name = "QUERY")]
        query: Vec<String>,
    },

    /// Manage the background playback daemon
    #[command(name = "daemon")]
    DaemonControl {
        #[command(subcommand)]
        action: DaemonAction,
    },

    /// Internal background playback daemon entrypoint
    #[command(name = "__daemon", hide = true)]
    Daemon(DaemonOverrides),
}

/// Subcommands for `tunr daemon`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Subcommand)]
pub enum DaemonAction {
    /// Gracefully stop the running background playback daemon
    Stop,
}

/// Source input for `tunr add`: a filesystem path, `-s <SONG>`, or `-e`/`--existing` library song.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddInput {
    Path { path: PathBuf, folder: bool },
    Song(String),
    ExistingSong(String),
}

/// Validated application command ready for dispatch.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Init {
        path: PathBuf,
    },
    Tui,
    Version,
    Play {
        query: PlayQuery,
        mode: PlayMode,
        kind: TargetKind,
        artist: Option<String>,
        loop_playback: bool,
    },
    Add {
        input: AddInput,
        playlist_name: Option<String>,
    },
    Next,
    Prev,
    Rewind {
        seconds: Option<f64>,
    },
    Advance {
        seconds: f64,
    },
    Volume {
        level: Option<u8>,
    },
    Delete {
        target: String,
        kind: TargetKind,
    },
    Create {
        playlist: String,
        songs: Vec<String>,
    },
    Pause,
    Resume,
    Stop,
    Status,
    List {
        kind: Option<ListKind>,
    },
    Update,
    Download(DownloadOptions),
    Search(SearchOptions),
    DaemonStop,
    Daemon(DaemonOverrides),
}

fn parse_one_based_index(raw: &str) -> std::result::Result<usize, String> {
    let val: usize = raw
        .parse()
        .map_err(|_| format!("index must be a positive integer (>= 1), got '{raw}'"))?;
    if val == 0 {
        return Err(format!(
            "index must be a positive integer (>= 1), got '{raw}'"
        ));
    }
    Ok(val)
}

fn parse_non_negative_secs(raw: &str) -> std::result::Result<f64, String> {
    let val: f64 = raw
        .parse()
        .map_err(|_| format!("invalid number of seconds '{raw}'"))?;
    if !val.is_finite() || val < 0.0 {
        return Err(format!("seconds must be a finite number >= 0.0, got {raw}"));
    }
    Ok(if val == 0.0 { 0.0 } else { val })
}

fn parse_volume(raw: &str) -> std::result::Result<u8, String> {
    let val: u8 = raw
        .parse()
        .map_err(|_| format!("volume must be an integer between 0 and 100, got '{raw}'"))?;
    if val > paths::MAX_VOLUME {
        return Err(format!(
            "volume must be an integer between 0 and {}, got '{raw}'",
            paths::MAX_VOLUME
        ));
    }
    Ok(val)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CsvFieldState {
    FieldStart,
    InQuotes,
    AfterClosingQuote,
    Unquoted,
}

#[derive(Debug)]
enum CreateToken<'a> {
    Flag(&'a str),
    Kv(&'a str),
}

/// Push an escaped `,`, `"`, or `\` when preceded by `\`, or keep `\` literally for other characters.
fn push_escaped_or_literal(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    buf: &mut String,
) {
    match chars.peek().copied() {
        Some(esc @ (',' | '"' | '\\')) => {
            chars.next();
            buf.push(esc);
        }
        _ => buf.push('\\'),
    }
}

fn push_quoted_song(buf: &str, songs: &mut Vec<String>) -> Result<()> {
    let trimmed = buf.trim();
    if trimmed.is_empty() {
        return Err(TunrError::InvalidArg(if songs.is_empty() {
            "songs list cannot be empty".to_string()
        } else {
            "song entry in `songs=` cannot be empty".to_string()
        }));
    }
    songs.push(trimmed.to_string());
    Ok(())
}

fn push_unquoted_song(buf: &str, songs: &mut Vec<String>) {
    let trimmed = buf.trim();
    if !trimmed.is_empty() {
        songs.push(trimmed.to_string());
    }
}

/// Parse the RHS of a single `songs=...` value supporting escaped commas (`\,`) and CSV quotes (`"..."`).
fn parse_songs_rhs(rhs: &str) -> Result<Vec<String>> {
    let mut songs = Vec::new();
    let mut state = CsvFieldState::FieldStart;
    let mut buf = String::new();
    let mut chars = rhs.chars().peekable();

    loop {
        match state {
            CsvFieldState::FieldStart => match chars.peek().copied() {
                None => break,
                Some(ch) if ch.is_whitespace() => {
                    chars.next();
                }
                Some(',') => {
                    chars.next();
                }
                Some('"') => {
                    chars.next();
                    buf.clear();
                    state = CsvFieldState::InQuotes;
                }
                Some(_) => {
                    buf.clear();
                    state = CsvFieldState::Unquoted;
                }
            },
            CsvFieldState::InQuotes => match chars.next() {
                None => {
                    return Err(TunrError::InvalidArg(
                        "unclosed quote in `songs=` argument".to_string(),
                    ));
                }
                Some('"') => {
                    if chars.peek() == Some(&'"') {
                        chars.next();
                        buf.push('"');
                    } else {
                        state = CsvFieldState::AfterClosingQuote;
                    }
                }
                Some('\\') => push_escaped_or_literal(&mut chars, &mut buf),
                Some(ch) => buf.push(ch),
            },
            CsvFieldState::AfterClosingQuote => match chars.next() {
                None => {
                    push_quoted_song(&buf, &mut songs)?;
                    break;
                }
                Some(',') => {
                    push_quoted_song(&buf, &mut songs)?;
                    buf.clear();
                    state = CsvFieldState::FieldStart;
                }
                Some(ch) if ch.is_whitespace() => {}
                Some(ch) => {
                    return Err(TunrError::InvalidArg(format!(
                        "unexpected character '{ch}' after closing quote in `songs=` argument"
                    )));
                }
            },
            CsvFieldState::Unquoted => match chars.next() {
                None => {
                    push_unquoted_song(&buf, &mut songs);
                    break;
                }
                Some(',') => {
                    push_unquoted_song(&buf, &mut songs);
                    buf.clear();
                    state = CsvFieldState::FieldStart;
                }
                Some('\\') => push_escaped_or_literal(&mut chars, &mut buf),
                Some(ch) => buf.push(ch),
            },
        }
    }

    if songs.is_empty() {
        return Err(TunrError::InvalidArg(
            "songs list cannot be empty".to_string(),
        ));
    }
    Ok(songs)
}

/// Parse `songs=<s1,s2,...>` from CLI tokens (supporting multiple `songs=` tokens,
/// escaped commas `\,`, and CSV-quoted fields `"..."`).
pub fn parse_songs_kv(tokens: &[String]) -> Result<Vec<String>> {
    if tokens.is_empty() {
        return Err(TunrError::InvalidArg(
            "expected `songs=<song1,song2,...>` argument".to_string(),
        ));
    }

    let mut groups: Vec<Vec<&str>> = Vec::new();
    for token in tokens {
        if token.starts_with("songs=") {
            groups.push(vec![token.as_str()]);
        } else if let Some(current) = groups.last_mut() {
            current.push(token.as_str());
        } else {
            return Err(TunrError::InvalidArg(
                "expected `songs=<song1,song2,...>` argument".to_string(),
            ));
        }
    }

    let mut songs = Vec::new();
    for group in groups {
        let joined = group.join(" ");
        let rhs = joined
            .strip_prefix("songs=")
            .expect("group always starts with `songs=`");
        songs.extend(parse_songs_rhs(rhs)?);
    }
    Ok(songs)
}

fn trim_non_empty(raw: &str, empty_msg: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(TunrError::InvalidArg(empty_msg.to_string()));
    }
    Ok(trimmed.to_string())
}

fn validate_playlist_name(raw: &str) -> Result<String> {
    trim_non_empty(raw, "playlist name cannot be empty")
}

fn validate_song_flag_value(raw: &str) -> Result<String> {
    trim_non_empty(raw, "--song value cannot be empty")
}

fn flush_kv_batch(batch: &mut Vec<String>, out: &mut Vec<String>) -> Result<()> {
    if !batch.is_empty() {
        out.extend(parse_songs_kv(batch)?);
        batch.clear();
    }
    Ok(())
}

/// Extract `create` songs in exact CLI occurrence order across `-s`/`--song` and `songs=...` tokens,
/// flushing contiguous `raw_songs` batches whenever `-s` or a non-contiguous index intervenes.
fn extract_create_songs(sub: &ArgMatches) -> Result<Vec<String>> {
    let mut items: Vec<(usize, CreateToken<'_>)> = Vec::new();

    if let (Some(indices), Some(values)) =
        (sub.indices_of("song"), sub.get_many::<String>("song"))
    {
        for (idx, val) in indices.zip(values) {
            items.push((idx, CreateToken::Flag(val.as_str())));
        }
    }

    if let (Some(indices), Some(values)) = (
        sub.indices_of("raw_songs"),
        sub.get_many::<String>("raw_songs"),
    ) {
        for (idx, val) in indices.zip(values) {
            items.push((idx, CreateToken::Kv(val.as_str())));
        }
    }

    if items.is_empty() {
        return Err(TunrError::InvalidArg(
            "missing songs: pass `songs=<s1,s2,...>` or `-s <SONG>`".to_string(),
        ));
    }

    items.sort_by_key(|(idx, _)| *idx);

    let mut songs = Vec::new();
    let mut kv_batch: Vec<String> = Vec::new();
    let mut prev_kv_idx: Option<usize> = None;

    for (idx, token) in items {
        match token {
            CreateToken::Flag(val) => {
                flush_kv_batch(&mut kv_batch, &mut songs)?;
                prev_kv_idx = None;
                songs.push(validate_song_flag_value(val)?);
            }
            CreateToken::Kv(val) => {
                if let Some(prev) = prev_kv_idx
                    && idx != prev + 1
                {
                    flush_kv_batch(&mut kv_batch, &mut songs)?;
                }
                kv_batch.push(val.to_string());
                prev_kv_idx = Some(idx);
            }
        }
    }
    flush_kv_batch(&mut kv_batch, &mut songs)?;

    if songs.is_empty() {
        return Err(TunrError::InvalidArg(
            "songs list cannot be empty".to_string(),
        ));
    }
    Ok(songs)
}

/// Validate filesystem preconditions for `tunr add`:
/// `-f` requires an existing directory; without `-f` requires an existing regular file.
pub fn validate_add_path(path: &Path, folder: bool) -> Result<()> {
    if !path.exists() {
        return Err(TunrError::NotFound(format!(
            "path does not exist: {}",
            path.display()
        )));
    }
    if folder && !path.is_dir() {
        return Err(TunrError::InvalidArg(format!(
            "`-f` requires a directory path: {}",
            path.display()
        )));
    }
    if !folder && !path.is_file() {
        return Err(TunrError::InvalidArg(format!(
            "adding without `-f` requires a file path (use `-f` for folders): {}",
            path.display()
        )));
    }
    Ok(())
}

fn resolve_target_kind(song: bool, playlist: bool) -> Result<TargetKind> {
    match (song, playlist) {
        (true, true) => Err(TunrError::InvalidArg(
            "cannot specify both `--song` and `--playlist`".to_string(),
        )),
        (true, false) => Ok(TargetKind::Song),
        (false, true) => Ok(TargetKind::Playlist),
        (false, false) => Ok(TargetKind::Auto),
    }
}

fn validate_cli(cli: Cli) -> Result<Command> {
    let Some(raw) = cli.command else {
        return Ok(Command::Tui);
    };

    match raw {
        RawCommand::Init { path } => {
            if path.as_os_str().is_empty() || path.to_str().is_some_and(|s| s.trim().is_empty()) {
                return Err(TunrError::InvalidArg(
                    "working directory path cannot be empty".to_string(),
                ));
            }
            Ok(Command::Init { path })
        }
        RawCommand::Tui => Ok(Command::Tui),
        RawCommand::Version => Ok(Command::Version),
        RawCommand::Play {
            target,
            mut mode,
            song,
            playlist,
            artist,
            index,
            loop_playback,
        } => {
            if let Some(idx) = index {
                if artist.is_some() {
                    return Err(TunrError::InvalidArg(
                        "--artist cannot be used with --index".to_string(),
                    ));
                }
                if let Some(t) = target {
                    if mode.is_none() {
                        match PlayMode::from_str(&t, true) {
                            Ok(parsed_mode) => mode = Some(parsed_mode),
                            Err(_) => {
                                return Err(TunrError::InvalidArg(
                                    "cannot specify both `--index` and a positional target"
                                        .to_string(),
                                ));
                            }
                        }
                    } else {
                        return Err(TunrError::InvalidArg(
                            "cannot specify both `--index` and a positional target".to_string(),
                        ));
                    }
                }
                return Ok(Command::Play {
                    query: PlayQuery::Index(idx),
                    mode: mode.unwrap_or_default(),
                    kind: resolve_target_kind(song, playlist)?,
                    artist: None,
                    loop_playback: loop_playback.unwrap_or(false),
                });
            }

            match target {
                None => {
                    if song || playlist || mode.is_some() || artist.is_some() || loop_playback.is_some() {
                        return Err(TunrError::InvalidArg(
                            "play requires a song or playlist name when flags or play mode are specified"
                                .to_string(),
                        ));
                    }
                    Ok(Command::Resume)
                }
                Some(target) => {
                    if target.trim().is_empty() {
                        return Err(TunrError::InvalidArg(
                            "play target cannot be empty".to_string(),
                        ));
                    }
                    let clean_artist = match artist {
                        Some(raw) => {
                            let trimmed = raw.trim();
                            if trimmed.is_empty() {
                                return Err(TunrError::InvalidArg(
                                    "--artist cannot be empty".to_string(),
                                ));
                            }
                            Some(trimmed.to_string())
                        }
                        None => None,
                    };
                    Ok(Command::Play {
                        query: PlayQuery::Text(target),
                        mode: mode.unwrap_or_default(),
                        kind: resolve_target_kind(song, playlist)?,
                        artist: clean_artist,
                        loop_playback: loop_playback.unwrap_or(false),
                    })
                }
            }
        }
        RawCommand::Add {
            folder,
            existing,
            song,
            playlist_name,
            path,
        } => {
            let clean_playlist = playlist_name
                .as_deref()
                .map(validate_playlist_name)
                .transpose()?;
            if existing {
                if folder {
                    return Err(TunrError::InvalidArg(
                        "cannot combine `--folder` with `--existing`".to_string(),
                    ));
                }
                if clean_playlist.is_none() {
                    return Err(TunrError::InvalidArg(
                        "`-e`/`--existing` requires a target playlist (`-p <PLAYLIST>`)"
                            .to_string(),
                    ));
                }
                let query = match (song, path) {
                    (Some(raw_song), None) => trim_non_empty(&raw_song, "song cannot be empty")?,
                    (None, Some(path)) => {
                        let raw = path.to_string_lossy();
                        trim_non_empty(&raw, "song cannot be empty")?
                    }
                    (Some(_), Some(_)) => {
                        return Err(TunrError::InvalidArg(
                            "cannot specify both `--song` and `<PATH>`".to_string(),
                        ));
                    }
                    (None, None) => {
                        return Err(TunrError::InvalidArg(
                            "provide an existing song name or index with `-e`".to_string(),
                        ));
                    }
                };
                return Ok(Command::Add {
                    input: AddInput::ExistingSong(query),
                    playlist_name: clean_playlist,
                });
            }
            let input = match (song, path) {
                (Some(raw_song), None) => {
                    AddInput::Song(trim_non_empty(&raw_song, "song cannot be empty")?)
                }
                (None, Some(path)) => {
                    if path.as_os_str().is_empty()
                        || path.to_str().is_some_and(|s| s.trim().is_empty())
                    {
                        return Err(TunrError::InvalidArg(
                            "add path cannot be empty".to_string(),
                        ));
                    }
                    AddInput::Path { path, folder }
                }
                (Some(_), Some(_)) => {
                    return Err(TunrError::InvalidArg(
                        "cannot specify both `--song` and `<PATH>`".to_string(),
                    ));
                }
                (None, None) => {
                    return Err(TunrError::InvalidArg(
                        "provide a song via `-s <SONG>` or a path `<PATH>`".to_string(),
                    ));
                }
            };
            Ok(Command::Add {
                input,
                playlist_name: clean_playlist,
            })
        }
        RawCommand::Next => Ok(Command::Next),
        RawCommand::Prev => Ok(Command::Prev),
        RawCommand::Rewind { seconds } => Ok(Command::Rewind { seconds }),
        RawCommand::Advance { seconds } => Ok(Command::Advance { seconds }),
        RawCommand::Volume { level } => Ok(Command::Volume { level }),
        RawCommand::Delete {
            target,
            song,
            playlist,
        } => {
            if target.trim().is_empty() {
                return Err(TunrError::InvalidArg(
                    "delete target cannot be empty".to_string(),
                ));
            }
            Ok(Command::Delete {
                target,
                kind: resolve_target_kind(song, playlist)?,
            })
        }
        RawCommand::Create {
            playlist,
            song,
            raw_songs,
        } => {
            let playlist = validate_playlist_name(&playlist)?;
            let mut songs = Vec::new();
            for s in &song {
                songs.push(validate_song_flag_value(s)?);
            }
            if !raw_songs.is_empty() {
                songs.extend(parse_songs_kv(&raw_songs)?);
            }
            if songs.is_empty() {
                return Err(TunrError::InvalidArg(
                    "missing songs: pass `songs=<s1,s2,...>` or `-s <SONG>`".to_string(),
                ));
            }
            Ok(Command::Create { playlist, songs })
        }
        RawCommand::Pause => Ok(Command::Pause),
        RawCommand::Resume => Ok(Command::Resume),
        RawCommand::Stop => Ok(Command::Stop),
        RawCommand::Status => Ok(Command::Status),
        RawCommand::List { kind } => Ok(Command::List { kind }),
        RawCommand::Update => Ok(Command::Update),
        RawCommand::Download {
            dest,
            set_dest,
            format,
            playlist,
            no_embed_thumbnail,
            no_add,
            playlist_name,
            jobs,
            no_tui,
            urls,
        } => {
            if jobs == 0 {
                return Err(TunrError::InvalidArg(
                    "--jobs must be at least 1".to_string(),
                ));
            }
            if let Some(ref d) = dest
                && (d.as_os_str().is_empty() || d.to_str().is_some_and(|s| s.trim().is_empty()))
            {
                return Err(TunrError::InvalidArg(
                    "--dest requires a non-empty directory path".to_string(),
                ));
            }
            if let Some(ref d) = set_dest
                && (d.as_os_str().is_empty() || d.to_str().is_some_and(|s| s.trim().is_empty()))
            {
                return Err(TunrError::InvalidArg(
                    "--set-dest requires a non-empty directory path".to_string(),
                ));
            }
            let clean_playlist = match playlist_name {
                Some(name) => {
                    let trimmed = name.trim();
                    if trimmed.is_empty() {
                        return Err(TunrError::InvalidArg(
                            "playlist name cannot be empty".to_string(),
                        ));
                    }
                    Some(trimmed.to_string())
                }
                None => None,
            };
            let has_link_files = urls.iter().any(|arg| is_link_file_arg(arg));
            let (normalized_urls, targets) = if !has_link_files {
                (normalize_urls(&urls)?, Vec::new())
            } else {
                let mut seen_urls = HashSet::new();
                let mut out_targets = Vec::new();
                for raw in &urls {
                    if raw.trim().is_empty() {
                        return Err(TunrError::InvalidArg(
                            "URL or .link path cannot be empty".to_string(),
                        ));
                    }
                    if is_link_file_arg(raw) {
                        // Preserve literal path including internal spaces; do not split_whitespace().
                        out_targets.push(DownloadTarget::LinkFile(PathBuf::from(raw.trim())));
                    } else {
                        for u in normalize_urls(std::slice::from_ref(raw))? {
                            if seen_urls.insert(u.clone()) {
                                out_targets.push(DownloadTarget::Url(u));
                            }
                        }
                    }
                }
                (Vec::new(), out_targets)
            };
            if normalized_urls.is_empty() && targets.is_empty() && set_dest.is_none() {
                return Err(TunrError::InvalidArg(
                    "missing URL or .link file (pass <URL|LINK_FILE...> or --set-dest <DIR>)"
                        .to_string(),
                ));
            }
            Ok(Command::Download(DownloadOptions {
                urls: normalized_urls,
                targets,
                dest,
                set_dest,
                format,
                playlist,
                embed_thumbnail: !no_embed_thumbnail,
                auto_add: !no_add,
                playlist_name: clean_playlist,
                jobs,
                no_tui,
            }))
        }
        RawCommand::Search {
            limit,
            set_limit,
            download,
            dest,
            provider,
            provider_default,
            format,
            format_default,
            no_tui,
            query,
        } => {
            if let Some(n) = limit {
                paths::validate_search_limit(n)?;
            }
            if let Some(n) = set_limit {
                paths::validate_search_limit(n)?;
            }
            if let Some(ref d) = dest
                && (d.as_os_str().is_empty() || d.to_str().is_some_and(|s| s.trim().is_empty()))
            {
                return Err(TunrError::InvalidArg(
                    "--dest requires a non-empty directory path".to_string(),
                ));
            }
            let joined_query = query.join(" ").trim().to_string();
            let setting_default =
                set_limit.is_some() || provider_default.is_some() || format_default.is_some();
            if joined_query.is_empty() && no_tui && (!setting_default || download) {
                return Err(TunrError::InvalidArg(
                    "missing search query (pass <QUERY...> or omit --no-tui for interactive prompt)"
                        .to_string(),
                ));
            }
            Ok(Command::Search(SearchOptions {
                query: joined_query,
                limit,
                set_limit,
                no_tui,
                download,
                dest,
                provider,
                provider_default,
                format,
                format_default,
            }))
        }
        RawCommand::DaemonControl {
            action: DaemonAction::Stop,
        } => Ok(Command::DaemonStop),
        RawCommand::Daemon(overrides) => Ok(Command::Daemon(overrides)),
    }
}

/// Normalize single-dash multi-character `search` default flags (`-pd`, `-fd`, `-format-default`)
/// into their canonical `--` long forms before clap parses them, preserving tokens after `--`
/// and immediate flag values.
pub fn normalize_search_args<I, T>(args: I) -> Vec<OsString>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString>,
{
    let raw: Vec<OsString> = args.into_iter().map(Into::into).collect();
    if raw.get(1).and_then(|s| s.to_str()) != Some("search") {
        return raw;
    }

    let mut out = Vec::with_capacity(raw.len());
    out.push(raw[0].clone());
    out.push(raw[1].clone());

    let mut after_double_dash = false;
    let mut expect_value = false;

    for arg in raw.into_iter().skip(2) {
        if after_double_dash {
            out.push(arg);
            continue;
        }
        if expect_value {
            expect_value = false;
            out.push(arg);
            continue;
        }

        let Some(text) = arg.to_str() else {
            out.push(arg);
            continue;
        };

        if text == "--" {
            after_double_dash = true;
            out.push(arg);
            continue;
        }

        let rewritten = if text == "-pd" {
            expect_value = true;
            Some("--provider-default".to_string())
        } else if let Some(rest) = text.strip_prefix("-pd=") {
            Some(format!("--provider-default={rest}"))
        } else if text == "-fd" || text == "-format-default" {
            expect_value = true;
            Some("--format-default".to_string())
        } else if let Some(rest) = text.strip_prefix("-fd=") {
            Some(format!("--format-default={rest}"))
        } else if let Some(rest) = text.strip_prefix("-format-default=") {
            Some(format!("--format-default={rest}"))
        } else {
            None
        };

        if let Some(next) = rewritten {
            out.push(OsString::from(next));
        } else {
            if matches!(
                text,
                "-n" | "--limit"
                    | "--set-limit"
                    | "--dest"
                    | "-p"
                    | "--provider"
                    | "--provider-default"
                    | "--pd"
                    | "-f"
                    | "--format"
                    | "--format-default"
                    | "--fd"
            ) {
                expect_value = true;
            }
            out.push(arg);
        }
    }

    out
}

/// Normalize `-p -e` / `--playlist --existing` / `-pe` / `-ep` in `tunr add` so both
/// `add -p -e <PLAYLIST> <SONG>` and `add -e <SONG> -p <PLAYLIST>` parse unambiguously.
pub fn normalize_add_args<I, T>(args: I) -> Vec<OsString>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString>,
{
    let raw: Vec<OsString> = args.into_iter().map(Into::into).collect();
    if raw.get(1).and_then(|s| s.to_str()) != Some("add") {
        return raw;
    }

    let mut out = Vec::with_capacity(raw.len() + 1);
    out.push(raw[0].clone());
    out.push(raw[1].clone());

    let mut after_double_dash = false;
    let mut expect_value = false;
    let mut i = 2;

    while i < raw.len() {
        let arg = &raw[i];
        if after_double_dash {
            out.push(arg.clone());
            i += 1;
            continue;
        }
        if expect_value {
            expect_value = false;
            out.push(arg.clone());
            i += 1;
            continue;
        }

        let Some(text) = arg.to_str() else {
            out.push(arg.clone());
            i += 1;
            continue;
        };

        if text == "--" {
            after_double_dash = true;
            out.push(arg.clone());
            i += 1;
            continue;
        }

        if matches!(text, "-pe" | "-ep") {
            out.push(OsString::from("-e"));
            out.push(OsString::from("-p"));
            expect_value = true;
            i += 1;
            continue;
        }

        if matches!(text, "-p" | "--playlist")
            && raw
                .get(i + 1)
                .and_then(|next| next.to_str())
                .is_some_and(|next| matches!(next, "-e" | "--existing"))
        {
            out.push(raw[i + 1].clone());
            out.push(arg.clone());
            expect_value = true;
            i += 2;
            continue;
        }

        if matches!(text, "-p" | "--playlist" | "-s" | "--song") {
            expect_value = true;
        }
        out.push(arg.clone());
        i += 1;
    }

    out
}

fn validate_cli_with_matches(cli: Cli, matches: &ArgMatches) -> Result<Command> {
    if let Some(RawCommand::Create { playlist, .. }) = &cli.command {
        let playlist = validate_playlist_name(playlist)?;
        let sub = matches
            .subcommand_matches("create")
            .expect("matched RawCommand::Create must have `create` subcommand matches");
        let songs = extract_create_songs(sub)?;
        return Ok(Command::Create { playlist, songs });
    }
    validate_cli(cli)
}

/// Parse and validate CLI arguments from `std::env::args_os()`, exiting cleanly on `--help`/`--version`.
pub fn parse_cli() -> Result<Command> {
    let args = normalize_add_args(normalize_search_args(std::env::args_os()));
    let matches = match Cli::command().try_get_matches_from(args) {
        Ok(m) => m,
        Err(err) => match err.kind() {
            ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => err.exit(),
            _ => return Err(TunrError::InvalidArg(err.to_string())),
        },
    };
    let cli = Cli::from_arg_matches(&matches)
        .map_err(|err| TunrError::InvalidArg(err.to_string()))?;
    validate_cli_with_matches(cli, &matches)
}

/// Parse and validate CLI arguments from an arbitrary iterator (for tests).
pub fn parse_cli_from<I, T>(args: I) -> Result<Command>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let normalized = normalize_add_args(normalize_search_args(args));
    let matches = Cli::command()
        .try_get_matches_from(normalized)
        .map_err(|err| TunrError::InvalidArg(err.to_string()))?;
    let cli = Cli::from_arg_matches(&matches)
        .map_err(|err| TunrError::InvalidArg(err.to_string()))?;
    validate_cli_with_matches(cli, &matches)
}

/// Render clap's `-V` / `--version` string.
pub fn rendered_version() -> String {
    Cli::command().render_version()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn no_args_and_tui_parse_to_tui() {
        assert_eq!(parse_cli_from(["tunr"]).unwrap(), Command::Tui);
        assert_eq!(parse_cli_from(["tunr", "tui"]).unwrap(), Command::Tui);
    }

    #[test]
    fn version_command_and_flag_match_expected_version() {
        assert_eq!(
            parse_cli_from(["tunr", "version"]).unwrap(),
            Command::Version
        );
        assert_eq!(rendered_version().trim(), "tunr 1.0.0");
        let err = Cli::try_parse_from(["tunr", "-V"]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::DisplayVersion);
        assert_eq!(err.to_string().trim(), "tunr 1.0.0");
    }

    #[test]
    fn play_parses_defaults_modes_and_disambiguation() {
        assert_eq!(
            parse_cli_from(["tunr", "play"]).unwrap(),
            Command::Resume
        );
        assert_eq!(
            parse_cli_from(["tunr", "play", "My Song"]).unwrap(),
            Command::Play {
                query: PlayQuery::Text("My Song".to_string()),
                mode: PlayMode::Next,
                kind: TargetKind::Auto,
                artist: None,
                loop_playback: false,
            }
        );
        assert_eq!(
            parse_cli_from(["tunr", "play", "Chill Mix", "shuffle"]).unwrap(),
            Command::Play {
                query: PlayQuery::Text("Chill Mix".to_string()),
                mode: PlayMode::Shuffle,
                kind: TargetKind::Auto,
                artist: None,
                loop_playback: false,
            }
        );
        assert_eq!(
            parse_cli_from(["tunr", "play", "Chill Mix", "NEXT", "--playlist"]).unwrap(),
            Command::Play {
                query: PlayQuery::Text("Chill Mix".to_string()),
                mode: PlayMode::Next,
                kind: TargetKind::Playlist,
                artist: None,
                loop_playback: false,
            }
        );
        assert_eq!(
            parse_cli_from(["tunr", "play", "Track", "--song"]).unwrap(),
            Command::Play {
                query: PlayQuery::Text("Track".to_string()),
                mode: PlayMode::Next,
                kind: TargetKind::Song,
                artist: None,
                loop_playback: false,
            }
        );
        assert_eq!(
            parse_cli_from(["tunr", "play", "Track", "--artist", "Artist A"]).unwrap(),
            Command::Play {
                query: PlayQuery::Text("Track".to_string()),
                mode: PlayMode::Next,
                kind: TargetKind::Auto,
                artist: Some("Artist A".to_string()),
                loop_playback: false,
            }
        );
        assert_eq!(
            parse_cli_from([
                "tunr",
                "play",
                "Track",
                "shuffle",
                "--song",
                "--artist",
                "  Artist B  ",
            ])
            .unwrap(),
            Command::Play {
                query: PlayQuery::Text("Track".to_string()),
                mode: PlayMode::Shuffle,
                kind: TargetKind::Song,
                artist: Some("Artist B".to_string()),
                loop_playback: false,
            }
        );
        assert_eq!(
            parse_cli_from(["tunr", "play", "MyList[2]"]).unwrap(),
            Command::Play {
                query: PlayQuery::Text("MyList[2]".to_string()),
                mode: PlayMode::Next,
                kind: TargetKind::Auto,
                artist: None,
                loop_playback: false,
            }
        );
        assert_eq!(
            parse_cli_from(["tunr", "play", "MyList[2]", "shuffle"]).unwrap(),
            Command::Play {
                query: PlayQuery::Text("MyList[2]".to_string()),
                mode: PlayMode::Shuffle,
                kind: TargetKind::Auto,
                artist: None,
                loop_playback: false,
            }
        );
        assert_eq!(
            parse_cli_from(["tunr", "play", "MyList[2]", "--playlist"]).unwrap(),
            Command::Play {
                query: PlayQuery::Text("MyList[2]".to_string()),
                mode: PlayMode::Next,
                kind: TargetKind::Playlist,
                artist: None,
                loop_playback: false,
            }
        );
        assert!(matches!(
            parse_cli_from(["tunr", "play", "-i", "2", "MyList[1]"]),
            Err(TunrError::InvalidArg(_))
        ));
        assert!(parse_cli_from(["tunr", "play", "--song"]).is_err());
        assert!(parse_cli_from(["tunr", "play", "--playlist"]).is_err());
        assert!(matches!(
            parse_cli_from(["tunr", "play", "--artist", "Artist A"]),
            Err(TunrError::InvalidArg(_))
        ));
        assert!(matches!(
            parse_cli_from(["tunr", "play", "Track", "--artist", "   "]),
            Err(TunrError::InvalidArg(_))
        ));
        assert!(matches!(
            parse_cli_from(["tunr", "play", "Mix", "--playlist", "--artist", "Artist A"]),
            Err(TunrError::InvalidArg(_))
        ));
        assert!(parse_cli_from(["tunr", "play", "Track", "random"]).is_err());
        assert!(parse_cli_from(["tunr", "play", "Track", "--song", "--playlist"]).is_err());
        assert!(parse_cli_from(["tunr", "play", "   "]).is_err());
    }

    #[test]
    fn play_parses_loop_flag_and_rejects_invalid_usage() {
        assert_eq!(
            parse_cli_from(["tunr", "play", "My Song", "-l", "true"]).unwrap(),
            Command::Play {
                query: PlayQuery::Text("My Song".to_string()),
                mode: PlayMode::Next,
                kind: TargetKind::Auto,
                artist: None,
                loop_playback: true,
            }
        );
        assert_eq!(
            parse_cli_from(["tunr", "play", "My Song", "-l", "false"]).unwrap(),
            Command::Play {
                query: PlayQuery::Text("My Song".to_string()),
                mode: PlayMode::Next,
                kind: TargetKind::Auto,
                artist: None,
                loop_playback: false,
            }
        );
        assert_eq!(
            parse_cli_from(["tunr", "play", "MyPlaylist", "--loop", "true"]).unwrap(),
            Command::Play {
                query: PlayQuery::Text("MyPlaylist".to_string()),
                mode: PlayMode::Next,
                kind: TargetKind::Auto,
                artist: None,
                loop_playback: true,
            }
        );
        assert_eq!(
            parse_cli_from(["tunr", "play", "MyPlaylist", "--loop", "false"]).unwrap(),
            Command::Play {
                query: PlayQuery::Text("MyPlaylist".to_string()),
                mode: PlayMode::Next,
                kind: TargetKind::Auto,
                artist: None,
                loop_playback: false,
            }
        );
        assert_eq!(
            parse_cli_from(["tunr", "play", "MyPlaylist", "--loop=true"]).unwrap(),
            Command::Play {
                query: PlayQuery::Text("MyPlaylist".to_string()),
                mode: PlayMode::Next,
                kind: TargetKind::Auto,
                artist: None,
                loop_playback: true,
            }
        );
        assert_eq!(
            parse_cli_from(["tunr", "play", "--index", "2", "shuffle", "--loop", "true"])
                .unwrap(),
            Command::Play {
                query: PlayQuery::Index(2),
                mode: PlayMode::Shuffle,
                kind: TargetKind::Auto,
                artist: None,
                loop_playback: true,
            }
        );

        assert!(matches!(
            parse_cli_from(["tunr", "play", "My Song", "--loop"]),
            Err(TunrError::InvalidArg(_))
        ));
        assert!(matches!(
            parse_cli_from(["tunr", "play", "My Song", "--loop", "maybe"]),
            Err(TunrError::InvalidArg(_))
        ));
        assert!(matches!(
            parse_cli_from(["tunr", "play", "--loop", "true"]),
            Err(TunrError::InvalidArg(_))
        ));
        assert!(matches!(
            parse_cli_from(["tunr", "play", "--loop", "false"]),
            Err(TunrError::InvalidArg(_))
        ));
    }

    #[test]
    fn play_parses_numeric_targets_and_index_flag() {
        assert_eq!(
            parse_cli_from(["tunr", "play", "3"]).unwrap(),
            Command::Play {
                query: PlayQuery::Text("3".to_string()),
                mode: PlayMode::Next,
                kind: TargetKind::Auto,
                artist: None,
                loop_playback: false,
            }
        );
        assert_eq!(
            parse_cli_from(["tunr", "play", "3", "shuffle"]).unwrap(),
            Command::Play {
                query: PlayQuery::Text("3".to_string()),
                mode: PlayMode::Shuffle,
                kind: TargetKind::Auto,
                artist: None,
                loop_playback: false,
            }
        );
        assert_eq!(
            parse_cli_from(["tunr", "play", "--index", "3"]).unwrap(),
            Command::Play {
                query: PlayQuery::Index(3),
                mode: PlayMode::Next,
                kind: TargetKind::Auto,
                artist: None,
                loop_playback: false,
            }
        );
        assert_eq!(
            parse_cli_from(["tunr", "play", "-i", "3"]).unwrap(),
            Command::Play {
                query: PlayQuery::Index(3),
                mode: PlayMode::Next,
                kind: TargetKind::Auto,
                artist: None,
                loop_playback: false,
            }
        );
        assert_eq!(
            parse_cli_from(["tunr", "play", "--index", "3", "shuffle"]).unwrap(),
            Command::Play {
                query: PlayQuery::Index(3),
                mode: PlayMode::Shuffle,
                kind: TargetKind::Auto,
                artist: None,
                loop_playback: false,
            }
        );
        assert_eq!(
            parse_cli_from(["tunr", "play", "-i", "2", "--playlist"]).unwrap(),
            Command::Play {
                query: PlayQuery::Index(2),
                mode: PlayMode::Next,
                kind: TargetKind::Playlist,
                artist: None,
                loop_playback: false,
            }
        );
        assert_eq!(
            parse_cli_from(["tunr", "play", "--song", "-i", "4"]).unwrap(),
            Command::Play {
                query: PlayQuery::Index(4),
                mode: PlayMode::Next,
                kind: TargetKind::Song,
                artist: None,
                loop_playback: false,
            }
        );

        for invalid in [
            ["tunr", "play", "--index", "0"].as_slice(),
            ["tunr", "play", "--index", "-1"].as_slice(),
            ["tunr", "play", "--index", "abc"].as_slice(),
        ] {
            let err = parse_cli_from(invalid.iter().copied()).unwrap_err();
            assert!(
                matches!(err, TunrError::InvalidArg(ref msg) if msg.contains("index must be a positive integer (>= 1)")),
                "unexpected error for {invalid:?}: {err:?}"
            );
        }

        for invalid in [
            ["tunr", "play", "--index", "3", "--artist", "Foo"].as_slice(),
            ["tunr", "play", "--index", "3", "Some Song"].as_slice(),
            ["tunr", "play", "--index", "3", "4"].as_slice(),
            ["tunr", "play", "--index", "3", "Some Song", "shuffle"].as_slice(),
        ] {
            assert!(
                matches!(
                    parse_cli_from(invalid.iter().copied()),
                    Err(TunrError::InvalidArg(_))
                ),
                "expected InvalidArg for {invalid:?}"
            );
        }
    }

    #[test]
    fn add_parses_flags_and_validates_playlist_and_paths() {
        assert_eq!(
            parse_cli_from(["tunr", "add", "song.mp3"]).unwrap(),
            Command::Add {
                input: AddInput::Path {
                    path: PathBuf::from("song.mp3"),
                    folder: false,
                },
                playlist_name: None,
            }
        );
        assert_eq!(
            parse_cli_from(["tunr", "add", "-f", "./music"]).unwrap(),
            Command::Add {
                input: AddInput::Path {
                    path: PathBuf::from("./music"),
                    folder: true,
                },
                playlist_name: None,
            }
        );
        assert_eq!(
            parse_cli_from(["tunr", "add", "-p", "Favorites", "song.mp3"]).unwrap(),
            Command::Add {
                input: AddInput::Path {
                    path: PathBuf::from("song.mp3"),
                    folder: false,
                },
                playlist_name: Some("Favorites".to_string()),
            }
        );
        assert_eq!(
            parse_cli_from(["tunr", "add", "song.mp3", "-p", "  Favorites  "]).unwrap(),
            Command::Add {
                input: AddInput::Path {
                    path: PathBuf::from("song.mp3"),
                    folder: false,
                },
                playlist_name: Some("Favorites".to_string()),
            }
        );
        assert_eq!(
            parse_cli_from(["tunr", "add", "-fp", "Roadtrip", "./music"]).unwrap(),
            Command::Add {
                input: AddInput::Path {
                    path: PathBuf::from("./music"),
                    folder: true,
                },
                playlist_name: Some("Roadtrip".to_string()),
            }
        );
        assert_eq!(
            parse_cli_from([
                "tunr",
                "add",
                "--folder",
                "./music",
                "--playlist",
                "Roadtrip",
            ])
            .unwrap(),
            Command::Add {
                input: AddInput::Path {
                    path: PathBuf::from("./music"),
                    folder: true,
                },
                playlist_name: Some("Roadtrip".to_string()),
            }
        );
        assert_eq!(
            parse_cli_from(["tunr", "add", "-s", "song.mp3"]).unwrap(),
            Command::Add {
                input: AddInput::Song("song.mp3".to_string()),
                playlist_name: None,
            }
        );
        assert_eq!(
            parse_cli_from(["tunr", "add", "-s", "  My Song  ", "-p", "Favorites"]).unwrap(),
            Command::Add {
                input: AddInput::Song("My Song".to_string()),
                playlist_name: Some("Favorites".to_string()),
            }
        );
        assert_eq!(
            parse_cli_from(["tunr", "add", "--playlist", " Roadtrip ", "--song", "1"]).unwrap(),
            Command::Add {
                input: AddInput::Song("1".to_string()),
                playlist_name: Some("Roadtrip".to_string()),
            }
        );

        for valid_existing in [
            ["tunr", "add", "-p", "-e", "Favorites", "Mundo"].as_slice(),
            ["tunr", "add", "-pe", "Favorites", "Mundo"].as_slice(),
            ["tunr", "add", "-ep", "Favorites", "Mundo"].as_slice(),
            ["tunr", "add", "-e", "-p", "Favorites", "Mundo"].as_slice(),
            [
                "tunr",
                "add",
                "--playlist",
                "--existing",
                "Favorites",
                "Mundo",
            ]
            .as_slice(),
            ["tunr", "add", "-p", "Favorites", "-e", "Mundo"].as_slice(),
            ["tunr", "add", "-e", "Mundo", "-p", "Favorites"].as_slice(),
            ["tunr", "add", "-e", "-s", "Mundo", "-p", "Favorites"].as_slice(),
        ] {
            assert_eq!(
                parse_cli_from(valid_existing.iter().copied()).unwrap(),
                Command::Add {
                    input: AddInput::ExistingSong("Mundo".to_string()),
                    playlist_name: Some("Favorites".to_string()),
                },
                "failed for {valid_existing:?}"
            );
        }
        assert_eq!(
            parse_cli_from(["tunr", "add", "-p", "-e", "1", "5"]).unwrap(),
            Command::Add {
                input: AddInput::ExistingSong("5".to_string()),
                playlist_name: Some("1".to_string()),
            }
        );

        for invalid in [
            ["tunr", "add"].as_slice(),
            ["tunr", "add", "-f"].as_slice(),
            ["tunr", "add", "-p", "Favorites"].as_slice(),
            ["tunr", "add", "-e", "Mundo"].as_slice(),
            ["tunr", "add", "-p", "Favorites", "-e"].as_slice(),
            ["tunr", "add", "-p", "-e"].as_slice(),
            ["tunr", "add", "-p", "-e", "", "Mundo"].as_slice(),
            ["tunr", "add", "-p", "-e", "Favorites", "   "].as_slice(),
            ["tunr", "add", "-f", "-e", "-p", "Favorites", "Mundo"].as_slice(),
            ["tunr", "add", "-p", "-e", "1", "5", "extra"].as_slice(),
            ["tunr", "add", "-s", ""].as_slice(),
            ["tunr", "add", "-s", "   "].as_slice(),
            ["tunr", "add", "-s", "song.mp3", "-p", ""].as_slice(),
            ["tunr", "add", "-s", "song.mp3", "-p", "   "].as_slice(),
            ["tunr", "add", "song.mp3", "-p", "   "].as_slice(),
            ["tunr", "add", ""].as_slice(),
            ["tunr", "add", "   "].as_slice(),
            ["tunr", "add", "-f", "-s", "song.mp3"].as_slice(),
            ["tunr", "add", "song.mp3", "-s", "other.mp3"].as_slice(),
            ["tunr", "add", "-s", "other.mp3", "song.mp3"].as_slice(),
            ["tunr", "add", "song.mp3", "Favorites"].as_slice(),
            ["tunr", "add", "-p", "Favorites", "song.mp3", "Extra"].as_slice(),
        ] {
            assert!(
                matches!(
                    parse_cli_from(invalid.iter().copied()),
                    Err(TunrError::InvalidArg(_))
                ),
                "expected InvalidArg for {invalid:?}"
            );
        }

        // Filesystem validation for -f vs non--f
        let tmp = std::env::temp_dir().join(format!("tunr-cli-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&tmp).unwrap();
        let file = tmp.join("sample.wav");
        fs::write(&file, b"RIFF").unwrap();

        assert!(validate_add_path(&file, false).is_ok());
        assert!(validate_add_path(&tmp, true).is_ok());
        assert!(matches!(
            validate_add_path(&tmp, false),
            Err(TunrError::InvalidArg(_))
        ));
        assert!(matches!(
            validate_add_path(&file, true),
            Err(TunrError::InvalidArg(_))
        ));
        assert!(matches!(
            validate_add_path(&tmp.join("missing.wav"), false),
            Err(TunrError::NotFound(_))
        ));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn transport_and_seek_commands_parse_and_validate() {
        assert_eq!(parse_cli_from(["tunr", "next"]).unwrap(), Command::Next);
        assert_eq!(parse_cli_from(["tunr", "prev"]).unwrap(), Command::Prev);
        assert_eq!(
            parse_cli_from(["tunr", "rewind"]).unwrap(),
            Command::Rewind { seconds: None }
        );
        assert_eq!(
            parse_cli_from(["tunr", "rewind", "5.5"]).unwrap(),
            Command::Rewind { seconds: Some(5.5) }
        );
        assert_eq!(
            parse_cli_from(["tunr", "rewind", "0"]).unwrap(),
            Command::Rewind { seconds: Some(0.0) }
        );
        assert_eq!(
            parse_cli_from(["tunr", "advance", "12"]).unwrap(),
            Command::Advance { seconds: 12.0 }
        );

        assert!(parse_cli_from(["tunr", "rewind", "-1"]).is_err());
        assert!(parse_cli_from(["tunr", "advance", "-0.5"]).is_err());
        assert!(parse_cli_from(["tunr", "rewind", "NaN"]).is_err());
        assert!(parse_cli_from(["tunr", "advance", "inf"]).is_err());
    }

    #[test]
    fn delete_parses_with_optional_disambiguation() {
        assert_eq!(
            parse_cli_from(["tunr", "delete", "Chill"]).unwrap(),
            Command::Delete {
                target: "Chill".to_string(),
                kind: TargetKind::Auto,
            }
        );
        assert_eq!(
            parse_cli_from(["tunr", "delete", "Chill", "--song"]).unwrap(),
            Command::Delete {
                target: "Chill".to_string(),
                kind: TargetKind::Song,
            }
        );
        assert_eq!(
            parse_cli_from(["tunr", "delete", "-s", "Chill"]).unwrap(),
            Command::Delete {
                target: "Chill".to_string(),
                kind: TargetKind::Song,
            }
        );
        assert_eq!(
            parse_cli_from(["tunr", "delete", "Chill", "--playlist"]).unwrap(),
            Command::Delete {
                target: "Chill".to_string(),
                kind: TargetKind::Playlist,
            }
        );
        assert_eq!(
            parse_cli_from(["tunr", "delete", "-p", "Chill"]).unwrap(),
            Command::Delete {
                target: "Chill".to_string(),
                kind: TargetKind::Playlist,
            }
        );
        assert_eq!(
            parse_cli_from(["tunr", "delete", "-p", "MyList[2]"]).unwrap(),
            Command::Delete {
                target: "MyList[2]".to_string(),
                kind: TargetKind::Playlist,
            }
        );
        assert_eq!(
            parse_cli_from(["tunr", "delete", "MyList[2]"]).unwrap(),
            Command::Delete {
                target: "MyList[2]".to_string(),
                kind: TargetKind::Auto,
            }
        );

        for invalid in [
            ["tunr", "delete", "Chill", "--song", "--playlist"].as_slice(),
            ["tunr", "delete", "-s", "-p", "Chill"].as_slice(),
            ["tunr", "delete", ""].as_slice(),
            ["tunr", "delete", "   "].as_slice(),
            ["tunr", "delete", "-p"].as_slice(),
        ] {
            assert!(
                matches!(
                    parse_cli_from(invalid.iter().copied()),
                    Err(TunrError::InvalidArg(_))
                ),
                "expected InvalidArg for {invalid:?}"
            );
        }
    }

    #[test]
    fn create_supports_quoted_and_unquoted_songs_kv() {
        assert_eq!(
            parse_cli_from(["tunr", "create", "Mix", "songs=Alpha,Beta"]).unwrap(),
            Command::Create {
                playlist: "Mix".to_string(),
                songs: vec!["Alpha".to_string(), "Beta".to_string()],
            }
        );
        assert_eq!(
            parse_cli_from(["tunr", "create", "Mix", "songs=Song One, Song Two"]).unwrap(),
            Command::Create {
                playlist: "Mix".to_string(),
                songs: vec!["Song One".to_string(), "Song Two".to_string()],
            }
        );
        assert_eq!(
            parse_cli_from([
                "tunr",
                "create",
                "Mix",
                "songs=Song",
                "One,",
                "Song",
                "Two"
            ])
            .unwrap(),
            Command::Create {
                playlist: "Mix".to_string(),
                songs: vec!["Song One".to_string(), "Song Two".to_string()],
            }
        );

        assert!(parse_cli_from(["tunr", "create", "Mix"]).is_err());
        assert!(parse_cli_from(["tunr", "create", "Mix", "songs="]).is_err());
        assert!(parse_cli_from(["tunr", "create", "Mix", "songs= , "]).is_err());
        assert!(parse_cli_from(["tunr", "create", "Mix", "tracks=A,B"]).is_err());
        assert!(parse_cli_from(["tunr", "create", "  ", "songs=A"]).is_err());
    }

    #[test]
    fn create_supports_escaped_commas_csv_quotes_and_multi_songs_kv() {
        assert_eq!(
            parse_cli_from([
                "tunr",
                "create",
                "Mix",
                r"songs=Sanctuary - Joji\, Justin Raisen,Circles",
            ])
            .unwrap(),
            Command::Create {
                playlist: "Mix".to_string(),
                songs: vec![
                    "Sanctuary - Joji, Justin Raisen".to_string(),
                    "Circles".to_string(),
                ],
            }
        );
        assert_eq!(
            parse_cli_from(["tunr", "create", "Mix", r"songs=Alpha\\,Beta"]).unwrap(),
            Command::Create {
                playlist: "Mix".to_string(),
                songs: vec![r"Alpha\".to_string(), "Beta".to_string()],
            }
        );
        assert_eq!(
            parse_cli_from(["tunr", "create", "Mix", r"songs=Alpha\\\,Beta"]).unwrap(),
            Command::Create {
                playlist: "Mix".to_string(),
                songs: vec![r"Alpha\,Beta".to_string()],
            }
        );
        assert_eq!(
            parse_cli_from([
                "tunr",
                "create",
                "Mix",
                r"songs=C:\Music\Track.mp3,Circles",
            ])
            .unwrap(),
            Command::Create {
                playlist: "Mix".to_string(),
                songs: vec![r"C:\Music\Track.mp3".to_string(), "Circles".to_string()],
            }
        );
        assert_eq!(
            parse_cli_from(["tunr", "create", "Mix", r#"songs="Alpha\\""#]).unwrap(),
            Command::Create {
                playlist: "Mix".to_string(),
                songs: vec![r"Alpha\".to_string()],
            }
        );
        assert_eq!(
            parse_cli_from(["tunr", "create", "Mix", r#"songs=\"Alpha,Beta\""#]).unwrap(),
            Command::Create {
                playlist: "Mix".to_string(),
                songs: vec![r#""Alpha"#.to_string(), r#"Beta""#.to_string()],
            }
        );

        // CSV-quoted items (including leading whitespace after comma and inner quotes)
        assert_eq!(
            parse_cli_from([
                "tunr",
                "create",
                "Mix",
                r#"songs="Sanctuary - Joji, Justin Raisen",Circles"#,
            ])
            .unwrap(),
            Command::Create {
                playlist: "Mix".to_string(),
                songs: vec![
                    "Sanctuary - Joji, Justin Raisen".to_string(),
                    "Circles".to_string(),
                ],
            }
        );
        assert_eq!(
            parse_cli_from([
                "tunr",
                "create",
                "Mix",
                r#"songs="Sanctuary - Joji, Justin Raisen", "Circles - Post Malone""#,
            ])
            .unwrap(),
            Command::Create {
                playlist: "Mix".to_string(),
                songs: vec![
                    "Sanctuary - Joji, Justin Raisen".to_string(),
                    "Circles - Post Malone".to_string(),
                ],
            }
        );
        assert_eq!(
            parse_cli_from([
                "tunr",
                "create",
                "Mix",
                r#"songs="Skrillex and Diplo - ""Where Are Ü Now"" with Justin Bieber",Circles"#,
            ])
            .unwrap(),
            Command::Create {
                playlist: "Mix".to_string(),
                songs: vec![
                    r#"Skrillex and Diplo - "Where Are Ü Now" with Justin Bieber"#.to_string(),
                    "Circles".to_string(),
                ],
            }
        );
        assert_eq!(
            parse_cli_from([
                "tunr",
                "create",
                "Mix",
                r#"songs="Skrillex and Diplo - \"Where Are Ü Now\" with Justin Bieber",Circles"#,
            ])
            .unwrap(),
            Command::Create {
                playlist: "Mix".to_string(),
                songs: vec![
                    r#"Skrillex and Diplo - "Where Are Ü Now" with Justin Bieber"#.to_string(),
                    "Circles".to_string(),
                ],
            }
        );

        // Unquoted fields with mid-title quotes remain literal
        assert_eq!(
            parse_cli_from([
                "tunr",
                "create",
                "Mix",
                r#"songs=Skrillex and Diplo - "Where Are Ü Now" with Justin Bieber (Official Video),Circles"#,
            ])
            .unwrap(),
            Command::Create {
                playlist: "Mix".to_string(),
                songs: vec![
                    r#"Skrillex and Diplo - "Where Are Ü Now" with Justin Bieber (Official Video)"#
                        .to_string(),
                    "Circles".to_string(),
                ],
            }
        );
        assert_eq!(
            parse_cli_from(["tunr", "create", "Mix", r#"songs=12" Remix,Circles"#]).unwrap(),
            Command::Create {
                playlist: "Mix".to_string(),
                songs: vec![r#"12" Remix"#.to_string(), "Circles".to_string()],
            }
        );

        // Multiple `songs=` tokens (quoted and unquoted multi-token) and literal `songs=` inside a title
        assert_eq!(
            parse_cli_from([
                "tunr",
                "create",
                "Mix",
                "songs=Mundo - IV Of Spades",
                "songs=After Dark - Mr.Kitty",
            ])
            .unwrap(),
            Command::Create {
                playlist: "Mix".to_string(),
                songs: vec![
                    "Mundo - IV Of Spades".to_string(),
                    "After Dark - Mr.Kitty".to_string(),
                ],
            }
        );
        assert_eq!(
            parse_cli_from([
                "tunr",
                "create",
                "Mix",
                "songs=Song",
                "One",
                "songs=Song",
                "Two,",
                "Song",
                "Three",
            ])
            .unwrap(),
            Command::Create {
                playlist: "Mix".to_string(),
                songs: vec![
                    "Song One".to_string(),
                    "Song Two".to_string(),
                    "Song Three".to_string(),
                ],
            }
        );
        assert_eq!(
            parse_cli_from(["tunr", "create", "Mix", "songs=My songs=cool,Circles"]).unwrap(),
            Command::Create {
                playlist: "Mix".to_string(),
                songs: vec!["My songs=cool".to_string(), "Circles".to_string()],
            }
        );
    }

    #[test]
    fn create_supports_repeatable_song_flag_and_interleaving() {
        assert_eq!(
            parse_cli_from([
                "tunr",
                "create",
                "Mix",
                "-s",
                "Sanctuary - Joji, Justin Raisen",
                "-s",
                "Circles - Post Malone",
            ])
            .unwrap(),
            Command::Create {
                playlist: "Mix".to_string(),
                songs: vec![
                    "Sanctuary - Joji, Justin Raisen".to_string(),
                    "Circles - Post Malone".to_string(),
                ],
            }
        );
        assert_eq!(
            parse_cli_from([
                "tunr",
                "create",
                "Mix",
                "--song",
                r#"Skrillex and Diplo - "Where Are Ü Now""#,
                "-s",
                "  After Dark  ",
            ])
            .unwrap(),
            Command::Create {
                playlist: "Mix".to_string(),
                songs: vec![
                    r#"Skrillex and Diplo - "Where Are Ü Now""#.to_string(),
                    "After Dark".to_string(),
                ],
            }
        );

        // Interleaving -s/--song with songs= preserves exact CLI order
        assert_eq!(
            parse_cli_from([
                "tunr",
                "create",
                "Mix",
                "songs=Alpha,Beta",
                "-s",
                "Gamma",
            ])
            .unwrap(),
            Command::Create {
                playlist: "Mix".to_string(),
                songs: vec!["Alpha".to_string(), "Beta".to_string(), "Gamma".to_string()],
            }
        );
        assert_eq!(
            parse_cli_from([
                "tunr",
                "create",
                "Mix",
                "-s",
                "First",
                "songs=Second,Third",
                "-s",
                "Fourth",
            ])
            .unwrap(),
            Command::Create {
                playlist: "Mix".to_string(),
                songs: vec![
                    "First".to_string(),
                    "Second".to_string(),
                    "Third".to_string(),
                    "Fourth".to_string(),
                ],
            }
        );
        assert_eq!(
            parse_cli_from([
                "tunr",
                "create",
                "-s",
                "First",
                "Mix",
                "songs=Second",
            ])
            .unwrap(),
            Command::Create {
                playlist: "Mix".to_string(),
                songs: vec!["First".to_string(), "Second".to_string()],
            }
        );

        // Bare '-' and hyphen-leading song titles via attached '=' or '--'
        assert_eq!(
            parse_cli_from([
                "tunr",
                "create",
                "New-List",
                "songs=Mundo",
                "-",
                "IV",
                "Of",
                "Spades,",
                "After",
                "Dark",
                "-",
                "Mr.Kitty",
            ])
            .unwrap(),
            Command::Create {
                playlist: "New-List".to_string(),
                songs: vec![
                    "Mundo - IV Of Spades".to_string(),
                    "After Dark - Mr.Kitty".to_string(),
                ],
            }
        );
        assert_eq!(
            parse_cli_from(["tunr", "create", "Mix", "-s=-Track", "--song=-Other"]).unwrap(),
            Command::Create {
                playlist: "Mix".to_string(),
                songs: vec!["-Track".to_string(), "-Other".to_string()],
            }
        );
        assert_eq!(
            parse_cli_from(["tunr", "create", "Mix", "-s", "-"]).unwrap(),
            Command::Create {
                playlist: "Mix".to_string(),
                songs: vec!["-".to_string()],
            }
        );
        assert_eq!(
            parse_cli_from(["tunr", "create", "Mix", "--", "songs=-Track"]).unwrap(),
            Command::Create {
                playlist: "Mix".to_string(),
                songs: vec!["-Track".to_string()],
            }
        );
        assert_eq!(
            parse_cli_from(["tunr", "create", "Mix", "-s", "Alpha", "--", "songs=-Beta"])
                .unwrap(),
            Command::Create {
                playlist: "Mix".to_string(),
                songs: vec!["Alpha".to_string(), "-Beta".to_string()],
            }
        );
    }

    #[test]
    fn create_rejects_malformed_quotes_unknown_flags_and_invalid_args() {
        for invalid in [
            ["tunr", "create", "Mix", r#"songs="""#].as_slice(),
            ["tunr", "create", "Mix", r#"songs="",Alpha"#].as_slice(),
            ["tunr", "create", "Mix", r#"songs=Alpha,"""#].as_slice(),
            ["tunr", "create", "Mix", "songs=Alpha", "songs="].as_slice(),
            ["tunr", "create", "Mix", "-s", ""].as_slice(),
            ["tunr", "create", "Mix", "-s", "   "].as_slice(),
            ["tunr", "create", "Mix", r#"songs="Unclosed"#].as_slice(),
            ["tunr", "create", "Mix", r#"songs=Alpha, "Unclosed"#].as_slice(),
            ["tunr", "create", "Mix", r#"songs="Alpha\""#].as_slice(),
            ["tunr", "create", "Mix", r#"songs="Alpha"Beta"#].as_slice(),
            ["tunr", "create", "Mix", r#"songs="Alpha" "Beta""#].as_slice(),
            ["tunr", "create", "Mix", "--unknown"].as_slice(),
            ["tunr", "create", "Mix", "-x", "Alpha"].as_slice(),
            ["tunr", "create", "Mix", "-s", "Alpha", "--unknown"].as_slice(),
            ["tunr", "create", "Mix", "songs=Alpha", "--unknown"].as_slice(),
            ["tunr", "create", "Mix", "-s", "--unknown"].as_slice(),
            ["tunr", "create", "Mix", "-s", "-s", "Alpha"].as_slice(),
            ["tunr", "create", "Mix", "Alpha"].as_slice(),
            ["tunr", "create", "Mix", "-s", "Alpha", "Beta"].as_slice(),
            ["tunr", "create", "Mix", "songs=Alpha", "-s", "Beta", "Gamma"].as_slice(),
            ["tunr", "create", "Mix", "--", "-Track"].as_slice(),
            ["tunr", "create", "Mix", "-s", "Alpha", "--", "-s", "Beta"].as_slice(),
        ] {
            assert!(
                matches!(
                    parse_cli_from(invalid.iter().copied()),
                    Err(TunrError::InvalidArg(_))
                ),
                "expected InvalidArg for {invalid:?}"
            );
        }
    }

    #[test]
    fn supplemental_and_hidden_commands_parse() {
        assert_eq!(parse_cli_from(["tunr", "pause"]).unwrap(), Command::Pause);
        assert_eq!(parse_cli_from(["tunr", "resume"]).unwrap(), Command::Resume);
        assert_eq!(parse_cli_from(["tunr", "stop"]).unwrap(), Command::Stop);
        assert_eq!(parse_cli_from(["tunr", "status"]).unwrap(), Command::Status);
        assert_eq!(
            parse_cli_from(["tunr", "list"]).unwrap(),
            Command::List { kind: None }
        );
        assert_eq!(
            parse_cli_from(["tunr", "list", "songs"]).unwrap(),
            Command::List {
                kind: Some(ListKind::Songs)
            }
        );
        assert_eq!(
            parse_cli_from(["tunr", "list", "playlists"]).unwrap(),
            Command::List {
                kind: Some(ListKind::Playlists)
            }
        );
        assert_eq!(
            parse_cli_from(["tunr", "daemon", "stop"]).unwrap(),
            Command::DaemonStop
        );
        assert_eq!(
            parse_cli_from(["tunr", "__daemon"]).unwrap(),
            Command::Daemon(DaemonOverrides::default())
        );

        let overrides = DaemonOverrides {
            library_path: Some(PathBuf::from("/tmp/data/library.json")),
            config_path: None,
            socket_path: Some("/tmp/run/custom.sock".to_string()),
            pid_path: Some(PathBuf::from("/tmp/run/custom.pid")),
            log_path: Some(PathBuf::from("/tmp/run/custom.log")),
        };
        let resolved = overrides.resolve_paths().unwrap();
        assert_eq!(resolved.library_path, PathBuf::from("/tmp/data/library.json"));
        assert_eq!(
            resolved.config_path,
            PathBuf::from("/tmp/data/config/config.json")
        );
        assert_eq!(resolved.pid_path, PathBuf::from("/tmp/run/custom.pid"));
        assert_eq!(resolved.log_path, PathBuf::from("/tmp/run/custom.log"));
        #[cfg(unix)]
        assert_eq!(
            resolved.ipc_endpoint,
            IpcEndpoint::UnixSocket(PathBuf::from("/tmp/run/custom.sock"))
        );
        #[cfg(windows)]
        assert!(matches!(
            resolved.ipc_endpoint,
            IpcEndpoint::NamedPipe(ref name) if name.starts_with(r"\\.\pipe\tunr-")
        ));
    }

    #[test]
    fn daemon_overrides_parses_and_resolves_config_path() {
        assert_eq!(
            parse_cli_from([
                "tunr",
                "__daemon",
                "--config-path",
                "/tmp/ws/config/config.json",
            ])
            .unwrap(),
            Command::Daemon(DaemonOverrides {
                config_path: Some(PathBuf::from("/tmp/ws/config/config.json")),
                ..DaemonOverrides::default()
            })
        );

        let cmd = parse_cli_from([
            "tunr",
            "__daemon",
            "--library-path",
            "/tmp/ws/playlists/library.json",
            "--config-path",
            "/tmp/ws/config/config.json",
            "--socket-path",
            "/tmp/run/custom.sock",
            "--pid-path",
            "/tmp/run/custom.pid",
            "--log-path",
            "/tmp/run/custom.log",
        ])
        .unwrap();

        let Command::Daemon(overrides) = cmd else {
            panic!("expected Command::Daemon");
        };
        let resolved = overrides.resolve_paths().unwrap();
        assert_eq!(
            resolved.library_path,
            PathBuf::from("/tmp/ws/playlists/library.json")
        );
        assert_eq!(
            resolved.config_path,
            PathBuf::from("/tmp/ws/config/config.json")
        );
    }

    #[test]
    fn volume_parses_optional_level_and_rejects_invalid_values() {
        assert_eq!(
            parse_cli_from(["tunr", "volume"]).unwrap(),
            Command::Volume { level: None }
        );
        assert_eq!(
            parse_cli_from(["tunr", "volume", "0"]).unwrap(),
            Command::Volume { level: Some(0) }
        );
        assert_eq!(
            parse_cli_from(["tunr", "volume", "50"]).unwrap(),
            Command::Volume { level: Some(50) }
        );
        assert_eq!(
            parse_cli_from(["tunr", "volume", "100"]).unwrap(),
            Command::Volume { level: Some(100) }
        );

        for invalid in ["-1", "101", "50.5", "abc"] {
            let err = parse_cli_from(["tunr", "volume", invalid]).unwrap_err();
            let expected = format!("volume must be an integer between 0 and 100, got '{invalid}'");
            assert!(
                matches!(err, TunrError::InvalidArg(ref msg) if msg.contains(&expected)),
                "unexpected error for `tunr volume {invalid}`: {err:?}"
            );
        }
    }

    #[test]
    fn init_parses_path_and_rejects_empty() {
        assert_eq!(
            parse_cli_from(["tunr", "init", "/tmp/my-music-ws"]).unwrap(),
            Command::Init {
                path: PathBuf::from("/tmp/my-music-ws")
            }
        );
        assert_eq!(
            parse_cli_from(["tunr", "init", "./workspace"]).unwrap(),
            Command::Init {
                path: PathBuf::from("./workspace")
            }
        );
        assert!(matches!(
            parse_cli_from(["tunr", "init", ""]),
            Err(TunrError::InvalidArg(_))
        ));
        assert!(matches!(
            parse_cli_from(["tunr", "init", "   "]),
            Err(TunrError::InvalidArg(_))
        ));
        assert!(parse_cli_from(["tunr", "init"]).is_err());
    }

    #[test]
    fn download_parses_options_and_validates_constraints() {
        assert_eq!(
            parse_cli_from(["tunr", "download", "https://youtu.be/dQw4w9WgXcQ"]).unwrap(),
            Command::Download(DownloadOptions {
                urls: vec!["https://youtu.be/dQw4w9WgXcQ".to_string()],
                targets: Vec::new(),
                dest: None,
                set_dest: None,
                format: DownloadFormat::Mp3,
                playlist: false,
                embed_thumbnail: true,
                auto_add: true,
                playlist_name: None,
                jobs: 4,
                no_tui: false,
            })
        );

        assert_eq!(
            parse_cli_from([
                "tunr",
                "download",
                "-d",
                "/tmp/music",
                "-f",
                "FLAC",
                "--playlist",
                "--no-embed-thumbnail",
                "-p",
                "Favorites",
                "--jobs",
                "8",
                "--no-tui",
                "https://example.com/list",
            ])
            .unwrap(),
            Command::Download(DownloadOptions {
                urls: vec!["https://example.com/list".to_string()],
                targets: Vec::new(),
                dest: Some(PathBuf::from("/tmp/music")),
                set_dest: None,
                format: DownloadFormat::Flac,
                playlist: true,
                embed_thumbnail: false,
                auto_add: true,
                playlist_name: Some("Favorites".to_string()),
                jobs: 8,
                no_tui: true,
            })
        );

        assert_eq!(
            parse_cli_from(["tunr", "download", "--set-dest", "/tmp/saved"]).unwrap(),
            Command::Download(DownloadOptions {
                urls: Vec::new(),
                targets: Vec::new(),
                dest: None,
                set_dest: Some(PathBuf::from("/tmp/saved")),
                format: DownloadFormat::Mp3,
                playlist: false,
                embed_thumbnail: true,
                auto_add: true,
                playlist_name: None,
                jobs: 4,
                no_tui: false,
            })
        );

        assert_eq!(
            parse_cli_from([
                "tunr",
                "download",
                "--set-dest",
                "/tmp/saved",
                "--no-add",
                "-j",
                "2",
                "https://example.com/a https://example.com/b",
                "https://example.com/a",
                "https://example.com/c",
            ])
            .unwrap(),
            Command::Download(DownloadOptions {
                urls: vec![
                    "https://example.com/a".to_string(),
                    "https://example.com/b".to_string(),
                    "https://example.com/c".to_string(),
                ],
                targets: Vec::new(),
                dest: None,
                set_dest: Some(PathBuf::from("/tmp/saved")),
                format: DownloadFormat::Mp3,
                playlist: false,
                embed_thumbnail: true,
                auto_add: false,
                playlist_name: None,
                jobs: 2,
                no_tui: false,
            })
        );

        // Missing URL without --set-dest fails.
        assert!(matches!(
            parse_cli_from(["tunr", "download"]),
            Err(TunrError::InvalidArg(_))
        ));

        // --jobs 0 fails.
        assert!(matches!(
            parse_cli_from(["tunr", "download", "-j", "0", "https://example.com/a"]),
            Err(TunrError::InvalidArg(_))
        ));
        assert!(matches!(
            parse_cli_from([
                "tunr",
                "download",
                "--jobs",
                "0",
                "https://example.com/a"
            ]),
            Err(TunrError::InvalidArg(_))
        ));

        // Empty URL arg fails.
        assert!(matches!(
            parse_cli_from(["tunr", "download", "   "]),
            Err(TunrError::InvalidArg(_))
        ));

        // Conflicting -p / --playlist-name and --no-add fails.
        assert!(
            parse_cli_from([
                "tunr",
                "download",
                "-p",
                "MyList",
                "--no-add",
                "https://example.com/a",
            ])
            .is_err()
        );

        // Empty/whitespace -d or --set-dest or -p or URL fails.
        assert!(matches!(
            parse_cli_from(["tunr", "download", "-d", "", "https://example.com/a"]),
            Err(TunrError::InvalidArg(_))
        ));
        assert!(matches!(
            parse_cli_from(["tunr", "download", "--dest=   ", "https://example.com/a"]),
            Err(TunrError::InvalidArg(_))
        ));
        assert!(matches!(
            parse_cli_from(["tunr", "download", "--set-dest", ""]),
            Err(TunrError::InvalidArg(_))
        ));
        assert!(matches!(
            parse_cli_from(["tunr", "download", "--set-dest=  "]),
            Err(TunrError::InvalidArg(_))
        ));
        assert!(matches!(
            parse_cli_from(["tunr", "download", "-p", "  ", "https://example.com/a"]),
            Err(TunrError::InvalidArg(_))
        ));
    }

    #[test]
    fn download_parses_link_files_and_mixed_targets() {
        // Positional .link file (disk-free: path does not need to exist at parse time).
        assert_eq!(
            parse_cli_from(["tunr", "download", "test-links/test.link"]).unwrap(),
            Command::Download(DownloadOptions {
                urls: Vec::new(),
                targets: vec![DownloadTarget::LinkFile(PathBuf::from(
                    "test-links/test.link"
                ))],
                dest: None,
                set_dest: None,
                format: DownloadFormat::Mp3,
                playlist: false,
                embed_thumbnail: true,
                auto_add: true,
                playlist_name: None,
                jobs: 4,
                no_tui: false,
            })
        );

        // .link paths with internal spaces and surrounding whitespace are trimmed and preserved intact.
        assert_eq!(
            parse_cli_from(["tunr", "download", "  my links/chill mix.LINK  "]).unwrap(),
            Command::Download(DownloadOptions {
                urls: Vec::new(),
                targets: vec![DownloadTarget::LinkFile(PathBuf::from(
                    "my links/chill mix.LINK"
                ))],
                dest: None,
                set_dest: None,
                format: DownloadFormat::Mp3,
                playlist: false,
                embed_thumbnail: true,
                auto_add: true,
                playlist_name: None,
                jobs: 4,
                no_tui: false,
            })
        );

        // Mixed .link files and URLs preserve positional order and deduplicate URLs.
        assert_eq!(
            parse_cli_from([
                "tunr",
                "download",
                "first.link",
                "https://example.com/a https://example.com/b",
                "my lists/second file.link",
                "https://example.com/a",
                "https://example.com/c",
            ])
            .unwrap(),
            Command::Download(DownloadOptions {
                urls: Vec::new(),
                targets: vec![
                    DownloadTarget::LinkFile(PathBuf::from("first.link")),
                    DownloadTarget::Url("https://example.com/a".to_string()),
                    DownloadTarget::Url("https://example.com/b".to_string()),
                    DownloadTarget::LinkFile(PathBuf::from("my lists/second file.link")),
                    DownloadTarget::Url("https://example.com/c".to_string()),
                ],
                dest: None,
                set_dest: None,
                format: DownloadFormat::Mp3,
                playlist: false,
                embed_thumbnail: true,
                auto_add: true,
                playlist_name: None,
                jobs: 4,
                no_tui: false,
            })
        );

        // URL ending in `.link` is still parsed as a raw URL, not a local .link file.
        assert_eq!(
            parse_cli_from(["tunr", "download", "https://example.com/foo.link"]).unwrap(),
            Command::Download(DownloadOptions {
                urls: vec!["https://example.com/foo.link".to_string()],
                targets: Vec::new(),
                dest: None,
                set_dest: None,
                format: DownloadFormat::Mp3,
                playlist: false,
                embed_thumbnail: true,
                auto_add: true,
                playlist_name: None,
                jobs: 4,
                no_tui: false,
            })
        );

        // `.link` file combined with `--set-dest`.
        assert_eq!(
            parse_cli_from([
                "tunr",
                "download",
                "--set-dest",
                "/tmp/saved",
                "my lists/mix.link",
            ])
            .unwrap(),
            Command::Download(DownloadOptions {
                urls: Vec::new(),
                targets: vec![DownloadTarget::LinkFile(PathBuf::from("my lists/mix.link"))],
                dest: None,
                set_dest: Some(PathBuf::from("/tmp/saved")),
                format: DownloadFormat::Mp3,
                playlist: false,
                embed_thumbnail: true,
                auto_add: true,
                playlist_name: None,
                jobs: 4,
                no_tui: false,
            })
        );

        // Empty/whitespace argument alongside a `.link` file is still rejected.
        assert!(matches!(
            parse_cli_from(["tunr", "download", "a.link", "   "]),
            Err(TunrError::InvalidArg(_))
        ));
    }

    #[test]
    fn search_parses_options_and_validates_constraints() {
        assert_eq!(
            parse_cli_from(["tunr", "search"]).unwrap(),
            Command::Search(SearchOptions {
                query: String::new(),
                limit: None,
                set_limit: None,
                no_tui: false,
                download: false,
                dest: None,
                provider: None,
                provider_default: None,
                format: None,
                format_default: None,
            })
        );

        assert_eq!(
            parse_cli_from(["tunr", "search", "   "]).unwrap(),
            Command::Search(SearchOptions {
                query: String::new(),
                limit: None,
                set_limit: None,
                no_tui: false,
                download: false,
                dest: None,
                provider: None,
                provider_default: None,
                format: None,
                format_default: None,
            })
        );

        assert_eq!(
            parse_cli_from(["tunr", "search", "-n", "15", "lofi", "hip", "hop"]).unwrap(),
            Command::Search(SearchOptions {
                query: "lofi hip hop".to_string(),
                limit: Some(15),
                set_limit: None,
                no_tui: false,
                download: false,
                dest: None,
                provider: None,
                provider_default: None,
                format: None,
                format_default: None,
            })
        );

        assert_eq!(
            parse_cli_from([
                "tunr",
                "search",
                "--limit",
                "25",
                "--set-limit",
                "20",
                "--no-tui",
                "synthwave",
            ])
            .unwrap(),
            Command::Search(SearchOptions {
                query: "synthwave".to_string(),
                limit: Some(25),
                set_limit: Some(20),
                no_tui: true,
                download: false,
                dest: None,
                provider: None,
                provider_default: None,
                format: None,
                format_default: None,
            })
        );

        assert_eq!(
            parse_cli_from(["tunr", "search", "--set-limit", "30"]).unwrap(),
            Command::Search(SearchOptions {
                query: String::new(),
                limit: None,
                set_limit: Some(30),
                no_tui: false,
                download: false,
                dest: None,
                provider: None,
                provider_default: None,
                format: None,
                format_default: None,
            })
        );

        assert_eq!(
            parse_cli_from(["tunr", "search", "--set-limit", "30", "--no-tui"]).unwrap(),
            Command::Search(SearchOptions {
                query: String::new(),
                limit: None,
                set_limit: Some(30),
                no_tui: true,
                download: false,
                dest: None,
                provider: None,
                provider_default: None,
                format: None,
                format_default: None,
            })
        );

        // -d/--download, --dest, -p/--provider, -f/--format (with and without leading '.').
        assert_eq!(
            parse_cli_from([
                "tunr",
                "search",
                "-d",
                "--dest",
                "/tmp/dl",
                "-p",
                "https://soundcloud.com/",
                "-f",
                ".mp4",
                "chill",
                "beats",
            ])
            .unwrap(),
            Command::Search(SearchOptions {
                query: "chill beats".to_string(),
                limit: None,
                set_limit: None,
                no_tui: false,
                download: true,
                dest: Some(PathBuf::from("/tmp/dl")),
                provider: Some(SearchProvider::SoundCloud),
                provider_default: None,
                format: Some(DownloadFormat::Mp4),
                format_default: None,
            })
        );

        assert_eq!(
            parse_cli_from([
                "tunr",
                "search",
                "--download",
                "--provider",
                "https://youtu.be/xyz",
                "--format",
                "mp3",
                "--no-tui",
                "ambient",
            ])
            .unwrap(),
            Command::Search(SearchOptions {
                query: "ambient".to_string(),
                limit: None,
                set_limit: None,
                no_tui: true,
                download: true,
                dest: None,
                provider: Some(SearchProvider::YouTube),
                provider_default: None,
                format: Some(DownloadFormat::Mp3),
                format_default: None,
            })
        );

        // Default-setting flags: -pd, --pd, --provider-default, -fd, --fd, -format-default, --format-default.
        assert_eq!(
            parse_cli_from(["tunr", "search", "-pd", "soundcloud"]).unwrap(),
            Command::Search(SearchOptions {
                query: String::new(),
                limit: None,
                set_limit: None,
                no_tui: false,
                download: false,
                dest: None,
                provider: None,
                provider_default: Some(SearchProvider::SoundCloud),
                format: None,
                format_default: None,
            })
        );
        assert_eq!(
            parse_cli_from(["tunr", "search", "--pd=youtube", "-fd", ".mp4"]).unwrap(),
            Command::Search(SearchOptions {
                query: String::new(),
                limit: None,
                set_limit: None,
                no_tui: false,
                download: false,
                dest: None,
                provider: None,
                provider_default: Some(SearchProvider::YouTube),
                format: None,
                format_default: Some(DownloadFormat::Mp4),
            })
        );
        assert_eq!(
            parse_cli_from(["tunr", "search", "-format-default", ".mp3", "--no-tui"]).unwrap(),
            Command::Search(SearchOptions {
                query: String::new(),
                limit: None,
                set_limit: None,
                no_tui: true,
                download: false,
                dest: None,
                provider: None,
                provider_default: None,
                format: None,
                format_default: Some(DownloadFormat::Mp3),
            })
        );
        assert_eq!(
            parse_cli_from([
                "tunr",
                "search",
                "--provider-default",
                "soundcloud.com",
                "--format-default=.flac",
            ])
            .unwrap(),
            Command::Search(SearchOptions {
                query: String::new(),
                limit: None,
                set_limit: None,
                no_tui: false,
                download: false,
                dest: None,
                provider: None,
                provider_default: Some(SearchProvider::SoundCloud),
                format: None,
                format_default: Some(DownloadFormat::Flac),
            })
        );

        // `--` passthrough keeps literal `-pd` / `-fd` as query text.
        assert_eq!(
            parse_cli_from(["tunr", "search", "--no-tui", "--", "-pd", "-fd"]).unwrap(),
            Command::Search(SearchOptions {
                query: "-pd -fd".to_string(),
                limit: None,
                set_limit: None,
                no_tui: true,
                download: false,
                dest: None,
                provider: None,
                provider_default: None,
                format: None,
                format_default: None,
            })
        );

        // `download -f` also accepts `.mp4` / `mp4` / `.mp3`.
        assert_eq!(
            parse_cli_from(["tunr", "download", "-f", ".mp4", "https://youtu.be/abc"]).unwrap(),
            Command::Download(DownloadOptions {
                urls: vec!["https://youtu.be/abc".to_string()],
                targets: Vec::new(),
                dest: None,
                set_dest: None,
                format: DownloadFormat::Mp4,
                playlist: false,
                embed_thumbnail: true,
                auto_add: true,
                playlist_name: None,
                jobs: 4,
                no_tui: false,
            })
        );

        // Empty/whitespace query with --no-tui and without default-setting flags fails.
        assert!(matches!(
            parse_cli_from(["tunr", "search", "--no-tui"]),
            Err(TunrError::InvalidArg(_))
        ));
        assert!(matches!(
            parse_cli_from(["tunr", "search", "--no-tui", "   "]),
            Err(TunrError::InvalidArg(_))
        ));
        assert!(matches!(
            parse_cli_from(["tunr", "search", "--no-tui", "-d", "--set-limit", "5"]),
            Err(TunrError::InvalidArg(_))
        ));

        // Empty --dest, unsupported provider, and unsupported format fail.
        assert!(matches!(
            parse_cli_from(["tunr", "search", "--dest", "", "jazz"]),
            Err(TunrError::InvalidArg(_))
        ));
        assert!(matches!(
            parse_cli_from(["tunr", "search", "-p", "https://example.com", "jazz"]),
            Err(TunrError::InvalidArg(_))
        ));
        assert!(matches!(
            parse_cli_from(["tunr", "search", "-f", ".avi", "jazz"]),
            Err(TunrError::InvalidArg(_))
        ));

        // Bounds validation (1..=100) for --limit / -n and --set-limit.
        assert!(matches!(
            parse_cli_from(["tunr", "search", "-n", "0", "jazz"]),
            Err(TunrError::InvalidArg(_))
        ));
        assert!(matches!(
            parse_cli_from(["tunr", "search", "--limit", "101", "jazz"]),
            Err(TunrError::InvalidArg(_))
        ));
        assert!(matches!(
            parse_cli_from(["tunr", "search", "--set-limit", "0"]),
            Err(TunrError::InvalidArg(_))
        ));
        assert!(matches!(
            parse_cli_from(["tunr", "search", "--set-limit", "101"]),
            Err(TunrError::InvalidArg(_))
        ));
    }

    #[test]
    fn update_command_parses_without_args_and_rejects_extra_args() {
        assert_eq!(
            parse_cli_from(["tunr", "update"]).unwrap(),
            Command::Update
        );
        assert!(matches!(
            parse_cli_from(["tunr", "update", "extra"]),
            Err(TunrError::InvalidArg(_))
        ));
    }
}
