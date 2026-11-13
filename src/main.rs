use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;

use tunr::cli::{AddInput, Command, ListKind, PlayQuery, ThemeAction, parse_cli};
use tunr::error::{Result, TunrError};
use tunr::ipc::{self, IpcRequest, PlaybackState, PlaybackStatus};
use tunr::library::{DeleteOutcome, Library, PlayResolution, SongCandidate, UpdateSummary};
use tunr::paths::{self, AppPaths};
use tunr::theme::{DEFAULT_THEME_NAME, Theme, ThemesFile};
use tunr::{daemon, download, download_tui, search, search_tui, tui};

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}

async fn run_blocking<T, F>(f: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|err| TunrError::Async(format!("blocking task failed: {err}")))?
}

async fn run() -> Result<()> {
    let command = parse_cli()?;
    match command {
        Command::Version => {
            println!("tunr {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Command::Init { path } => {
            let (cfg, paths) = run_blocking(move || {
                let cfg = paths::initialize_workspace(&path)?;
                let paths = AppPaths::resolve()?;
                if !paths.library_path.exists() {
                    Library::update(&paths.library_path, |_| Ok(()))?;
                }
                Ok((cfg, paths))
            })
            .await?;
            let _ = ipc::send_request(&paths, &IpcRequest::Shutdown).await;
            #[cfg(target_os = "android")]
            if is_android_shared_storage(&cfg.working_dir) {
                eprintln!(
                    "warning: workspace {} is on Android shared storage, where file locking may be unreliable; consider placing the workspace under $HOME (e.g. ~/tunr-workspace) and using `tunr add -f` to import music",
                    cfg.working_dir.display()
                );
            }
            println!(
                "Initialized Tunr workspace at {}",
                cfg.working_dir.display()
            );
            println!("  Config:    {}", paths.config_path.display());
            println!("  Playlists: {}", paths.playlists_dir.display());
            println!("  Library:   {}", paths.library_path.display());
            println!("  Music:     {}", paths.music_dir.display());
            return Ok(());
        }
        Command::Daemon(overrides) => {
            let paths = run_blocking(move || overrides.resolve_paths()).await?;
            return daemon::run_daemon(&paths).await;
        }
        Command::Search(opts) => {
            let saving_defaults = opts.set_limit.is_some()
                || opts.provider_default.is_some()
                || opts.format_default.is_some();

            let paths_opt = if saving_defaults || opts.download {
                Some(run_blocking(AppPaths::resolve).await?)
            } else {
                run_blocking(|| Ok(AppPaths::resolve().ok())).await?
            };

            if saving_defaults {
                let paths = paths_opt.as_ref().ok_or_else(|| {
                    TunrError::NotInitialized("workspace is not initialized".to_string())
                })?;
                if let Some(limit) = opts.set_limit {
                    let p = paths.clone();
                    run_blocking(move || paths::set_workspace_search_limit(&p, limit)).await?;
                    println!("Saved default search limit: {limit}");
                }
                if let Some(provider) = opts.provider_default {
                    let p = paths.clone();
                    run_blocking(move || paths::set_workspace_search_provider(&p, provider))
                        .await?;
                    println!(
                        "Saved default search provider: {} ({})",
                        provider.display_name(),
                        provider.id()
                    );
                }
                if let Some(format) = opts.format_default {
                    let p = paths.clone();
                    run_blocking(move || paths::set_workspace_search_format(&p, format)).await?;
                    println!("Saved default download format: {format}");
                }
                if opts.query.is_empty() && !opts.download {
                    return Ok(());
                }
            }

            let workspace_cfg = if let Some(p) = paths_opt.clone() {
                run_blocking(move || Ok(paths::load_workspace_config(&p).ok())).await?
            } else {
                None
            };

            let config_limit = opts
                .set_limit
                .or_else(|| workspace_cfg.as_ref().and_then(|c| c.search_limit));
            let config_provider = opts
                .provider_default
                .or_else(|| workspace_cfg.as_ref().and_then(|c| c.search_provider));
            let config_format = opts
                .format_default
                .or_else(|| workspace_cfg.as_ref().and_then(|c| c.search_format));

            let env_limit = std::env::var("TUNR_SEARCH_LIMIT").ok();
            let env_provider = std::env::var("TUNR_SEARCH_PROVIDER").ok();
            let env_format = std::env::var("TUNR_SEARCH_FORMAT").ok();

            let effective_limit =
                search::resolve_search_limit(&opts, env_limit.as_deref(), config_limit)?;
            let effective_provider =
                search::resolve_search_provider(&opts, env_provider.as_deref(), config_provider)?;
            let effective_format =
                search::resolve_search_format(&opts, env_format.as_deref(), config_format)?;

            if opts.download {
                let paths = paths_opt
                    .as_ref()
                    .ok_or_else(|| {
                        TunrError::NotInitialized("workspace is not initialized".to_string())
                    })?
                    .clone();
                let dest = opts.dest.clone();
                run_blocking(move || {
                    tunr::download::validate_search_download_destination(
                        &paths,
                        dest.as_deref(),
                    )
                })
                .await?;
            }

            let mut cfg = search_tui::SearchRunConfig {
                limit: effective_limit,
                provider: effective_provider,
                download_mode: opts.download,
                format: effective_format,
                paths: paths_opt.clone(),
                dest: opts.dest,
                theme: Theme::default(),
                theme_warning: None,
            };

            if !opts.no_tui
                && std::io::stdin().is_terminal()
                && std::io::stdout().is_terminal()
            {
                let theme_outcome = run_blocking(move || {
                    Ok(paths::load_active_theme_for_paths(paths_opt.as_ref()))
                })
                .await?;
                cfg.theme = theme_outcome.theme;
                cfg.theme_warning = theme_outcome.warning;
                let initial_query = if opts.query.is_empty() {
                    None
                } else {
                    Some(opts.query)
                };
                return search_tui::run_search_tui(initial_query, cfg).await;
            }

            if opts.query.is_empty() {
                return Err(TunrError::InvalidArg(
                    "missing search query (pass <QUERY...> when stdout/stdin is not a TTY or with --no-tui)"
                        .to_string(),
                ));
            }
            return search_tui::run_search_plain(&opts.query, cfg).await;
        }
        Command::Theme(action) => {
            return run_blocking(move || {
                let paths_opt = match AppPaths::resolve() {
                    Ok(p) => Some(p),
                    Err(TunrError::NotInitialized(_)) => None,
                    Err(err) => return Err(err),
                };
                let global_path = paths::resolve_global_themes_path()?;
                let stdout = std::io::stdout();
                let stderr = std::io::stderr();
                let mut out = stdout.lock();
                let mut err_out = stderr.lock();
                execute_theme_action_with_paths(
                    action,
                    paths_opt.as_ref(),
                    &global_path,
                    &mut out,
                    &mut err_out,
                )
            })
            .await;
        }
        _ => {}
    }

    let paths = run_blocking(AppPaths::resolve).await?;
    match command {
        Command::Version
        | Command::Init { .. }
        | Command::Daemon(_)
        | Command::Search(_)
        | Command::Theme(_) => {
            unreachable!()
        }
        Command::Tui => {
            if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
                return Err(TunrError::InvalidArg(
                    "interactive TUI requires a terminal (stdin/stdout TTY); pass a subcommand such as `tunr status` or `tunr play`".to_string(),
                ));
            }
            let p = paths.clone();
            let theme_outcome =
                run_blocking(move || Ok(paths::load_active_theme_for_paths(Some(&p)))).await?;
            tui::run_tui(&paths, &theme_outcome.theme, theme_outcome.warning).await
        }
        Command::Play {
            query,
            mode,
            kind,
            artist,
            loop_playback,
        } => {
            let lib_path = paths.library_path.clone();
            let (library, resolution) = run_blocking(move || {
                let library = Library::load(&lib_path)?;
                let resolution = match &query {
                    PlayQuery::Text(target) => {
                        library.resolve_play_target_with_artist(target, kind, artist.as_deref())?
                    }
                    PlayQuery::Index(idx) => {
                        library.resolve_play_index(*idx, kind, artist.as_deref())?
                    }
                };
                Ok((library, resolution))
            })
            .await?;
            let resolved = match resolution {
                PlayResolution::Ready(play_target) => play_target,
                PlayResolution::AmbiguousSongs { query, candidates } => {
                    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
                        let list = candidates
                            .iter()
                            .map(|c| {
                                let a = c.artist.as_deref().unwrap_or("Unknown Artist");
                                format!("{} — {}", c.title, a)
                            })
                            .collect::<Vec<_>>()
                            .join("; ");
                        return Err(TunrError::Ambiguous(format!(
                            "multiple songs match '{query}': {list}; specify `--artist <ARTIST>`"
                        )));
                    }
                    run_blocking(move || {
                        let stdin = std::io::stdin();
                        let stdout = std::io::stdout();
                        let mut reader = stdin.lock();
                        let mut writer = stdout.lock();
                        let selected_idx = prompt_song_selection(
                            &query,
                            &candidates,
                            &mut reader,
                            &mut writer,
                        )?;
                        library.play_target_for_song_index(selected_idx)
                    })
                    .await?
                }
            };
            let (queue, start_index, playlist_name) = resolved.into_play_parts();
            let status = ipc::ensure_daemon_and_send(
                &paths,
                &IpcRequest::Play {
                    queue,
                    start_index,
                    mode,
                    playlist_name,
                    loop_playback,
                },
            )
            .await?;
            print_playback_status(&status);
            Ok(())
        }
        Command::Add {
            input,
            playlist_name,
        } => {
            let lib_path = paths.library_path.clone();
            let summary = run_blocking(move || {
                Library::update(&lib_path, |library| {
                    let target_pl = playlist_name.as_deref();
                    match input {
                        AddInput::Path { path, folder } => {
                            library.add_files(&path, folder, target_pl)
                        }
                        AddInput::Song(query) => library.add_song(&query, target_pl),
                        AddInput::ExistingSong(query) => {
                            let pl = target_pl.ok_or_else(|| {
                                TunrError::InvalidArg(
                                    "`-e`/`--existing` requires a target playlist (`-p <PLAYLIST>`)"
                                        .to_string(),
                                )
                            })?;
                            library.add_existing_song(&query, pl)
                        }
                    }
                })
            })
            .await?;

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
            let status = ipc::send_request(&paths, &IpcRequest::Next).await?;
            print_playback_status(&status);
            Ok(())
        }
        Command::Prev => {
            let status = ipc::send_request(&paths, &IpcRequest::Prev).await?;
            print_playback_status(&status);
            Ok(())
        }
        Command::Rewind { seconds } => {
            let req = match seconds {
                Some(seconds) => IpcRequest::SeekRelative(-seconds),
                None => IpcRequest::SeekAbsolute(0.0),
            };
            let status = ipc::send_request(&paths, &req).await?;
            print_playback_status(&status);
            Ok(())
        }
        Command::Advance { seconds } => {
            let status = ipc::send_request(&paths, &IpcRequest::SeekRelative(seconds)).await?;
            print_playback_status(&status);
            Ok(())
        }
        Command::Volume { level } => match level {
            Some(vol) => {
                let p = paths.clone();
                run_blocking(move || paths::set_workspace_volume(&p, vol)).await?;
                match ipc::send_request(&paths, &IpcRequest::SetVolume(vol)).await {
                    Ok(status) => {
                        println!("Volume: {}%", status.volume);
                        Ok(())
                    }
                    Err(TunrError::Ipc(_)) => {
                        println!("Volume: {vol}%");
                        Ok(())
                    }
                    Err(err) => Err(err),
                }
            }
            None => match ipc::send_request(&paths, &IpcRequest::GetStatus).await {
                Ok(status) => {
                    println!("Volume: {}%", status.volume);
                    Ok(())
                }
                Err(TunrError::Ipc(_)) => {
                    let p = paths.clone();
                    let vol = run_blocking(move || {
                        let cfg = paths::load_workspace_config(&p)?;
                        Ok(paths::effective_volume(&cfg))
                    })
                    .await?;
                    println!("Volume: {vol}%");
                    Ok(())
                }
                Err(err) => Err(err),
            },
        },
        Command::Delete { target, kind } => {
            let lib_path = paths.library_path.clone();
            let outcome = run_blocking(move || {
                Library::update(&lib_path, |library| library.delete_target(&target, kind))
            })
            .await?;
            match outcome {
                DeleteOutcome::Song(song) => {
                    println!("Deleted song '{}' from library.", song.title);
                }
                DeleteOutcome::Playlist(playlist) => {
                    println!("Deleted playlist '{}' from library.", playlist.name);
                }
                DeleteOutcome::PlaylistSong {
                    playlist,
                    song,
                    index,
                } => {
                    println!(
                        "Removed song '{}' (#{}) from playlist '{}' (kept in library).",
                        song.title, index, playlist.name
                    );
                }
            }
            Ok(())
        }
        Command::Create { playlist, songs } => {
            let lib_path = paths.library_path.clone();
            let created = run_blocking(move || {
                Library::update(&lib_path, |library| {
                    library.create_playlist(&playlist, &songs)
                })
            })
            .await?;
            println!(
                "Created playlist '{}' with {} song(s).",
                created.name,
                created.song_ids.len()
            );
            Ok(())
        }
        Command::Pause => {
            let status = ipc::send_request(&paths, &IpcRequest::SetPaused(true)).await?;
            print_playback_status(&status);
            Ok(())
        }
        Command::Resume => {
            let status = ipc::send_request(&paths, &IpcRequest::SetPaused(false)).await?;
            print_playback_status(&status);
            Ok(())
        }
        Command::Stop => {
            let status = ipc::send_request(&paths, &IpcRequest::Stop).await?;
            print_playback_status(&status);
            Ok(())
        }
        Command::Status => match ipc::send_request(&paths, &IpcRequest::GetStatus).await {
            Ok(status) => {
                print_playback_status(&status);
                Ok(())
            }
            Err(TunrError::Ipc(_)) => {
                println!("State: stopped (daemon not running)");
                Ok(())
            }
            Err(err) => Err(err),
        },
        Command::DaemonStop => match ipc::send_request(&paths, &IpcRequest::Shutdown).await {
            Ok(_) => {
                println!("Stopped playback daemon.");
                Ok(())
            }
            Err(TunrError::Ipc(_)) => {
                println!("Playback daemon is not running.");
                Ok(())
            }
            Err(err) => Err(err),
        },
        Command::List { kind } => {
            let lib_path = paths.library_path.clone();
            let library = run_blocking(move || Library::load(&lib_path)).await?;
            print_library_list(&library, kind);
            Ok(())
        }
        Command::Update => {
            let summary = run_blocking(move || execute_workspace_update(&paths)).await?;
            println!("{}", format_update_summary(&summary));
            for skipped in summary.skipped_files {
                eprintln!("warning: skipped {skipped}");
            }
            Ok(())
        }
        Command::Download(opts) => {
            let opts_for_plan = opts.clone();
            let plan =
                run_blocking(move || download::resolve_download_plan(&opts_for_plan)).await?;
            let outcome = if plan.is_empty() {
                download::run_download_plan(&paths, &opts, &plan).await?
            } else if !opts.no_tui
                && std::io::stdout().is_terminal()
                && std::io::stdin().is_terminal()
            {
                let p = paths.clone();
                let theme_outcome =
                    run_blocking(move || Ok(paths::load_active_theme_for_paths(Some(&p)))).await?;
                download_tui::run_download_tui_plan(
                    &paths,
                    &opts,
                    &plan,
                    &theme_outcome.theme,
                    theme_outcome.warning,
                )
                .await?
            } else {
                download_tui::run_download_plan_with_line_progress(&paths, &opts, &plan).await?
            };
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
            for warning in &outcome.warnings {
                eprintln!("warning: {warning}");
            }
            for failure in &outcome.failed {
                eprintln!("download failed for {}: {}", failure.url, failure.error);
            }
            if !outcome.failed.is_empty() {
                return Err(TunrError::Download(format!(
                    "{} download(s) failed ({} succeeded)",
                    outcome.failed.len(),
                    outcome.downloaded_files.len()
                )));
            }
            Ok(())
        }
    }
}

