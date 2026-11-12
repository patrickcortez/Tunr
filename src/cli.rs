use std::ffi::OsString;
use std::path::{Path, PathBuf};

use clap::error::ErrorKind;
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};

use crate::download::{DownloadFormat, DownloadOptions, normalize_urls};
use crate::error::{Result, TermusicError};
use crate::ipc::{PlayMode, TargetKind};
use crate::paths::AppPaths;

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
    pub socket_path: Option<PathBuf>,
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
            let run_root = socket_path.parent().unwrap_or_else(|| Path::new("."));
            let mut paths = AppPaths::from_roots(data_root, run_root);
            paths.library_path = library_path.clone();
            paths.socket_path = socket_path.clone();
            paths.pid_path = pid_path.clone();
            paths.log_path = log_path.clone();
            return Ok(paths);
        }

        let mut paths = AppPaths::resolve()?;
        if let Some(p) = &self.library_path {
            paths.library_path = p.clone();
        }
        if let Some(p) = &self.socket_path {
            paths.socket_path = p.clone();
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
        /// Target audio format
        #[arg(
            short = 'f',
            long = "format",
            value_enum,
            ignore_case = true,
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

    /// Internal background playback daemon entrypoint
    #[command(name = "__daemon", hide = true)]
    Daemon(DaemonOverrides),
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
        } => match target {
            None => {
                if song || playlist || mode.is_some() {
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
                Ok(Command::Play {
                    target,
                    mode: mode.unwrap_or_default(),
                    kind: resolve_target_kind(song, playlist)?,
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
        RawCommand::Daemon(overrides) => Ok(Command::Daemon(overrides)),
    }
}

/// Parse and validate CLI arguments from `std::env::args_os()`, exiting cleanly on `--help`/`--version`.
pub fn parse_cli() -> Result<Command> {
    match Cli::try_parse() {
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
    let cli = Cli::try_parse_from(args)
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
            }
        );
        assert_eq!(
            parse_cli_from(["termusic", "play", "Chill Mix", "shuffle"]).unwrap(),
            Command::Play {
                target: "Chill Mix".to_string(),
                mode: PlayMode::Shuffle,
                kind: TargetKind::Auto,
            }
        );
        assert_eq!(
            parse_cli_from(["termusic", "play", "Chill Mix", "NEXT", "--playlist"]).unwrap(),
            Command::Play {
                target: "Chill Mix".to_string(),
                mode: PlayMode::Next,
                kind: TargetKind::Playlist,
            }
        );
        assert_eq!(
            parse_cli_from(["termusic", "play", "Track", "--song"]).unwrap(),
            Command::Play {
                target: "Track".to_string(),
                mode: PlayMode::Next,
                kind: TargetKind::Song,
            }
        );
        assert!(parse_cli_from(["termusic", "play", "--song"]).is_err());
        assert!(parse_cli_from(["termusic", "play", "--playlist"]).is_err());
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
            parse_cli_from(["termusic", "__daemon"]).unwrap(),
            Command::Daemon(DaemonOverrides::default())
        );
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
}
