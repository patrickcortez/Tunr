use termusic::cli::{Command, ListKind, parse_cli};
use termusic::error::{Result, TermusicError};
use termusic::ipc::{self, IpcRequest, PlaybackState, PlaybackStatus};
use termusic::library::{DeleteOutcome, Library};
use termusic::paths::{self, AppPaths};
use termusic::{daemon, download, tui};

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let command = parse_cli()?;
    match command {
        Command::Init { path } => {
            let cfg = paths::initialize_workspace(&path)?;
            let paths = AppPaths::resolve()?;
            if !paths.library_path.exists() {
                let library = Library::load(&paths.library_path)?;
                library.save(&paths.library_path)?;
            }
            let _ = ipc::send_request(&paths, &IpcRequest::Shutdown);
            println!(
                "Initialized Termusic workspace at {}",
                cfg.working_dir.display()
            );
            println!("  Config:    {}", paths.config_path.display());
            println!("  Playlists: {}", paths.playlists_dir.display());
            println!("  Library:   {}", paths.library_path.display());
            println!("  Music:     {}", paths.music_dir.display());
            return Ok(());
        }
        Command::Daemon(overrides) => {
            let paths = overrides.resolve_paths()?;
            return daemon::run_daemon(&paths);
        }
        _ => {}
    }

    let paths = AppPaths::resolve()?;
    match command {
        Command::Init { .. } | Command::Daemon(_) => unreachable!(),
        Command::Tui => tui::run_tui(&paths),
        Command::Version => {
            println!("termusic {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Command::Play { target, mode, kind } => {
            let library = Library::load(&paths.library_path)?;
            let resolved = library.resolve_play_target(&target, kind)?;
            let (queue, start_index, playlist_name) = resolved.into_play_parts();
            let status = ipc::ensure_daemon_and_send(
                &paths,
                &IpcRequest::Play {
                    queue,
                    start_index,
                    mode,
                    playlist_name,
                },
            )?;
            print_playback_status(&status);
            Ok(())
        }
        Command::Add {
            folder,
            playlist,
            path,
            playlist_name,
        } => {
            let mut library = Library::load(&paths.library_path)?;
            let target_pl = if playlist {
                playlist_name.as_deref()
            } else {
                None
            };
            let summary = library.add_files(&path, folder, target_pl)?;
            library.save(&paths.library_path)?;

            println!(
                "Added {} new song(s) ({} already in library).",
                summary.added_count, summary.existing_count
            );
            if let Some(pl_name) = summary.playlist_name {
                println!(
                    "Playlist '{}': added {} song(s).",
                    pl_name, summary.playlist_added_count
                );
            }
            for skipped in summary.skipped_files {
                eprintln!("warning: skipped {skipped}");
            }
            Ok(())
        }
        Command::Next => {
            let status = ipc::send_request(&paths, &IpcRequest::Next)?;
            print_playback_status(&status);
            Ok(())
        }
        Command::Prev => {
            let status = ipc::send_request(&paths, &IpcRequest::Prev)?;
            print_playback_status(&status);
            Ok(())
        }
        Command::Rewind { seconds } => {
            let status = ipc::send_request(&paths, &IpcRequest::SeekRelative(-seconds))?;
            print_playback_status(&status);
            Ok(())
        }
        Command::Advance { seconds } => {
            let status = ipc::send_request(&paths, &IpcRequest::SeekRelative(seconds))?;
            print_playback_status(&status);
            Ok(())
        }
        Command::Delete { target, kind } => {
            let mut library = Library::load(&paths.library_path)?;
            let outcome = library.delete_target(&target, kind)?;
            library.save(&paths.library_path)?;
            match outcome {
                DeleteOutcome::Song(song) => {
                    println!("Deleted song '{}' from library.", song.title);
                }
                DeleteOutcome::Playlist(playlist) => {
                    println!("Deleted playlist '{}' from library.", playlist.name);
                }
            }
            Ok(())
        }
        Command::Create { playlist, songs } => {
            let mut library = Library::load(&paths.library_path)?;
            let created = library.create_playlist(&playlist, &songs)?;
            library.save(&paths.library_path)?;
            println!(
                "Created playlist '{}' with {} song(s).",
                created.name,
                created.song_ids.len()
            );
            Ok(())
        }
        Command::Pause => {
            let status = ipc::send_request(&paths, &IpcRequest::SetPaused(true))?;
            print_playback_status(&status);
            Ok(())
        }
        Command::Resume => {
            let status = ipc::send_request(&paths, &IpcRequest::SetPaused(false))?;
            print_playback_status(&status);
            Ok(())
        }
        Command::Stop => {
            let status = ipc::send_request(&paths, &IpcRequest::Stop)?;
            print_playback_status(&status);
            Ok(())
        }
        Command::Status => match ipc::send_request(&paths, &IpcRequest::GetStatus) {
            Ok(status) => {
                print_playback_status(&status);
                Ok(())
            }
            Err(TermusicError::Ipc(_)) => {
                println!("State: stopped (daemon not running)");
                Ok(())
            }
            Err(err) => Err(err),
        },
        Command::List { kind } => {
            let library = Library::load(&paths.library_path)?;
            print_library_list(&library, kind);
            Ok(())
        }
        Command::Download(opts) => {
            let outcome = download::run_download(&paths, &opts)?;
            if let Some(saved_dest) = &outcome.saved_dest {
                println!(
                    "Saved default download destination: {}",
                    saved_dest.display()
                );
            }
            for file in &outcome.downloaded_files {
                println!("Downloaded: {}", file.display());
            }
            if let Some(summary) = &outcome.add_summary {
                println!(
                    "Added {} new song(s) ({} already in library).",
                    summary.added_count, summary.existing_count
                );
                if let Some(pl_name) = &summary.playlist_name {
                    println!(
                        "Playlist '{}': added {} song(s).",
                        pl_name, summary.playlist_added_count
                    );
                }
                for skipped in &summary.skipped_files {
                    eprintln!("warning: skipped {skipped}");
                }
            }
            Ok(())
        }
    }
}