fn format_theme_list(file: &ThemesFile) -> String {
    let mut out = String::new();
    for name in file.available_names() {
        let marker = if name == file.active { "*" } else { " " };
        out.push_str(&format!("{marker} {name}\n"));
    }
    out
}

fn execute_theme_action_with_paths<W: Write, E: Write>(
    action: ThemeAction,
    paths_opt: Option<&AppPaths>,
    global_path: &Path,
    out: &mut W,
    err_out: &mut E,
) -> Result<()> {
    let target = match paths_opt {
        Some(p) => p.themes_path.clone(),
        None => global_path.to_path_buf(),
    };

    match action {
        ThemeAction::Path => {
            paths::ensure_themes_file(&target)?;
            writeln!(out, "{}", target.display())?;
        }
        ThemeAction::List => {
            paths::ensure_themes_file(&target)?;
            let file = paths::load_themes_file(&target)?;
            write!(out, "{}", format_theme_list(&file))?;
        }
        ThemeAction::Set { name } => {
            let chosen = paths::set_active_theme(&target, &name)?;
            writeln!(out, "Active theme: {chosen}")?;
        }
        ThemeAction::Current => {
            match paths::resolve_existing_themes_path_with_global(paths_opt, Some(global_path)) {
                Some(existing) => {
                    let file = paths::load_themes_file(&existing)?;
                    let (_theme, warn) = file.resolve_active();
                    if let Some(w) = warn {
                        writeln!(err_out, "warning: {w}")?;
                    }
                    writeln!(out, "{}", file.active)?;
                }
                None => {
                    writeln!(out, "{DEFAULT_THEME_NAME}")?;
                }
            }
        }
    }
    Ok(())
}

