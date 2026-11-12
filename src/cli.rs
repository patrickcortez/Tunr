use std::ffi::OsString;
use std::path::{Path, PathBuf};

use clap::error::ErrorKind;
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};

use crate::download::{DownloadFormat, DownloadOptions, normalize_urls, parse_download_format_str};
use crate::error::{Result, TermusicError};
use crate::ipc::{PlayMode, TargetKind};
use crate::paths::{self, AppPaths, IpcEndpoint};
use crate::search::{SearchOptions, SearchProvider};

/// Sub-target filter for `termusic list`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ListKind {
    Songs,
    Playlists,
}

/// Optional path overrides accepted by the hidden `__daemon` subcommand.
#[derive(Debug, Clone, PartialEq, Eq, Args, Default)]
pub struct DaemonOverrides {
    #[arg(long, hide = true)]
    pub library_path: Option<PathBuf>,
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
            paths.ipc_endpoint = IpcEndpoint::from_override(socket_path)?;
            paths.pid_path = pid_path.clone();
            paths.log_path = log_path.clone();
            return Ok(paths);
        }

        let mut paths = AppPaths::resolve()?;
        if let Some(p) = &self.library_path {
            paths.library_path = p.clone();
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
    name = "termusic",
    version = env!("CARGO_PKG_VERSION"),
    about = "Command-line music player with interactive TUI"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<RawCommand>,
}

#[derive(Debug, Subcommand)]
pub enum RawCommand {
    /// Initialize a Termusic working directory before using other commands
    Init {
        /// Working directory location for Termusic config, playlists, and music
        path: PathBuf,
    },

    /// Launch interactive TUI mode (also default when no subcommand is given)
    Tui,

    /// Print version information
    Version,

    /// Play a song or playlist, or resume paused playback when omitted
    Play {
        /// Song title or playlist name (omit to resume currently paused playback)
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
    },

    /// Add an audio file or folder of audio files to the library
    Add {
        /// Recursively scan a folder for supported audio files
        #[arg(short = 'f', long = "folder")]
        folder: bool,
        /// Add imported song(s) to the named playlist (creating it if absent)
        #[arg(short = 'p', long = "playlist")]
        playlist: bool,
        /// Path to audio file (or directory when -f is set)
        path: PathBuf,
        /// Target playlist name (required when -p is set)
        playlist_name: Option<String>,
    },

    /// Skip to the next track
    Next,

    /// Return to the previous track
    Prev,

    /// Rewind playback by <seconds>
    Rewind {
        #[arg(allow_negative_numbers = true, value_parser = parse_non_negative_secs)]
        seconds: f64,
    },

    /// Advance playback by <seconds>
    Advance {
        #[arg(allow_negative_numbers = true, value_parser = parse_non_negative_secs)]
        seconds: f64,
    },

    /// Remove a song or playlist from the library index (never deletes audio files on disk)
    Delete {
        /// Song title or playlist name
        target: String,
        /// Force deleting a song
        #[arg(long, conflicts_with = "playlist")]
        song: bool,
        /// Force deleting a playlist
        #[arg(long, conflicts_with = "song")]
        playlist: bool,
    },

    /// Create a playlist from existing library songs: create <name> songs=<s1,s2,...>
    Create {
        /// Name of the new playlist
        playlist: String,
        /// Comma-separated song list prefixed with `songs=`
        #[arg(
            required = true,
            num_args = 1..,
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "songs=<s1,s2,...>"
        )]
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
        /// One or more media URLs to download
        #[arg(value_name = "URL")]
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