fn print_playback_status(status: &PlaybackStatus) {
    if status.state == PlaybackState::Stopped || status.current_track.is_none() {
        println!("State: {} [mode: {}]", status.state, status.mode);
        return;
    }

    let track = status.current_track.as_ref().expect("checked above");
    let artist = track
        .artist
        .as_deref()
        .map(|a| format!(" - {a}"))
        .unwrap_or_default();
    let playlist = status
        .playlist_name
        .as_deref()
        .map(|p| format!(" | playlist: {p}"))
        .unwrap_or_default();
    let total = status
        .duration_secs
        .or(track.duration_secs)
        .map(format_time)
        .unwrap_or_else(|| "--:--".to_string());

    println!(
        "[{}] {}{} ({}/{}) {} / {} [mode: {}{}]",
        status.state,
        track.title,
        artist,
        status.queue_index + 1,
        status.queue_len,
        format_time(status.position_secs),
        total,
        status.mode,
        playlist,
    );
}

fn print_library_list(library: &Library, kind: Option<ListKind>) {
    let show_songs = matches!(kind, None | Some(ListKind::Songs));
    let show_playlists = matches!(kind, None | Some(ListKind::Playlists));

    if show_songs {
        println!("Songs ({}):", library.songs.len());
        if library.songs.is_empty() {
            println!("  (none)");
        } else {
            for (idx, song) in library.songs.iter().enumerate() {
                let artist = song
                    .artist
                    .as_deref()
                    .map(|a| format!(" - {a}"))
                    .unwrap_or_default();
                let dur = song
                    .duration_secs
                    .map(format_time)
                    .unwrap_or_else(|| "--:--".to_string());
                println!("  {}. {}{} [{}]", idx + 1, song.title, artist, dur);
            }
        }
    }

    if show_playlists {
        if show_songs {
            println!();
        }
        println!("Playlists ({}):", library.playlists.len());
        if library.playlists.is_empty() {
            println!("  (none)");
        } else {
            for playlist in &library.playlists {
                let titles: Vec<&str> = playlist
                    .song_ids
                    .iter()
                    .filter_map(|id| library.find_song_by_id(id).map(|s| s.title.as_str()))
                    .collect();
                println!(
                    "  {} ({} song(s)): {}",
                    playlist.name,
                    titles.len(),
                    if titles.is_empty() {
                        "(empty)".to_string()
                    } else {
                        titles.join(", ")
                    }
                );
            }
        }
    }
}

fn format_time(secs: f64) -> String {
    let clamped = if secs.is_finite() && secs > 0.0 {
        secs.floor() as u64
    } else {
        0
    };
    let mins = clamped / 60;
    let rem = clamped % 60;
    format!("{mins:02}:{rem:02}")
}