fn execute_workspace_update(paths: &AppPaths) -> Result<UpdateSummary> {
    let cfg = paths::load_workspace_config(paths)?;
    Library::update_workspace(
        &paths.library_path,
        &paths.working_dir,
        &cfg.config_dir,
        &cfg.playlists_dir,
        cfg.download_dest.as_deref(),
    )
}

fn format_update_summary(summary: &UpdateSummary) -> String {
    if summary.added_count == 0 && summary.relinked_count == 0 && summary.removed_count == 0 {
        format!("Library is up to date ({} song(s)).", summary.unchanged_count)
    } else {
        format!(
            "Updated library: {} added, {} relinked, {} removed ({} unchanged).",
            summary.added_count,
            summary.relinked_count,
            summary.removed_count,
            summary.unchanged_count,
        )
    }
}

fn print_playback_status(status: &PlaybackStatus) {
    let text = format_playback_status(status);
    println!("{text}");
}

fn format_playback_status(status: &PlaybackStatus) -> String {
    if status.state == PlaybackState::Stopped || status.current_track.is_none() {
        return format!(
            "State: {} [mode: {} | vol: {}%]",
            status.state, status.mode, status.volume
        );
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
    let loop_label = if status.loop_playback {
        " | loop: on"
    } else {
        ""
    };
    let total = status
        .duration_secs
        .or(track.duration_secs)
        .map(format_time)
        .unwrap_or_else(|| "--:--".to_string());

    format!(
        "[{}] {}{} ({}/{}) {} / {} [mode: {}{}{} | vol: {}%]",
        status.state,
        track.title,
        artist,
        status.queue_index + 1,
        status.queue_len,
        format_time(status.position_secs),
        total,
        status.mode,
        playlist,
        loop_label,
        status.volume,
    )
}

fn print_library_list(library: &Library, kind: Option<ListKind>) {
    let stdout = std::io::stdout();
    let mut writer = stdout.lock();
    let _ = write_library_list(library, kind, &mut writer);
}

fn write_library_list<W: Write>(
    library: &Library,
    kind: Option<ListKind>,
    writer: &mut W,
) -> std::io::Result<()> {
    let show_songs = matches!(kind, None | Some(ListKind::Songs));
    let show_playlists = matches!(kind, None | Some(ListKind::Playlists));

    if show_songs {
        writeln!(writer, "Songs ({}):", library.songs.len())?;
        if library.songs.is_empty() {
            writeln!(writer, "  (none)")?;
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
                writeln!(writer, "  {}. {}{} [{}]", idx + 1, song.title, artist, dur)?;
            }
        }
    }

    if show_playlists {
        if show_songs {
            writeln!(writer)?;
        }
        writeln!(writer, "Playlists ({}):", library.playlists.len())?;
        if library.playlists.is_empty() {
            writeln!(writer, "  (none)")?;
        } else {
            for (idx, playlist) in library.playlists.iter().enumerate() {
                let titles: Vec<&str> = playlist
                    .song_ids
                    .iter()
                    .filter_map(|id| library.find_song_by_id(id).map(|s| s.title.as_str()))
                    .collect();
                writeln!(
                    writer,
                    "  {}. {} ({} song(s)): {}",
                    idx + 1,
                    playlist.name,
                    titles.len(),
                    if titles.is_empty() {
                        "(empty)".to_string()
                    } else {
                        titles.join(", ")
                    }
                )?;
            }
        }
    }

    Ok(())
}