/// Subcommands for `termusic daemon`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Subcommand)]
pub enum DaemonAction {
    /// Gracefully stop the running background playback daemon
    Stop,
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
        target: String,
        mode: PlayMode,
        kind: TargetKind,
        artist: Option<String>,
    },
    Add {
        folder: bool,
        playlist: bool,
        path: PathBuf,
        playlist_name: Option<String>,
    },
    Next,
    Prev,
    Rewind {
        seconds: f64,
    },
    Advance {
        seconds: f64,
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
    Download(DownloadOptions),
    Search(SearchOptions),
    DaemonStop,
    Daemon(DaemonOverrides),
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

/// Parse `songs=<s1,s2,...>` from trailing CLI tokens (supporting both quoted and unquoted forms).
pub fn parse_songs_kv(tokens: &[String]) -> Result<Vec<String>> {
    let joined = tokens.join(" ");
    let rhs = joined.strip_prefix("songs=").ok_or_else(|| {
        TermusicError::InvalidArg("expected `songs=<song1,song2,...>` argument".to_string())
    })?;
    let titles: Vec<String> = rhs
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .collect();
    if titles.is_empty() {
        return Err(TermusicError::InvalidArg(
            "songs list cannot be empty".to_string(),
        ));
    }
    Ok(titles)
}

/// Validate filesystem preconditions for `termusic add`:
/// `-f` requires an existing directory; without `-f` requires an existing regular file.
pub fn validate_add_path(path: &Path, folder: bool) -> Result<()> {
    if !path.exists() {
        return Err(TermusicError::NotFound(format!(
            "path does not exist: {}",
            path.display()
        )));
    }
    if folder && !path.is_dir() {
        return Err(TermusicError::InvalidArg(format!(
            "`-f` requires a directory path: {}",
            path.display()
        )));
    }
    if !folder && !path.is_file() {
        return Err(TermusicError::InvalidArg(format!(
            "adding without `-f` requires a file path (use `-f` for folders): {}",
            path.display()
        )));
    }
    Ok(())
}

fn resolve_target_kind(song: bool, playlist: bool) -> Result<TargetKind> {
    match (song, playlist) {
        (true, true) => Err(TermusicError::InvalidArg(
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
                return Err(TermusicError::InvalidArg(
                    "working directory path cannot be empty".to_string(),
                ));
            }
            Ok(Command::Init { path })
        }
        RawCommand::Tui => Ok(Command::Tui),
        RawCommand::Version => Ok(Command::Version),
        RawCommand::Play {
            target,
            mode,
            song,
            playlist,
            artist,
        } => match target {
            None => {
                if song || playlist || mode.is_some() || artist.is_some() {
                    return Err(TermusicError::InvalidArg(
                        "play requires a song or playlist name when flags or play mode are specified"
                            .to_string(),
                    ));
                }
                Ok(Command::Resume)
            }
            Some(target) => {
                if target.trim().is_empty() {
                    return Err(TermusicError::InvalidArg(
                        "play target cannot be empty".to_string(),
                    ));
                }
                let clean_artist = match artist {
                    Some(raw) => {
                        let trimmed = raw.trim();
                        if trimmed.is_empty() {
                            return Err(TermusicError::InvalidArg(
                                "--artist cannot be empty".to_string(),
                            ));
                        }
                        Some(trimmed.to_string())
                    }
                    None => None,
                };
                Ok(Command::Play {
                    target,
                    mode: mode.unwrap_or_default(),
                    kind: resolve_target_kind(song, playlist)?,
                    artist: clean_artist,
                })
            }
        },
        RawCommand::Add {
            folder,
            playlist,
            path,
            playlist_name,
        } => {
            if playlist {
                let Some(name) = playlist_name.as_deref().map(str::trim) else {
                    return Err(TermusicError::InvalidArg(
                        "`-p` requires a `<playlist-name>` argument".to_string(),
                    ));
                };
                if name.is_empty() {
                    return Err(TermusicError::InvalidArg(
                        "playlist name cannot be empty".to_string(),
                    ));
                }
            } else if playlist_name.is_some() {
                return Err(TermusicError::InvalidArg(
                    "unexpected `<playlist-name>` argument without `-p`".to_string(),
                ));
            }
            Ok(Command::Add {
                folder,
                playlist,
                path,
                playlist_name: playlist_name.map(|s| s.trim().to_string()),
            })
        }
        RawCommand::Next => Ok(Command::Next),
        RawCommand::Prev => Ok(Command::Prev),
        RawCommand::Rewind { seconds } => Ok(Command::Rewind { seconds }),
        RawCommand::Advance { seconds } => Ok(Command::Advance { seconds }),
        RawCommand::Delete {
            target,
            song,
            playlist,
        } => {
            if target.trim().is_empty() {
                return Err(TermusicError::InvalidArg(
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
            raw_songs,
        } => {
            let trimmed_playlist = playlist.trim();
            if trimmed_playlist.is_empty() {
                return Err(TermusicError::InvalidArg(
                    "playlist name cannot be empty".to_string(),
                ));
            }
            let songs = parse_songs_kv(&raw_songs)?;
            Ok(Command::Create {
                playlist: trimmed_playlist.to_string(),
                songs,
            })
        }
        RawCommand::Pause => Ok(Command::Pause),
        RawCommand::Resume => Ok(Command::Resume),
        RawCommand::Stop => Ok(Command::Stop),
        RawCommand::Status => Ok(Command::Status),
        RawCommand::List { kind } => Ok(Command::List { kind }),
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
                return Err(TermusicError::InvalidArg(
                    "--jobs must be at least 1".to_string(),
                ));
            }
            if let Some(ref d) = dest
                && (d.as_os_str().is_empty() || d.to_str().is_some_and(|s| s.trim().is_empty()))
            {
                return Err(TermusicError::InvalidArg(
                    "--dest requires a non-empty directory path".to_string(),
                ));
            }
            if let Some(ref d) = set_dest
                && (d.as_os_str().is_empty() || d.to_str().is_some_and(|s| s.trim().is_empty()))
            {
                return Err(TermusicError::InvalidArg(
                    "--set-dest requires a non-empty directory path".to_string(),
                ));
            }
            let clean_playlist = match playlist_name {
                Some(name) => {
                    let trimmed = name.trim();
                    if trimmed.is_empty() {
                        return Err(TermusicError::InvalidArg(
                            "playlist name cannot be empty".to_string(),
                        ));
                    }
                    Some(trimmed.to_string())
                }
                None => None,
            };
            let normalized_urls = normalize_urls(&urls)?;
            if normalized_urls.is_empty() && set_dest.is_none() {
                return Err(TermusicError::InvalidArg(
                    "missing URL (pass <URL...> or --set-dest <DIR>)".to_string(),
                ));
            }
            Ok(Command::Download(DownloadOptions {
                urls: normalized_urls,
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
                return Err(TermusicError::InvalidArg(
                    "--dest requires a non-empty directory path".to_string(),
                ));
            }
            let joined_query = query.join(" ").trim().to_string();
            let setting_default =
                set_limit.is_some() || provider_default.is_some() || format_default.is_some();
            if joined_query.is_empty() && no_tui && (!setting_default || download) {
                return Err(TermusicError::InvalidArg(
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

/// Parse and validate CLI arguments from `std::env::args_os()`, exiting cleanly on `--help`/`--version`.
pub fn parse_cli() -> Result<Command> {
    let args = normalize_search_args(std::env::args_os());
    match Cli::try_parse_from(args) {
        Ok(cli) => validate_cli(cli),
        Err(err) => match err.kind() {
            ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => err.exit(),
            _ => Err(TermusicError::InvalidArg(err.to_string())),
        },
    }
}

/// Parse and validate CLI arguments from an arbitrary iterator (for tests).
pub fn parse_cli_from<I, T>(args: I) -> Result<Command>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let normalized = normalize_search_args(args);
    let cli = Cli::try_parse_from(normalized)
        .map_err(|err| TermusicError::InvalidArg(err.to_string()))?;
    validate_cli(cli)
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
        assert_eq!(parse_cli_from(["termusic"]).unwrap(), Command::Tui);
        assert_eq!(parse_cli_from(["termusic", "tui"]).unwrap(), Command::Tui);
    }

    #[test]
    fn version_command_and_flag_match_expected_version() {
        assert_eq!(
            parse_cli_from(["termusic", "version"]).unwrap(),
            Command::Version
        );
        assert_eq!(rendered_version().trim(), "termusic 0.1.0");
        let err = Cli::try_parse_from(["termusic", "-V"]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::DisplayVersion);
        assert_eq!(err.to_string().trim(), "termusic 0.1.0");
    }

    #[test]
    fn play_parses_defaults_modes_and_disambiguation() {
        assert_eq!(
            parse_cli_from(["termusic", "play"]).unwrap(),
            Command::Resume
        );
        assert_eq!(
            parse_cli_from(["termusic", "play", "My Song"]).unwrap(),
            Command::Play {
                target: "My Song".to_string(),
                mode: PlayMode::Next,
                kind: TargetKind::Auto,
                artist: None,
            }
        );
        assert_eq!(
            parse_cli_from(["termusic", "play", "Chill Mix", "shuffle"]).unwrap(),
            Command::Play {
                target: "Chill Mix".to_string(),
                mode: PlayMode::Shuffle,
                kind: TargetKind::Auto,
                artist: None,
            }
        );
        assert_eq!(
            parse_cli_from(["termusic", "play", "Chill Mix", "NEXT", "--playlist"]).unwrap(),
            Command::Play {
                target: "Chill Mix".to_string(),
                mode: PlayMode::Next,
                kind: TargetKind::Playlist,
                artist: None,
            }
        );
        assert_eq!(
            parse_cli_from(["termusic", "play", "Track", "--song"]).unwrap(),
            Command::Play {
                target: "Track".to_string(),
                mode: PlayMode::Next,
                kind: TargetKind::Song,
                artist: None,
            }
        );
        assert_eq!(
            parse_cli_from(["termusic", "play", "Track", "--artist", "Artist A"]).unwrap(),
            Command::Play {
                target: "Track".to_string(),
                mode: PlayMode::Next,
                kind: TargetKind::Auto,
                artist: Some("Artist A".to_string()),
            }
        );
        assert_eq!(
            parse_cli_from([
                "termusic",
                "play",
                "Track",
                "shuffle",
                "--song",
                "--artist",
                "  Artist B  ",
            ])
            .unwrap(),
            Command::Play {
                target: "Track".to_string(),
                mode: PlayMode::Shuffle,
                kind: TargetKind::Song,
                artist: Some("Artist B".to_string()),
            }
        );
        assert!(parse_cli_from(["termusic", "play", "--song"]).is_err());
        assert!(parse_cli_from(["termusic", "play", "--playlist"]).is_err());
        assert!(matches!(
            parse_cli_from(["termusic", "play", "--artist", "Artist A"]),
            Err(TermusicError::InvalidArg(_))
        ));
        assert!(matches!(
            parse_cli_from(["termusic", "play", "Track", "--artist", "   "]),
            Err(TermusicError::InvalidArg(_))
        ));
        assert!(matches!(
            parse_cli_from(["termusic", "play", "Mix", "--playlist", "--artist", "Artist A"]),
            Err(TermusicError::InvalidArg(_))
        ));
        assert!(parse_cli_from(["termusic", "play", "Track", "random"]).is_err());
        assert!(parse_cli_from(["termusic", "play", "Track", "--song", "--playlist"]).is_err());
        assert!(parse_cli_from(["termusic", "play", "   "]).is_err());
    }

    #[test]
    fn add_parses_flags_and_validates_playlist_and_paths() {
        assert_eq!(
            parse_cli_from(["termusic", "add", "song.mp3"]).unwrap(),
            Command::Add {
                folder: false,
                playlist: false,
                path: PathBuf::from("song.mp3"),
                playlist_name: None,
            }
        );
        assert_eq!(
            parse_cli_from(["termusic", "add", "-f", "./music"]).unwrap(),
            Command::Add {
                folder: true,
                playlist: false,
                path: PathBuf::from("./music"),
                playlist_name: None,
            }
        );
        assert_eq!(
            parse_cli_from(["termusic", "add", "-p", "song.mp3", "Favorites"]).unwrap(),
            Command::Add {
                folder: false,
                playlist: true,
                path: PathBuf::from("song.mp3"),
                playlist_name: Some("Favorites".to_string()),
            }
        );
        assert_eq!(
            parse_cli_from(["termusic", "add", "-fp", "./music", "Roadtrip"]).unwrap(),
            Command::Add {
                folder: true,
                playlist: true,
                path: PathBuf::from("./music"),
                playlist_name: Some("Roadtrip".to_string()),
            }
        );

        // -p without playlist name must fail
        assert!(matches!(
            parse_cli_from(["termusic", "add", "-p", "song.mp3"]),
            Err(TermusicError::InvalidArg(_))
        ));
        // empty playlist name must fail
        assert!(matches!(
            parse_cli_from(["termusic", "add", "-p", "song.mp3", "  "]),
            Err(TermusicError::InvalidArg(_))
        ));
        // playlist name without -p must fail
        assert!(matches!(
            parse_cli_from(["termusic", "add", "song.mp3", "Favorites"]),
            Err(TermusicError::InvalidArg(_))
        ));

        // Filesystem validation for -f vs non--f
        let tmp = std::env::temp_dir().join(format!("termusic-cli-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&tmp).unwrap();
        let file = tmp.join("sample.wav");
        fs::write(&file, b"RIFF").unwrap();

        assert!(validate_add_path(&file, false).is_ok());
        assert!(validate_add_path(&tmp, true).is_ok());
        assert!(matches!(
            validate_add_path(&tmp, false),
            Err(TermusicError::InvalidArg(_))
        ));
        assert!(matches!(
            validate_add_path(&file, true),
            Err(TermusicError::InvalidArg(_))
        ));
        assert!(matches!(
            validate_add_path(&tmp.join("missing.wav"), false),
            Err(TermusicError::NotFound(_))
        ));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn transport_and_seek_commands_parse_and_validate() {
        assert_eq!(parse_cli_from(["termusic", "next"]).unwrap(), Command::Next);
        assert_eq!(parse_cli_from(["termusic", "prev"]).unwrap(), Command::Prev);
        assert_eq!(
            parse_cli_from(["termusic", "rewind", "5.5"]).unwrap(),
            Command::Rewind { seconds: 5.5 }
        );
        assert_eq!(
            parse_cli_from(["termusic", "rewind", "0"]).unwrap(),
            Command::Rewind { seconds: 0.0 }
        );
        assert_eq!(
            parse_cli_from(["termusic", "advance", "12"]).unwrap(),
            Command::Advance { seconds: 12.0 }
        );

        assert!(parse_cli_from(["termusic", "rewind", "-1"]).is_err());
        assert!(parse_cli_from(["termusic", "advance", "-0.5"]).is_err());
        assert!(parse_cli_from(["termusic", "rewind", "NaN"]).is_err());
        assert!(parse_cli_from(["termusic", "advance", "inf"]).is_err());
    }

    #[test]
    fn delete_parses_with_optional_disambiguation() {
        assert_eq!(
            parse_cli_from(["termusic", "delete", "Chill"]).unwrap(),
            Command::Delete {
                target: "Chill".to_string(),
                kind: TargetKind::Auto,
            }
        );
        assert_eq!(
            parse_cli_from(["termusic", "delete", "Chill", "--song"]).unwrap(),
            Command::Delete {
                target: "Chill".to_string(),
                kind: TargetKind::Song,
            }
        );
        assert_eq!(
            parse_cli_from(["termusic", "delete", "Chill", "--playlist"]).unwrap(),
            Command::Delete {
                target: "Chill".to_string(),
                kind: TargetKind::Playlist,
            }
        );
        assert!(parse_cli_from(["termusic", "delete", "Chill", "--song", "--playlist"]).is_err());
        assert!(parse_cli_from(["termusic", "delete", ""]).is_err());
    }

    #[test]
    fn create_supports_quoted_and_unquoted_songs_kv() {
        assert_eq!(
            parse_cli_from(["termusic", "create", "Mix", "songs=Alpha,Beta"]).unwrap(),
            Command::Create {
                playlist: "Mix".to_string(),
                songs: vec!["Alpha".to_string(), "Beta".to_string()],
            }
        );
        assert_eq!(
            parse_cli_from(["termusic", "create", "Mix", "songs=Song One, Song Two"]).unwrap(),
            Command::Create {
                playlist: "Mix".to_string(),
                songs: vec!["Song One".to_string(), "Song Two".to_string()],
            }
        );
        assert_eq!(
            parse_cli_from([
                "termusic",
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

        assert!(parse_cli_from(["termusic", "create", "Mix"]).is_err());
        assert!(parse_cli_from(["termusic", "create", "Mix", "songs="]).is_err());
        assert!(parse_cli_from(["termusic", "create", "Mix", "songs= , "]).is_err());
        assert!(parse_cli_from(["termusic", "create", "Mix", "tracks=A,B"]).is_err());
        assert!(parse_cli_from(["termusic", "create", "  ", "songs=A"]).is_err());
    }

    #[test]
    fn supplemental_and_hidden_commands_parse() {
        assert_eq!(parse_cli_from(["termusic", "pause"]).unwrap(), Command::Pause);
        assert_eq!(parse_cli_from(["termusic", "resume"]).unwrap(), Command::Resume);
        assert_eq!(parse_cli_from(["termusic", "stop"]).unwrap(), Command::Stop);
        assert_eq!(parse_cli_from(["termusic", "status"]).unwrap(), Command::Status);
        assert_eq!(
            parse_cli_from(["termusic", "list"]).unwrap(),
            Command::List { kind: None }
        );
        assert_eq!(
            parse_cli_from(["termusic", "list", "songs"]).unwrap(),
            Command::List {
                kind: Some(ListKind::Songs)
            }
        );
        assert_eq!(
            parse_cli_from(["termusic", "list", "playlists"]).unwrap(),
            Command::List {
                kind: Some(ListKind::Playlists)
            }
        );
        assert_eq!(
            parse_cli_from(["termusic", "daemon", "stop"]).unwrap(),
            Command::DaemonStop
        );
        assert_eq!(
            parse_cli_from(["termusic", "__daemon"]).unwrap(),
            Command::Daemon(DaemonOverrides::default())
        );

        let overrides = DaemonOverrides {
            library_path: Some(PathBuf::from("/tmp/data/library.json")),
            socket_path: Some("/tmp/run/custom.sock".to_string()),
            pid_path: Some(PathBuf::from("/tmp/run/custom.pid")),
            log_path: Some(PathBuf::from("/tmp/run/custom.log")),
        };
        let resolved = overrides.resolve_paths().unwrap();
        assert_eq!(resolved.library_path, PathBuf::from("/tmp/data/library.json"));
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
            IpcEndpoint::NamedPipe(ref name) if name.starts_with(r"\\.\pipe\termusic-")
        ));
    }

    #[test]
    fn init_parses_path_and_rejects_empty() {
        assert_eq!(
            parse_cli_from(["termusic", "init", "/tmp/my-music-ws"]).unwrap(),
            Command::Init {
                path: PathBuf::from("/tmp/my-music-ws")
            }
        );
        assert_eq!(
            parse_cli_from(["termusic", "init", "./workspace"]).unwrap(),
            Command::Init {
                path: PathBuf::from("./workspace")
            }
        );
        assert!(matches!(
            parse_cli_from(["termusic", "init", ""]),
            Err(TermusicError::InvalidArg(_))
        ));
        assert!(matches!(
            parse_cli_from(["termusic", "init", "   "]),
            Err(TermusicError::InvalidArg(_))
        ));
        assert!(parse_cli_from(["termusic", "init"]).is_err());
    }

    #[test]
    fn download_parses_options_and_validates_constraints() {
        assert_eq!(
            parse_cli_from(["termusic", "download", "https://youtu.be/dQw4w9WgXcQ"]).unwrap(),
            Command::Download(DownloadOptions {
                urls: vec!["https://youtu.be/dQw4w9WgXcQ".to_string()],
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
                "termusic",
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
            parse_cli_from(["termusic", "download", "--set-dest", "/tmp/saved"]).unwrap(),
            Command::Download(DownloadOptions {
                urls: Vec::new(),
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
                "termusic",
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
            parse_cli_from(["termusic", "download"]),
            Err(TermusicError::InvalidArg(_))
        ));

        // --jobs 0 fails.
        assert!(matches!(
            parse_cli_from(["termusic", "download", "-j", "0", "https://example.com/a"]),
            Err(TermusicError::InvalidArg(_))
        ));
        assert!(matches!(
            parse_cli_from([
                "termusic",
                "download",
                "--jobs",
                "0",
                "https://example.com/a"
            ]),
            Err(TermusicError::InvalidArg(_))
        ));

        // Empty URL arg fails.
        assert!(matches!(
            parse_cli_from(["termusic", "download", "   "]),
            Err(TermusicError::InvalidArg(_))
        ));

        // Conflicting -p / --playlist-name and --no-add fails.
        assert!(
            parse_cli_from([
                "termusic",
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
            parse_cli_from(["termusic", "download", "-d", "", "https://example.com/a"]),
            Err(TermusicError::InvalidArg(_))
        ));
        assert!(matches!(
            parse_cli_from(["termusic", "download", "--dest=   ", "https://example.com/a"]),
            Err(TermusicError::InvalidArg(_))
        ));
        assert!(matches!(
            parse_cli_from(["termusic", "download", "--set-dest", ""]),
            Err(TermusicError::InvalidArg(_))
        ));
        assert!(matches!(
            parse_cli_from(["termusic", "download", "--set-dest=  "]),
            Err(TermusicError::InvalidArg(_))
        ));
        assert!(matches!(
            parse_cli_from(["termusic", "download", "-p", "  ", "https://example.com/a"]),
            Err(TermusicError::InvalidArg(_))
        ));
    }

    #[test]
    fn search_parses_options_and_validates_constraints() {
        assert_eq!(
            parse_cli_from(["termusic", "search"]).unwrap(),
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
            parse_cli_from(["termusic", "search", "   "]).unwrap(),
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
            parse_cli_from(["termusic", "search", "-n", "15", "lofi", "hip", "hop"]).unwrap(),
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
                "termusic",
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
            parse_cli_from(["termusic", "search", "--set-limit", "30"]).unwrap(),
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
            parse_cli_from(["termusic", "search", "--set-limit", "30", "--no-tui"]).unwrap(),
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
                "termusic",
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
                "termusic",
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
            parse_cli_from(["termusic", "search", "-pd", "soundcloud"]).unwrap(),
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
            parse_cli_from(["termusic", "search", "--pd=youtube", "-fd", ".mp4"]).unwrap(),
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
            parse_cli_from(["termusic", "search", "-format-default", ".mp3", "--no-tui"]).unwrap(),
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
                "termusic",
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
            parse_cli_from(["termusic", "search", "--no-tui", "--", "-pd", "-fd"]).unwrap(),
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
            parse_cli_from(["termusic", "download", "-f", ".mp4", "https://youtu.be/abc"]).unwrap(),
            Command::Download(DownloadOptions {
                urls: vec!["https://youtu.be/abc".to_string()],
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
            parse_cli_from(["termusic", "search", "--no-tui"]),
            Err(TermusicError::InvalidArg(_))
        ));
        assert!(matches!(
            parse_cli_from(["termusic", "search", "--no-tui", "   "]),
            Err(TermusicError::InvalidArg(_))
        ));
        assert!(matches!(
            parse_cli_from(["termusic", "search", "--no-tui", "-d", "--set-limit", "5"]),
            Err(TermusicError::InvalidArg(_))
        ));

        // Empty --dest, unsupported provider, and unsupported format fail.
        assert!(matches!(
            parse_cli_from(["termusic", "search", "--dest", "", "jazz"]),
            Err(TermusicError::InvalidArg(_))
        ));
        assert!(matches!(
            parse_cli_from(["termusic", "search", "-p", "https://example.com", "jazz"]),
            Err(TermusicError::InvalidArg(_))
        ));
        assert!(matches!(
            parse_cli_from(["termusic", "search", "-f", ".avi", "jazz"]),
            Err(TermusicError::InvalidArg(_))
        ));

        // Bounds validation (1..=100) for --limit / -n and --set-limit.
        assert!(matches!(
            parse_cli_from(["termusic", "search", "-n", "0", "jazz"]),
            Err(TermusicError::InvalidArg(_))
        ));
        assert!(matches!(
            parse_cli_from(["termusic", "search", "--limit", "101", "jazz"]),
            Err(TermusicError::InvalidArg(_))
        ));
        assert!(matches!(
            parse_cli_from(["termusic", "search", "--set-limit", "0"]),
            Err(TermusicError::InvalidArg(_))
        ));
        assert!(matches!(
            parse_cli_from(["termusic", "search", "--set-limit", "101"]),
            Err(TermusicError::InvalidArg(_))
        ));
    }
}