fn prompt_song_selection<R: BufRead, W: Write>(
    query: &str,
    candidates: &[SongCandidate],
    reader: &mut R,
    writer: &mut W,
) -> Result<usize> {
    if candidates.is_empty() {
        return Err(TunrError::NotFound(format!(
            "no song found matching '{query}'"
        )));
    }

    writeln!(writer, "Multiple tracks match '{query}':")?;
    for (i, candidate) in candidates.iter().enumerate() {
        let artist = candidate.artist.as_deref().unwrap_or("Unknown Artist");
        let dur = candidate
            .duration_secs
            .map(format_time)
            .unwrap_or_else(|| "--:--".to_string());
        writeln!(writer, "  {}. {} — {} [{}]", i + 1, candidate.title, artist, dur)?;
    }

    loop {
        write!(writer, "Select [1-{}, q to cancel]: ", candidates.len())?;
        writer.flush()?;

        let mut line = String::new();
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 {
            return Err(TunrError::InvalidArg(
                "selection cancelled".to_string(),
            ));
        }

        let trimmed = line.trim();
        if trimmed.eq_ignore_ascii_case("q") || trimmed.eq_ignore_ascii_case("quit") {
            return Err(TunrError::InvalidArg(
                "selection cancelled".to_string(),
            ));
        }

        if let Ok(choice) = trimmed.parse::<usize>()
            && (1..=candidates.len()).contains(&choice)
        {
            return Ok(candidates[choice - 1].index);
        }

        writeln!(
            writer,
            "Invalid selection '{}'; enter 1-{} or 'q' to cancel.",
            trimmed,
            candidates.len()
        )?;
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

#[cfg(target_os = "android")]
fn is_android_shared_storage(path: &std::path::Path) -> bool {
    let raw = path.to_string_lossy();
    raw.starts_with("/sdcard")
        || raw.starts_with("/storage/emulated")
        || raw.starts_with("/storage/self")
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::path::PathBuf;

    use super::*;

    fn sample_candidates() -> Vec<SongCandidate> {
        vec![
            SongCandidate {
                index: 2,
                title: "Sunrise".to_string(),
                artist: Some("Artist A".to_string()),
                album: None,
                path: PathBuf::from("/music/a.mp3"),
                duration_secs: Some(185.0),
            },
            SongCandidate {
                index: 7,
                title: "Sunrise".to_string(),
                artist: None,
                album: None,
                path: PathBuf::from("/music/b.mp3"),
                duration_secs: None,
            },
        ]
    }

    #[test]
    fn prompt_song_selection_accepts_valid_choice_and_formats_candidates() {
        let candidates = sample_candidates();
        let mut reader = Cursor::new(b"2\n");
        let mut out = Vec::new();

        let chosen = prompt_song_selection("Sunrise", &candidates, &mut reader, &mut out).unwrap();
        assert_eq!(chosen, 7);

        let rendered = String::from_utf8(out).unwrap();
        assert!(rendered.contains("  1. Sunrise — Artist A [03:05]"));
        assert!(rendered.contains("  2. Sunrise — Unknown Artist [--:--]"));
        assert!(rendered.contains("Select [1-2, q to cancel]: "));
    }

    #[test]
    fn prompt_song_selection_reprompts_on_invalid_input() {
        let candidates = sample_candidates();
        let mut reader = Cursor::new(b"0\nabc\n99\n1\n");
        let mut out = Vec::new();

        let chosen = prompt_song_selection("Sunrise", &candidates, &mut reader, &mut out).unwrap();
        assert_eq!(chosen, 2);

        let rendered = String::from_utf8(out).unwrap();
        assert_eq!(rendered.matches("Select [1-2, q to cancel]: ").count(), 4);
    }

    #[test]
    fn prompt_song_selection_cancels_on_q_quit_and_eof() {
        let candidates = sample_candidates();

        for input in [b"q\n".as_slice(), b"QUIT\n".as_slice(), b"".as_slice()] {
            let mut reader = Cursor::new(input);
            let mut out = Vec::new();
            let err =
                prompt_song_selection("Sunrise", &candidates, &mut reader, &mut out).unwrap_err();
            assert!(
                matches!(err, TunrError::InvalidArg(ref msg) if msg == "selection cancelled"),
                "unexpected error for input {input:?}: {err:?}"
            );
        }
    }

    fn sample_library() -> Library {
        use tunr::library::{Playlist, Song};

        Library {
            songs: vec![
                Song {
                    id: "s1".to_string(),
                    title: "First Track".to_string(),
                    artist: Some("Artist One".to_string()),
                    album: None,
                    path: PathBuf::from("/music/first.mp3"),
                    duration_secs: Some(125.0),
                },
                Song {
                    id: "s2".to_string(),
                    title: "Second Track".to_string(),
                    artist: None,
                    album: None,
                    path: PathBuf::from("/music/second.mp3"),
                    duration_secs: None,
                },
            ],
            playlists: vec![
                Playlist {
                    name: "Chill".to_string(),
                    song_ids: vec!["s1".to_string(), "s2".to_string()],
                },
                Playlist {
                    name: "EmptyMix".to_string(),
                    song_ids: Vec::new(),
                },
            ],
            ..Library::default()
        }
    }

    #[test]
    fn write_library_list_renders_all_with_one_based_numbering() {
        let library = sample_library();
        let mut out = Vec::new();

        write_library_list(&library, None, &mut out).unwrap();
        let rendered = String::from_utf8(out).unwrap();

        let expected = "\
Songs (2):
  1. First Track - Artist One [02:05]
  2. Second Track [--:--]

Playlists (2):
  1. Chill (2 song(s)): First Track, Second Track
  2. EmptyMix (0 song(s)): (empty)
";
        assert_eq!(rendered, expected);
    }

    #[test]
    fn write_library_list_filters_songs_and_playlists() {
        let library = sample_library();

        let mut songs_out = Vec::new();
        write_library_list(&library, Some(ListKind::Songs), &mut songs_out).unwrap();
        let songs_rendered = String::from_utf8(songs_out).unwrap();
        assert_eq!(
            songs_rendered,
            "\
Songs (2):
  1. First Track - Artist One [02:05]
  2. Second Track [--:--]
"
        );

        let mut playlists_out = Vec::new();
        write_library_list(&library, Some(ListKind::Playlists), &mut playlists_out).unwrap();
        let playlists_rendered = String::from_utf8(playlists_out).unwrap();
        assert_eq!(
            playlists_rendered,
            "\
Playlists (2):
  1. Chill (2 song(s)): First Track, Second Track
  2. EmptyMix (0 song(s)): (empty)
"
        );
    }

    #[test]
    fn write_library_list_handles_empty_library_sections() {
        let empty = Library::default();
        let mut out = Vec::new();

        write_library_list(&empty, None, &mut out).unwrap();
        let rendered = String::from_utf8(out).unwrap();
        assert_eq!(
            rendered,
            "\
Songs (0):
  (none)

Playlists (0):
  (none)
"
        );
    }

    #[test]
    fn format_playback_status_renders_loop_flag_with_and_without_playlist() {
        use tunr::ipc::PlayMode;
        use tunr::library::Song;

        let track = Song {
            id: "s1".to_string(),
            title: "First Track".to_string(),
            artist: Some("Artist One".to_string()),
            album: None,
            path: PathBuf::from("/music/first.mp3"),
            duration_secs: Some(125.0),
        };
        let base = PlaybackStatus {
            state: PlaybackState::Playing,
            mode: PlayMode::Next,
            playlist_name: None,
            loop_playback: false,
            volume: 50,
            current_track: Some(track.clone()),
            queue_index: 0,
            queue_len: 2,
            position_secs: 15.0,
            duration_secs: Some(125.0),
            queue: vec![track],
        };

        assert_eq!(
            format_playback_status(&base),
            "[playing] First Track - Artist One (1/2) 00:15 / 02:05 [mode: next | vol: 50%]"
        );

        let loop_no_playlist = PlaybackStatus {
            loop_playback: true,
            ..base.clone()
        };
        assert_eq!(
            format_playback_status(&loop_no_playlist),
            "[playing] First Track - Artist One (1/2) 00:15 / 02:05 [mode: next | loop: on | vol: 50%]"
        );

        let no_loop_with_playlist = PlaybackStatus {
            playlist_name: Some("Mix".to_string()),
            loop_playback: false,
            ..base.clone()
        };
        assert_eq!(
            format_playback_status(&no_loop_with_playlist),
            "[playing] First Track - Artist One (1/2) 00:15 / 02:05 [mode: next | playlist: Mix | vol: 50%]"
        );

        let loop_with_playlist = PlaybackStatus {
            playlist_name: Some("Mix".to_string()),
            loop_playback: true,
            volume: 80,
            ..base.clone()
        };
        assert_eq!(
            format_playback_status(&loop_with_playlist),
            "[playing] First Track - Artist One (1/2) 00:15 / 02:05 [mode: next | playlist: Mix | loop: on | vol: 80%]"
        );

        let stopped = PlaybackStatus {
            state: PlaybackState::Stopped,
            loop_playback: true,
            volume: 35,
            current_track: None,
            ..base
        };
        assert_eq!(
            format_playback_status(&stopped),
            "State: stopped [mode: next | vol: 35%]"
        );
    }

    struct TempWorkspaceGuard {
        root: PathBuf,
    }

    impl TempWorkspaceGuard {
        fn new(label: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "tunr-main-test-{label}-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4()
            ));
            std::fs::create_dir_all(&root).unwrap();
            Self { root }
        }
    }

    impl Drop for TempWorkspaceGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn write_test_wav(path: &std::path::Path, sample_rate: u32, duration_secs: f64) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let channels: u16 = 1;
        let bits_per_sample: u16 = 16;
        let num_frames = (sample_rate as f64 * duration_secs).round() as u32;
        let block_align = channels * (bits_per_sample / 8);
        let byte_rate = sample_rate * u32::from(block_align);
        let data_size = num_frames * u32::from(block_align);
        let riff_chunk_size = 36 + data_size;

        let mut buf = Vec::with_capacity((44 + data_size) as usize);
        buf.extend_from_slice(b"RIFF");
        buf.extend_from_slice(&riff_chunk_size.to_le_bytes());
        buf.extend_from_slice(b"WAVE");
        buf.extend_from_slice(b"fmt ");
        buf.extend_from_slice(&16u32.to_le_bytes());
        buf.extend_from_slice(&1u16.to_le_bytes());
        buf.extend_from_slice(&channels.to_le_bytes());
        buf.extend_from_slice(&sample_rate.to_le_bytes());
        buf.extend_from_slice(&byte_rate.to_le_bytes());
        buf.extend_from_slice(&block_align.to_le_bytes());
        buf.extend_from_slice(&bits_per_sample.to_le_bytes());
        buf.extend_from_slice(b"data");
        buf.extend_from_slice(&data_size.to_le_bytes());
        for i in 0..num_frames {
            let t = i as f32 / sample_rate as f32;
            let sample = ((2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.25 * i16::MAX as f32)
                as i16;
            buf.extend_from_slice(&sample.to_le_bytes());
        }
        std::fs::write(path, buf).unwrap();
    }

    #[test]
    fn format_update_summary_distinguishes_up_to_date_and_changed_runs() {
        let up_to_date = UpdateSummary {
            added_count: 0,
            relinked_count: 0,
            removed_count: 0,
            unchanged_count: 4,
            skipped_files: Vec::new(),
        };
        assert_eq!(
            format_update_summary(&up_to_date),
            "Library is up to date (4 song(s))."
        );

        let changed = UpdateSummary {
            added_count: 2,
            relinked_count: 1,
            removed_count: 3,
            unchanged_count: 5,
            skipped_files: vec!["bad.wav: invalid".to_string()],
        };
        assert_eq!(
            format_update_summary(&changed),
            "Updated library: 2 added, 1 relinked, 3 removed (5 unchanged)."
        );
    }

    #[test]
    fn execute_workspace_update_reconciles_initialized_workspace() {
        let guard = TempWorkspaceGuard::new("update");
        let xdg_dir = guard.root.join("xdg");
        let ws_dir = guard.root.join("ws");
        let runtime_dir = guard.root.join("run");
        std::fs::create_dir_all(&xdg_dir).unwrap();
        std::fs::create_dir_all(&runtime_dir).unwrap();

        let cfg = paths::initialize_workspace_with_env(
            &ws_dir,
            &guard.root,
            Some(xdg_dir.into_os_string()),
            None,
        )
        .unwrap();
        let paths = AppPaths::from_roots(&cfg.working_dir, &runtime_dir);
        Library::update(&paths.library_path, |_| Ok(())).unwrap();

        let track_a = cfg.music_dir.join("TrackA.wav");
        let track_b = cfg.music_dir.join("TrackB.wav");
        write_test_wav(&track_a, 22_050, 0.25);
        write_test_wav(&track_b, 22_050, 0.30);

        let first = execute_workspace_update(&paths).unwrap();
        assert_eq!(first.added_count, 2);
        assert_eq!(first.relinked_count, 0);
        assert_eq!(first.removed_count, 0);
        assert_eq!(first.unchanged_count, 0);
        assert_eq!(
            format_update_summary(&first),
            "Updated library: 2 added, 0 relinked, 0 removed (0 unchanged)."
        );

        // Second run with no filesystem changes reports up-to-date.
        let second = execute_workspace_update(&paths).unwrap();
        assert_eq!(second.added_count, 0);
        assert_eq!(second.relinked_count, 0);
        assert_eq!(second.removed_count, 0);
        assert_eq!(second.unchanged_count, 2);
        assert_eq!(
            format_update_summary(&second),
            "Library is up to date (2 song(s))."
        );

        // Move TrackA into a subdirectory and delete TrackB.
        let moved_a = cfg.music_dir.join("Archive").join("TrackA.wav");
        std::fs::create_dir_all(moved_a.parent().unwrap()).unwrap();
        std::fs::rename(&track_a, &moved_a).unwrap();
        std::fs::remove_file(&track_b).unwrap();

        let third = execute_workspace_update(&paths).unwrap();
        assert_eq!(third.added_count, 0);
        assert_eq!(third.relinked_count, 1);
        assert_eq!(third.removed_count, 1);
        assert_eq!(third.unchanged_count, 0);
        assert_eq!(
            format_update_summary(&third),
            "Updated library: 0 added, 1 relinked, 1 removed (0 unchanged)."
        );

        let lib = Library::load(&paths.library_path).unwrap();
        assert_eq!(lib.songs.len(), 1);
        assert_eq!(lib.songs[0].path, std::fs::canonicalize(&moved_a).unwrap());
    }

    #[test]
    fn theme_command_handlers_work_initialized_and_uninitialized() {
        let guard = TempWorkspaceGuard::new("theme-cmd");
        let global_path = guard.root.join("global").join("themes.json");
        let ws_paths = AppPaths::from_roots(guard.root.join("ws"), guard.root.join("run"));

        // Uninitialized `current` reports default without creating global themes.json.
        let mut out = Vec::new();
        let mut err = Vec::new();
        execute_theme_action_with_paths(
            ThemeAction::Current,
            None,
            &global_path,
            &mut out,
            &mut err,
        )
        .unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "tunr-dark\n");
        assert!(err.is_empty());
        assert!(!global_path.exists());

        // Uninitialized `set` and `list` lazily create global themes.json.
        let mut out = Vec::new();
        let mut err = Vec::new();
        execute_theme_action_with_paths(
            ThemeAction::Set {
                name: "nord".to_string(),
            },
            None,
            &global_path,
            &mut out,
            &mut err,
        )
        .unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "Active theme: nord\n");
        assert!(global_path.exists());

        let mut out = Vec::new();
        let mut err = Vec::new();
        execute_theme_action_with_paths(
            ThemeAction::List,
            None,
            &global_path,
            &mut out,
            &mut err,
        )
        .unwrap();
        let listed = String::from_utf8(out).unwrap();
        assert!(listed.contains("* nord\n"), "list output: {listed}");
        assert!(listed.contains("  tunr-dark\n"));
        assert!(listed.contains("  tunr-light\n"));
        assert!(listed.contains("  rose-pine\n"));
        assert!(listed.contains("  dracula\n"));

        // Legacy initialized workspace without config/themes.json: `current` reads global without shadowing!
        let mut out = Vec::new();
        let mut err = Vec::new();
        execute_theme_action_with_paths(
            ThemeAction::Current,
            Some(&ws_paths),
            &global_path,
            &mut out,
            &mut err,
        )
        .unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "nord\n");
        assert!(!ws_paths.themes_path.exists());

        // Workspace `path` & `set` target workspace themes.json independently of global.
        let mut out = Vec::new();
        let mut err = Vec::new();
        execute_theme_action_with_paths(
            ThemeAction::Path,
            Some(&ws_paths),
            &global_path,
            &mut out,
            &mut err,
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap().trim(),
            ws_paths.themes_path.to_string_lossy()
        );
        assert!(ws_paths.themes_path.exists());

        let mut out = Vec::new();
        let mut err = Vec::new();
        execute_theme_action_with_paths(
            ThemeAction::Set {
                name: "rose-pine".to_string(),
            },
            Some(&ws_paths),
            &global_path,
            &mut out,
            &mut err,
        )
        .unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "Active theme: rose-pine\n");

        let mut out = Vec::new();
        let mut err = Vec::new();
        execute_theme_action_with_paths(
            ThemeAction::Current,
            Some(&ws_paths),
            &global_path,
            &mut out,
            &mut err,
        )
        .unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "rose-pine\n");
    }
}
