use std::fs;
use std::io;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Clear, Gauge, HighlightSpacing, List, ListItem, ListState, Paragraph, Wrap,
};
use tokio::sync::mpsc;
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::MissedTickBehavior;

use crate::error::{Result, TunrError};
use crate::ipc::{self, IpcRequest, PlayMode, PlaybackState, PlaybackStatus, TargetKind};
use crate::library::{DeleteOutcome, Library, Song};
use crate::paths::{self, AppPaths, DEFAULT_VOLUME, MAX_VOLUME};
use crate::platform::ShutdownSignals;
use crate::tui_common::{TerminalSession, fit_to_width, use_ascii_glyphs};

const SYNC_INTERVAL: Duration = Duration::from_millis(150);
const SEEK_STEP_SECS: f64 = 5.0;
const VOLUME_STEP: u8 = 5;
const STATUS_TTL: Duration = Duration::from_secs(5);
const STOP_ON_QUIT_TIMEOUT: Duration = Duration::from_secs(1);

/// Active UI pane receiving navigation and selection keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusPane {
    Playlists,
    Tracks,
}

/// Pending destructive library action awaiting Y/N confirmation in the modal dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmDelete {
    pub label: String,
    pub query: String,
    pub kind: TargetKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StatusMessage {
    text: String,
    is_error: bool,
    expires_at: Option<Instant>,
}

/// Asynchronous or blocking side effects emitted by [`TuiApp`] for the async runtime driver.
#[derive(Debug, Clone, PartialEq)]
pub enum TuiEffect {
    PollStatus {
        generation: u64,
    },
    EnsureAndSend {
        generation: u64,
        request: IpcRequest,
    },
    SendControl {
        generation: u64,
        request: IpcRequest,
    },
    SetVolume {
        generation: u64,
        volume: u8,
    },
    StopOnQuit {
        generation: u64,
    },
    CheckLibraryReload {
        generation: u64,
        known_stamp: Option<(SystemTime, u64)>,
    },
    DeleteTarget {
        generation: u64,
        target: ConfirmDelete,
    },
}

/// Completion events delivered back into [`TuiApp::apply_event`] from worker tasks.
#[derive(Debug)]
pub enum TuiEvent {
    StatusPolled {
        generation: u64,
        result: Result<PlaybackStatus>,
    },
    ActionCompleted {
        generation: u64,
        is_play: bool,
        result: Result<PlaybackStatus>,
    },
    VolumeUpdated {
        generation: u64,
        result: Result<Option<PlaybackStatus>>,
    },
    LibraryChecked {
        generation: u64,
        observed_stamp: Option<(SystemTime, u64)>,
        result: Result<Option<Library>>,
    },
    DeleteFinished {
        generation: u64,
        result: Result<(Library, Option<(SystemTime, u64)>, DeleteOutcome)>,
    },
    WorkerError(String),
}

struct AbortGuard(JoinHandle<()>);

impl Drop for AbortGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

enum QueuedIpcAction {
    Ipc {
        generation: u64,
        ensure_daemon: bool,
        request: IpcRequest,
    },
    SetVolume {
        generation: u64,
        volume: u8,
    },
}

/// Synchronous state machine and renderer for the interactive Tunr terminal interface.
pub struct TuiApp {
    library: Library,
    library_stamp: Option<(SystemTime, u64)>,
    focus: FocusPane,
    playlist_state: ListState,
    track_state: ListState,
    mode: PlayMode,
    mode_locked: bool,
    volume: u8,
    volume_gen: u64,
    status: PlaybackStatus,
    confirm_delete: Option<ConfirmDelete>,
    status_message: Option<StatusMessage>,
    should_quit: bool,
    status_gen: u64,
    min_status_gen: u64,
    applied_status_gen: u64,
    poll_in_flight: bool,
    action_in_flight: usize,
    library_gen: u64,
    reload_in_flight: bool,
    delete_in_flight: bool,
    effects: Vec<TuiEffect>,
}

impl TuiApp {
    /// Load library state synchronously and build a [`TuiApp`].
    ///
    /// Async callers such as [`run_tui`] should prefer loading off-thread via `spawn_blocking`
    /// and calling [`TuiApp::from_loaded`].
    pub fn new(paths: AppPaths) -> Result<Self> {
        let library = Library::load(&paths.library_path)?;
        let library_stamp = file_stamp(&paths.library_path);
        let initial_volume = load_initial_volume(&paths);
        Ok(Self::from_loaded(library, library_stamp).with_volume(initial_volume))
    }

    /// Construct a [`TuiApp`] from pre-loaded library contents and file metadata stamp.
    pub fn from_loaded(library: Library, library_stamp: Option<(SystemTime, u64)>) -> Self {
        let mut playlist_state = ListState::default();
        playlist_state.select(Some(0));

        let mut track_state = ListState::default();
        if !library.songs.is_empty() {
            track_state.select(Some(0));
        }

        Self {
            library,
            library_stamp,
            focus: FocusPane::Playlists,
            playlist_state,
            track_state,
            mode: PlayMode::Next,
            mode_locked: false,
            volume: DEFAULT_VOLUME,
            volume_gen: 0,
            status: PlaybackStatus::default(),
            confirm_delete: None,
            status_message: None,
            should_quit: false,
            status_gen: 0,
            min_status_gen: 0,
            applied_status_gen: 0,
            poll_in_flight: false,
            action_in_flight: 0,
            library_gen: 0,
            reload_in_flight: false,
            delete_in_flight: false,
            effects: Vec::new(),
        }
    }

    /// Override the initial volume percentage shown by [`TuiApp`].
    pub fn with_volume(mut self, volume: u8) -> Self {
        let clamped = volume.min(MAX_VOLUME);
        self.volume = clamped;
        self.status.volume = clamped;
        self
    }

    /// Return the active local volume percentage (`0..=100`).
    pub fn volume(&self) -> u8 {
        self.volume
    }

    /// Return the currently focused pane.
    pub fn focus(&self) -> FocusPane {
        self.focus
    }

    /// Return the active local playback mode.
    pub fn mode(&self) -> PlayMode {
        self.mode
    }

    /// Return whether the TUI event loop should exit.
    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    /// Return the selected playlist index (`0` = `[All Songs]`, `1..` = `library.playlists`).
    pub fn selected_playlist_index(&self) -> usize {
        self.playlist_state.selected().unwrap_or(0)
    }

    /// Return the selected track index within the currently visible tracks list.
    pub fn selected_track_index(&self) -> Option<usize> {
        self.track_state.selected()
    }

    /// Return the name of the selected playlist, or `None` when `[All Songs]` is active.
    pub fn selected_playlist_name(&self) -> Option<String> {
        let idx = self.selected_playlist_index();
        if idx == 0 {
            None
        } else {
            self.library.playlists.get(idx - 1).map(|p| p.name.clone())
        }
    }

    /// Return the tracks visible in the right-hand pane for the currently highlighted playlist.
    pub fn visible_tracks(&self) -> Vec<Song> {
        let idx = self.selected_playlist_index();
        if idx == 0 {
            return self.library.songs.clone();
        }
        match self.library.playlists.get(idx - 1) {
            Some(pl) => self.library.songs_for_playlist(pl),
            None => Vec::new(),
        }
    }

    /// Drain all pending outbound side effects queued since the last drain.
    pub fn take_effects(&mut self) -> Vec<TuiEffect> {
        std::mem::take(&mut self.effects)
    }

    /// Request exit from the TUI event loop without stopping playback daemon.
    pub fn request_quit(&mut self) {
        self.should_quit = true;
    }

    /// Schedule a deduplicated background `library.json` metadata check/reload.
    ///
    /// Skips emitting an effect if a reload check or delete transaction is already in flight.
    pub fn request_library_check(&mut self) {
        if self.reload_in_flight || self.delete_in_flight || self.should_quit {
            return;
        }
        self.reload_in_flight = true;
        self.library_gen = self.library_gen.saturating_add(1);
        self.effects.push(TuiEffect::CheckLibraryReload {
            generation: self.library_gen,
            known_stamp: self.library_stamp,
        });
    }

    /// Schedule a deduplicated background `GetStatus` IPC poll.
    ///
    /// Skips emitting an effect while another `GetStatus` poll or playback command is in flight.
    pub fn request_poll_status(&mut self) {
        if self.poll_in_flight || self.action_in_flight > 0 || self.should_quit {
            return;
        }
        self.poll_in_flight = true;
        let generation = self.next_status_gen();
        self.effects.push(TuiEffect::PollStatus { generation });
    }

    /// Apply an asynchronous worker completion event to the UI state.
    pub fn apply_event(&mut self, event: TuiEvent) {
        match event {
            TuiEvent::StatusPolled { generation, result } => {
                self.poll_in_flight = false;
                if generation < self.min_status_gen
                    || generation <= self.applied_status_gen
                    || self.action_in_flight > 0
                {
                    return;
                }
                self.applied_status_gen = generation;
                match result {
                    Ok(status) => self.apply_live_status(status, generation, false),
                    Err(err) if is_daemon_offline_error(&err) => {
                        self.reset_status_stopped();
                    }
                    Err(err) => {
                        self.reset_status_stopped();
                        self.set_message(format!("Status error: {err}"), true);
                    }
                }
            }
            TuiEvent::ActionCompleted {
                generation,
                is_play,
                result,
            } => {
                self.action_in_flight = self.action_in_flight.saturating_sub(1);
                if generation <= self.applied_status_gen {
                    return;
                }
                self.applied_status_gen = generation;
                match result {
                    Ok(status) => self.apply_live_status(status, generation, true),
                    Err(err) if !is_play && is_daemon_offline_error(&err) => {
                        self.reset_status_stopped();
                        self.set_message("Playback daemon is not running.", false);
                    }
                    Err(err) => {
                        let prefix = if is_play { "Play failed: " } else { "" };
                        self.set_message(format!("{prefix}{err}"), true);
                    }
                }
            }
            TuiEvent::VolumeUpdated { generation, result } => {
                self.action_in_flight = self.action_in_flight.saturating_sub(1);
                if generation <= self.applied_status_gen {
                    return;
                }
                self.applied_status_gen = generation;
                match result {
                    Ok(Some(status)) => self.apply_live_status(status, generation, true),
                    Ok(None) => {
                        self.reset_status_stopped();
                        self.status_message = None;
                    }
                    Err(err) => {
                        self.set_message(format!("Volume update failed: {err}"), true);
                    }
                }
            }
            TuiEvent::LibraryChecked {
                generation,
                observed_stamp,
                result,
            } => {
                self.reload_in_flight = false;
                if self.delete_in_flight || generation < self.library_gen {
                    return;
                }
                match result {
                    Ok(None) => {}
                    Ok(Some(new_lib)) => {
                        let prev_playlist_name = self.selected_playlist_name();
                        let prev_track_id = self.selected_track().map(|s| s.id);
                        let prev_track_idx = self.track_state.selected();
                        self.library = new_lib;
                        self.library_stamp = observed_stamp;
                        self.restore_selection(
                            prev_playlist_name.as_deref(),
                            prev_track_id.as_deref(),
                            prev_track_idx,
                        );
                    }
                    Err(err) => {
                        self.library_stamp = observed_stamp;
                        self.set_message(format!("Library reload failed: {err}"), true);
                    }
                }
            }
            TuiEvent::DeleteFinished { generation, result } => {
                self.delete_in_flight = false;
                if generation < self.library_gen {
                    return;
                }
                match result {
                    Ok((new_lib, new_stamp, outcome)) => {
                        let prev_playlist_name = self.selected_playlist_name();
                        let prev_track_id = self.selected_track().map(|s| s.id);
                        let prev_track_idx = self.track_state.selected();
                        self.library = new_lib;
                        self.library_stamp = new_stamp;
                        self.restore_selection(
                            prev_playlist_name.as_deref(),
                            prev_track_id.as_deref(),
                            prev_track_idx,
                        );
                        match outcome {
                            DeleteOutcome::Song(song) => {
                                self.set_message(
                                    format!("Removed song '{}' from library.", song.title),
                                    false,
                                );
                            }
                            DeleteOutcome::Playlist(pl) => {
                                self.set_message(
                                    format!("Deleted playlist '{}'.", pl.name),
                                    false,
                                );
                            }
                            DeleteOutcome::PlaylistSong {
                                playlist,
                                song,
                                index,
                            } => {
                                self.set_message(
                                    format!(
                                        "Removed song '{}' (#{}) from playlist '{}'.",
                                        song.title, index, playlist.name
                                    ),
                                    false,
                                );
                            }
                        }
                    }
                    Err(err) => {
                        self.set_message(format!("Delete failed: {err}"), true);
                    }
                }
            }
            TuiEvent::WorkerError(msg) => {
                self.poll_in_flight = false;
                self.reload_in_flight = false;
                self.delete_in_flight = false;
                self.set_message(msg, true);
            }
        }
    }

    /// Handle a single terminal key event.
    ///
    /// Navigation, seek, and volume keys respond to both `Press` and `Repeat` events, while
    /// state-toggling shortcuts, modal confirmations, and quit keys act only on `Press`.
    /// Release events are ignored.
    pub fn handle_key(&mut self, key: KeyEvent) {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return;
        }
        let is_press = key.kind == KeyEventKind::Press;

        if key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT)
            && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
        {
            if is_press {
                self.should_quit = true;
            }
            return;
        }

        if key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
            #[cfg(windows)]
            let allow_altgr_char = key
                .modifiers
                .contains(KeyModifiers::CONTROL | KeyModifiers::ALT)
                && matches!(key.code, KeyCode::Char(ch) if !ch.is_ascii_alphabetic());
            #[cfg(not(windows))]
            let allow_altgr_char = false;

            if !allow_altgr_char {
                return;
            }
        }

        if self.confirm_delete.is_some() {
            if is_press {
                self.handle_modal_key(key);
            }
            return;
        }

        match key.code {
            KeyCode::Left | KeyCode::Char('h') => {
                self.focus = FocusPane::Playlists;
            }
            KeyCode::Right | KeyCode::Char('l') => {
                self.focus = FocusPane::Tracks;
                self.ensure_track_selected();
            }
            KeyCode::Tab | KeyCode::BackTab if is_press => {
                self.focus = match self.focus {
                    FocusPane::Playlists => {
                        self.ensure_track_selected();
                        FocusPane::Tracks
                    }
                    FocusPane::Tracks => FocusPane::Playlists,
                };
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_up(),
            KeyCode::Down | KeyCode::Char('j') => self.move_down(),
            KeyCode::Enter if is_press => self.handle_enter(),
            KeyCode::Char('p') if is_press => self.handle_play_shortcut(),
            KeyCode::Char(' ') if is_press => self.send_control(IpcRequest::TogglePause),
            KeyCode::Char('n') if is_press => self.send_control(IpcRequest::Next),
            KeyCode::Char('b') if is_press => self.send_control(IpcRequest::Prev),
            KeyCode::Char('[') => self.send_control(IpcRequest::SeekRelative(-SEEK_STEP_SECS)),
            KeyCode::Char(']') => self.send_control(IpcRequest::SeekRelative(SEEK_STEP_SECS)),
            KeyCode::Char('-') => self.adjust_volume(-(VOLUME_STEP as i16)),
            KeyCode::Char('+') | KeyCode::Char('=') => self.adjust_volume(VOLUME_STEP as i16),
            KeyCode::Char('s') if is_press => self.toggle_mode(),
            KeyCode::Char('d') if is_press => self.open_delete_modal(),
            KeyCode::Char('q') | KeyCode::Esc if is_press => {
                self.should_quit = true;
            }
            KeyCode::Char('Q') if is_press => {
                let generation = self.next_status_gen();
                self.min_status_gen = generation;
                self.should_quit = true;
                self.effects.push(TuiEffect::StopOnQuit { generation });
            }
            _ => {}
        }
    }

    fn handle_modal_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                self.execute_pending_delete();
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.confirm_delete = None;
                self.set_message("Delete cancelled.", false);
            }
            _ => {}
        }
    }

    fn move_up(&mut self) {
        match self.focus {
            FocusPane::Playlists => {
                let len = self.playlist_count();
                if len == 0 {
                    self.playlist_state.select(None);
                    return;
                }
                let next = self
                    .playlist_state
                    .selected()
                    .map_or(0, |i| i.saturating_sub(1))
                    .min(len - 1);
                if self.playlist_state.selected() != Some(next) {
                    self.playlist_state.select(Some(next));
                    self.reset_track_selection();
                }
            }
            FocusPane::Tracks => {
                let len = self.visible_tracks().len();
                if len == 0 {
                    self.track_state.select(None);
                    return;
                }
                let next = self
                    .track_state
                    .selected()
                    .map_or(0, |i| i.saturating_sub(1))
                    .min(len - 1);
                self.track_state.select(Some(next));
            }
        }
    }

    fn move_down(&mut self) {
        match self.focus {
            FocusPane::Playlists => {
                let len = self.playlist_count();
                if len == 0 {
                    self.playlist_state.select(None);
                    return;
                }
                let next = self
                    .playlist_state
                    .selected()
                    .map_or(0, |i| (i + 1).min(len - 1));
                if self.playlist_state.selected() != Some(next) {
                    self.playlist_state.select(Some(next));
                    self.reset_track_selection();
                }
            }
            FocusPane::Tracks => {
                let len = self.visible_tracks().len();
                if len == 0 {
                    self.track_state.select(None);
                    return;
                }
                let next = self
                    .track_state
                    .selected()
                    .map_or(0, |i| (i + 1).min(len - 1));
                self.track_state.select(Some(next));
            }
        }
    }

    fn handle_enter(&mut self) {
        match self.focus {
            FocusPane::Playlists => {
                if self.visible_tracks().is_empty() {
                    self.set_message("Selected view has no tracks.", false);
                    return;
                }
                self.focus = FocusPane::Tracks;
                self.ensure_track_selected();
            }
            FocusPane::Tracks => {
                let queue = self.visible_tracks();
                if queue.is_empty() {
                    self.set_message("No tracks to play.", false);
                    return;
                }
                let start_index = self
                    .track_state
                    .selected()
                    .unwrap_or(0)
                    .min(queue.len() - 1);
                self.play_queue(queue, Some(start_index), self.selected_playlist_name());
            }
        }
    }

    fn handle_play_shortcut(&mut self) {
        let queue = self.visible_tracks();
        if queue.is_empty() {
            self.set_message("Selected playlist has no tracks to play.", false);
            return;
        }
        let start_index = match self.focus {
            FocusPane::Playlists => None,
            FocusPane::Tracks => Some(
                self.track_state
                    .selected()
                    .unwrap_or(0)
                    .min(queue.len() - 1),
            ),
        };
        self.play_queue(queue, start_index, self.selected_playlist_name());
    }

    fn play_queue(
        &mut self,
        queue: Vec<Song>,
        start_index: Option<usize>,
        playlist_name: Option<String>,
    ) {
        let request = IpcRequest::Play {
            queue,
            start_index,
            mode: self.mode,
            playlist_name,
            loop_playback: false,
        };
        let generation = self.next_status_gen();
        self.min_status_gen = generation;
        self.action_in_flight = self.action_in_flight.saturating_add(1);
        self.effects.push(TuiEffect::EnsureAndSend {
            generation,
            request,
        });
    }

    fn send_control(&mut self, request: IpcRequest) {
        let generation = self.next_status_gen();
        self.min_status_gen = generation;
        self.action_in_flight = self.action_in_flight.saturating_add(1);
        self.effects.push(TuiEffect::SendControl {
            generation,
            request,
        });
    }

    fn adjust_volume(&mut self, delta: i16) {
        let next_volume = (self.volume as i16 + delta).clamp(0, MAX_VOLUME as i16) as u8;
        self.volume = next_volume;
        self.status.volume = next_volume;
        let generation = self.next_status_gen();
        self.min_status_gen = generation;
        self.volume_gen = generation;
        self.action_in_flight = self.action_in_flight.saturating_add(1);
        self.effects.push(TuiEffect::SetVolume {
            generation,
            volume: next_volume,
        });
    }

    fn toggle_mode(&mut self) {
        self.mode = match self.mode {
            PlayMode::Next => PlayMode::Shuffle,
            PlayMode::Shuffle => PlayMode::Next,
        };
        self.mode_locked = true;
        self.set_message(format!("Mode set to {}.", self.mode), false);
    }

    fn open_delete_modal(&mut self) {
        if self.delete_in_flight {
            self.set_message("Delete already in progress.", false);
            return;
        }
        match self.focus {
            FocusPane::Tracks => {
                let Some(song) = self.selected_track() else {
                    self.set_message("No track selected to delete.", false);
                    return;
                };
                self.confirm_delete = Some(ConfirmDelete {
                    label: song.title.clone(),
                    query: song.title,
                    kind: TargetKind::Song,
                });
            }
            FocusPane::Playlists => {
                let idx = self.selected_playlist_index();
                if idx == 0 {
                    self.set_message("[All Songs] cannot be deleted.", false);
                    return;
                }
                let Some(pl) = self.library.playlists.get(idx - 1) else {
                    return;
                };
                self.confirm_delete = Some(ConfirmDelete {
                    label: pl.name.clone(),
                    query: pl.name.clone(),
                    kind: TargetKind::Playlist,
                });
            }
        }
    }

    fn execute_pending_delete(&mut self) {
        if self.delete_in_flight {
            return;
        }
        let Some(pending) = self.confirm_delete.take() else {
            return;
        };

        self.delete_in_flight = true;
        self.library_gen = self.library_gen.saturating_add(1);
        let generation = self.library_gen;
        self.set_message(format!("Deleting '{}'...", pending.label), false);
        self.effects.push(TuiEffect::DeleteTarget {
            generation,
            target: pending,
        });
    }

    fn next_status_gen(&mut self) -> u64 {
        self.status_gen = self.status_gen.saturating_add(1);
        self.status_gen
    }

    fn apply_live_status(&mut self, mut status: PlaybackStatus, generation: u64, clear_message: bool) {
        if !self.mode_locked && status.state != PlaybackState::Stopped {
            self.mode = status.mode;
            self.mode_locked = true;
        }
        if generation >= self.volume_gen {
            self.volume = status.volume.min(MAX_VOLUME);
            status.volume = self.volume;
        } else {
            status.volume = self.volume;
        }
        self.status = status;
        if clear_message {
            self.status_message = None;
        }
    }

    fn reset_status_stopped(&mut self) {
        self.status = PlaybackStatus {
            mode: self.mode,
            volume: self.volume,
            ..PlaybackStatus::default()
        };
    }

    fn playlist_count(&self) -> usize {
        1 + self.library.playlists.len()
    }

    fn selected_track(&self) -> Option<Song> {
        let idx = self.track_state.selected()?;
        self.visible_tracks().into_iter().nth(idx)
    }

    fn ensure_track_selected(&mut self) {
        let len = self.visible_tracks().len();
        if len == 0 {
            self.track_state.select(None);
        } else if let Some(idx) = self.track_state.selected() {
            if idx >= len {
                self.track_state.select(Some(len - 1));
            }
        } else {
            self.track_state.select(Some(0));
        }
    }

    fn reset_track_selection(&mut self) {
        let len = self.visible_tracks().len();
        if len == 0 {
            self.track_state.select(None);
        } else {
            *self.track_state.offset_mut() = 0;
            self.track_state.select(Some(0));
        }
    }

    fn restore_selection(
        &mut self,
        prev_playlist_name: Option<&str>,
        prev_track_id: Option<&str>,
        prev_track_idx: Option<usize>,
    ) {
        let playlist_idx = prev_playlist_name
            .and_then(|name| self.library.playlists.iter().position(|p| p.name == name))
            .map_or(0, |idx| idx + 1);
        self.playlist_state.select(Some(playlist_idx));

        let tracks = self.visible_tracks();
        if tracks.is_empty() {
            self.track_state.select(None);
            return;
        }
        if let Some(id) = prev_track_id
            && let Some(pos) = tracks.iter().position(|s| s.id == id)
        {
            self.track_state.select(Some(pos));
            return;
        }
        let clamped = prev_track_idx.unwrap_or(0).min(tracks.len() - 1);
        self.track_state.select(Some(clamped));
    }

    fn set_message(&mut self, text: impl Into<String>, is_error: bool) {
        self.status_message = Some(StatusMessage {
            text: text.into(),
            is_error,
            expires_at: Some(Instant::now() + STATUS_TTL),
        });
    }

    fn clear_expired_message(&mut self) {
        if let Some(msg) = &self.status_message
            && let Some(deadline) = msg.expires_at
            && Instant::now() >= deadline
        {
            self.status_message = None;
        }
    }

    /// Render the complete TUI frame (header, split library panes, now-playing footer, optional modal).
    pub fn render(&mut self, frame: &mut Frame) {
        self.render_with_ascii(frame, use_ascii_glyphs());
    }

    fn render_with_ascii(&mut self, frame: &mut Frame, ascii: bool) {
        let area = frame.area();
        let [header_area, main_area, footer_area] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(5),
        ])
        .areas(area);

        self.render_header(frame, header_area, ascii);
        self.render_main(frame, main_area, ascii);
        self.render_footer(frame, footer_area, ascii);

        if let Some(modal) = self.confirm_delete.clone() {
            self.render_delete_modal(frame, area, &modal, ascii);
        }
    }

    fn render_header(&self, frame: &mut Frame, area: Rect, ascii: bool) {
        if area.is_empty() {
            return;
        }
        let mode_text = match self.mode {
            PlayMode::Next => "[Mode: Next]",
            PlayMode::Shuffle => "[Mode: Shuffle]",
        };
        let (state_text, state_style) = match self.status.state {
            PlaybackState::Playing => (
                "Playing",
                Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
            ),
            PlaybackState::Paused => (
                "Paused",
                Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ),
            PlaybackState::Stopped => (
                "Stopped",
                Style::new().fg(Color::DarkGray).add_modifier(Modifier::BOLD),
            ),
        };

        let mut spans = vec![
            Span::styled(
                concat!(" Tunr v", env!("CARGO_PKG_VERSION")),
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
            Span::styled("   |   ", Style::new().fg(Color::DarkGray)),
            Span::styled(
                mode_text,
                Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD),
            ),
        ];
        if self.status.loop_playback {
            spans.push(Span::raw(" "));
            spans.push(Span::styled(
                "[Loop]",
                Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ));
        }
        spans.push(Span::styled("   |   ", Style::new().fg(Color::DarkGray)));
        spans.push(Span::styled(state_text, state_style));
        spans.push(Span::styled("   |   ", Style::new().fg(Color::DarkGray)));
        spans.push(Span::styled(
            format!("Vol: {}%", self.volume),
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ));
        let title_line = Line::from(spans);

        let block = Block::bordered()
            .border_type(border_type(ascii))
            .border_style(Style::new().fg(Color::DarkGray));
        let paragraph = Paragraph::new(title_line).block(block);
        frame.render_widget(paragraph, area);
    }

    fn render_main(&mut self, frame: &mut Frame, area: Rect, ascii: bool) {
        if area.is_empty() {
            return;
        }
        let (playlists_area, tracks_area) = if area.width < 65 {
            let [top, bottom] = Layout::vertical([
                Constraint::Percentage(35),
                Constraint::Percentage(65),
            ])
            .areas(area);
            (top, bottom)
        } else {
            let [left, right] = Layout::horizontal([
                Constraint::Percentage(30),
                Constraint::Percentage(70),
            ])
            .areas(area);
            (left, right)
        };

        self.render_playlists(frame, playlists_area, ascii);
        self.render_tracks(frame, tracks_area, ascii);
    }

    fn render_playlists(&mut self, frame: &mut Frame, area: Rect, ascii: bool) {
        if area.is_empty() {
            return;
        }
        let focused = self.focus == FocusPane::Playlists;
        let border_style = pane_border_style(focused);
        let title_style = pane_title_style(focused);

        let block = Block::bordered()
            .border_type(border_type(ascii))
            .border_style(border_style)
            .title(Span::styled(" Playlists ", title_style));

        let mut items = Vec::with_capacity(self.playlist_count());
        items.push(ListItem::new(format!(
            "[All Songs] ({})",
            self.library.songs.len()
        )));
        for pl in &self.library.playlists {
            let count = self.library.songs_for_playlist(pl).len();
            items.push(ListItem::new(format!("{} ({})", pl.name, count)));
        }

        let highlight_style = if focused {
            Style::new()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::new()
                .fg(Color::White)
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD)
        };

        let list = List::new(items)
            .block(block)
            .highlight_symbol("> ")
            .highlight_spacing(HighlightSpacing::Always)
            .highlight_style(highlight_style);

        frame.render_stateful_widget(list, area, &mut self.playlist_state);
    }

    fn render_tracks(&mut self, frame: &mut Frame, area: Rect, ascii: bool) {
        if area.is_empty() {
            return;
        }
        let focused = self.focus == FocusPane::Tracks;
        let border_style = pane_border_style(focused);
        let title_style = pane_title_style(focused);

        let context_label = self
            .selected_playlist_name()
            .unwrap_or_else(|| "[All Songs]".to_string());
        let block = Block::bordered()
            .border_type(border_type(ascii))
            .border_style(border_style)
            .title(Span::styled(format!(" Tracks: {context_label} "), title_style));

        let tracks = self.visible_tracks();
        if tracks.is_empty() {
            self.track_state.select(None);
            let inner = block.inner(area);
            frame.render_widget(block, area);
            if !inner.is_empty() {
                let empty = Paragraph::new(
                    "No tracks in this view.\nRun `tunr add <path>` to import music.",
                )
                .style(Style::new().fg(Color::DarkGray))
                .alignment(Alignment::Center)
                .wrap(Wrap { trim: true });
                frame.render_widget(empty, inner);
            }
            return;
        }

        let inner_width = area.width.saturating_sub(4) as usize;
        let active_track = if self.status.state != PlaybackState::Stopped {
            self.status.current_track.as_ref()
        } else {
            None
        };

        let items: Vec<ListItem> = tracks
            .iter()
            .map(|song| {
                let is_active =
                    active_track.is_some_and(|cur| cur.id == song.id || cur.path == song.path);
                let prefix = if is_active {
                    if ascii { "> " } else { "▶ " }
                } else {
                    "  "
                };
                let artist = song.artist.as_deref().unwrap_or("Unknown Artist");
                let dur = song
                    .duration_secs
                    .map(format_time)
                    .unwrap_or_else(|| "--:--".to_string());
                let dur_part = format!(" {dur}");
                let main_text = format!("{} - {}", song.title, artist);

                let prefix_len = prefix.chars().count();
                let dur_len = dur_part.chars().count();
                let label_budget = inner_width.saturating_sub(prefix_len + dur_len);
                let fitted = if label_budget > 0 {
                    fit_to_width(&main_text, label_budget)
                } else {
                    main_text
                };

                let prefix_style = if is_active {
                    Style::new().fg(Color::Green).add_modifier(Modifier::BOLD)
                } else {
                    Style::new()
                };
                let text_style = if is_active {
                    Style::new().fg(Color::Green).add_modifier(Modifier::BOLD)
                } else {
                    Style::new()
                };
                let dur_style = Style::new().fg(Color::DarkGray);

                ListItem::new(Line::from(vec![
                    Span::styled(prefix, prefix_style),
                    Span::styled(fitted, text_style),
                    Span::styled(dur_part, dur_style),
                ]))
            })
            .collect();

        let highlight_style = if focused {
            Style::new()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::new()
                .fg(Color::White)
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD)
        };

        let list = List::new(items)
            .block(block)
            .highlight_symbol("> ")
            .highlight_spacing(HighlightSpacing::Always)
            .highlight_style(highlight_style);

        frame.render_stateful_widget(list, area, &mut self.track_state);
    }

    fn render_footer(&self, frame: &mut Frame, area: Rect, ascii: bool) {
        if area.is_empty() {
            return;
        }
        let mut block = Block::bordered()
            .border_type(border_type(ascii))
            .border_style(Style::new().fg(Color::DarkGray))
            .title(Span::styled(
                " Now Playing ",
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ));

        if let Some(msg) = &self.status_message {
            let msg_style = if msg.is_error {
                Style::new().fg(Color::Red).add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(Color::Yellow)
            };
            block = block.title_bottom(
                Line::from(Span::styled(format!(" {} ", msg.text), msg_style)).right_aligned(),
            );
        }

        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.is_empty() {
            return;
        }

        let [now_area, gauge_area, help_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(inner);

        let now_line = match (&self.status.state, &self.status.current_track) {
            (PlaybackState::Stopped, _) | (_, None) => Line::from(Span::styled(
                "No track playing — press Enter on a track or p on a playlist",
                Style::new().fg(Color::DarkGray),
            )),
            (state, Some(track)) => {
                let icon = match state {
                    PlaybackState::Playing => {
                        if ascii {
                            "> "
                        } else {
                            "▶ "
                        }
                    }
                    PlaybackState::Paused => "|| ",
                    PlaybackState::Stopped => "",
                };
                let artist = track
                    .artist
                    .as_deref()
                    .map(|a| format!(" - {a}"))
                    .unwrap_or_default();
                let playlist_ctx = self
                    .status
                    .playlist_name
                    .as_deref()
                    .unwrap_or("[All Songs]");
                let queue_pos = if self.status.queue_len > 0 {
                    format!(" ({}/{})", self.status.queue_index + 1, self.status.queue_len)
                } else {
                    String::new()
                };
                Line::from(vec![
                    Span::styled(icon, Style::new().fg(Color::Green).add_modifier(Modifier::BOLD)),
                    Span::styled(
                        format!("{}{}", track.title, artist),
                        Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("  [{playlist_ctx}]{queue_pos}"),
                        Style::new().fg(Color::Cyan),
                    ),
                ])
            }
        };
        frame.render_widget(Paragraph::new(now_line), now_area);

        let total_secs = self.status.duration_secs.or_else(|| {
            self.status
                .current_track
                .as_ref()
                .and_then(|t| t.duration_secs)
        });
        let pos_secs = if self.status.state == PlaybackState::Stopped {
            0.0
        } else {
            self.status.position_secs
        };
        let ratio = if self.status.state == PlaybackState::Stopped {
            0.0
        } else if let Some(total) = total_secs.filter(|t| t.is_finite() && *t > 0.0) {
            if pos_secs.is_finite() {
                (pos_secs / total).clamp(0.0, 1.0)
            } else {
                0.0
            }
        } else {
            0.0
        };
        let total_label = total_secs
            .map(format_time)
            .unwrap_or_else(|| "--:--".to_string());
        let gauge_label = format!("{} / {}", format_time(pos_secs), total_label);
        let gauge = Gauge::default()
            .ratio(ratio)
            .label(gauge_label)
            .style(Style::new().fg(Color::White).bg(Color::DarkGray))
            .gauge_style(Style::new().fg(Color::Cyan).bg(Color::DarkGray));
        frame.render_widget(gauge, gauge_area);

        let help_text = if inner.width < 128 {
            if ascii {
                "<-/->/Tab:Pane  h/j/k/l:Nav  Enter:Play  Space:Pause  n/b:Next  -/+:Vol  d:Del  q:Quit"
            } else {
                "←/→/Tab:Pane  ↑/↓:Nav  Enter:Play  Space:Pause  n/b:Next  -/+:Vol  d:Del  q:Quit"
            }
        } else if ascii {
            "<-/->/Tab:Pane Up/Down/j/k:Nav Enter:Play p:PlayList Space:Pause n/b:Next/Prev [/]:+/-5s -/+:Vol s:Mode d:Del q:Quit Q:Stop+Quit"
        } else {
            "←/→/Tab:Pane ↑/↓/j/k:Nav Enter:Play p:PlayList Space:Pause n/b:Next/Prev [/]:±5s -/+:Vol s:Mode d:Del q:Quit Q:Stop+Quit"
        };
        let help_line = Line::from(Span::styled(help_text, Style::new().fg(Color::Gray)));
        frame.render_widget(Paragraph::new(help_line), help_area);
    }

    fn render_delete_modal(&self, frame: &mut Frame, area: Rect, modal: &ConfirmDelete, ascii: bool) {
        if area.width < 10 || area.height < 5 {
            return;
        }
        let popup = area.centered(
            Constraint::Length(area.width.min(64)),
            Constraint::Length(area.height.min(7)),
        );
        frame.render_widget(Clear, popup);

        let modal_border = if ascii {
            BorderType::Plain
        } else {
            BorderType::Double
        };
        let block = Block::bordered()
            .border_type(modal_border)
            .border_style(Style::new().fg(Color::Red))
            .title(Span::styled(
                " Confirm Delete ",
                Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
            ));

        let body = match modal.kind {
            TargetKind::Song | TargetKind::Auto => format!(
                "Remove song '{}' from library?\n(Audio file on disk will NOT be deleted)\n\n[y / Enter] Confirm   [n / Esc] Cancel",
                modal.label
            ),
            TargetKind::Playlist => format!(
                "Delete playlist '{}'?\n(Songs remain in library)\n\n[y / Enter] Confirm   [n / Esc] Cancel",
                modal.label
            ),
        };

        let paragraph = Paragraph::new(body)
            .block(block)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true });
        frame.render_widget(paragraph, popup);
    }
}

/// Launch the interactive ratatui TUI on the current terminal.
pub async fn run_tui(paths: &AppPaths) -> Result<()> {
    let paths_for_load = paths.clone();
    let (library, library_stamp, initial_volume) = tokio::task::spawn_blocking(move || -> Result<_> {
        let library = Library::load(&paths_for_load.library_path)?;
        let stamp = file_stamp(&paths_for_load.library_path);
        let initial_volume = load_initial_volume(&paths_for_load);
        Ok((library, stamp, initial_volume))
    })
    .await
    .map_err(|err| TunrError::Async(format!("failed to load library: {err}")))??;

    let mut app = TuiApp::from_loaded(library, library_stamp).with_volume(initial_volume);
    app.request_poll_status();

    let mut session = TerminalSession::enter()?;
    let mut signals = ShutdownSignals::new()?;

    let (worker_tx, mut worker_rx) = mpsc::unbounded_channel::<TuiEvent>();
    let (action_tx, action_rx) = mpsc::unbounded_channel::<QueuedIpcAction>();
    let action_worker = tokio::spawn(run_ipc_action_worker(
        paths.clone(),
        action_rx,
        worker_tx.clone(),
    ));
    let action_guard = AbortGuard(action_worker);
    let mut background_tasks = JoinSet::new();

    // Create EventStream after TerminalSession so EventStream drops before raw mode restores.
    let mut events = EventStream::new();

    let mut stop_on_quit = dispatch_effects(
        paths,
        app.take_effects(),
        &worker_tx,
        &action_tx,
        &mut background_tasks,
    );

    let first_tick = tokio::time::Instant::now() + SYNC_INTERVAL;
    let mut sync_interval = tokio::time::interval_at(first_tick, SYNC_INTERVAL);
    sync_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

    while !app.should_quit() {
        session
            .terminal_mut()
            .draw(|frame| app.render(frame))?;

        tokio::select! {
            maybe_ev = events.next() => {
                match maybe_ev {
                    Some(Ok(Event::Key(key)))
                        if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                    {
                        app.handle_key(key);
                    }
                    Some(Ok(Event::Resize(_, _))) => {}
                    Some(Ok(_)) => {}
                    Some(Err(err)) => return Err(err.into()),
                    None => app.request_quit(),
                }
            }
            _ = sync_interval.tick() => {
                app.clear_expired_message();
                app.request_library_check();
                app.request_poll_status();
            }
            Some(worker_ev) = worker_rx.recv() => {
                app.apply_event(worker_ev);
                while let Ok(more) = worker_rx.try_recv() {
                    app.apply_event(more);
                }
            }
            Some(join_res) = background_tasks.join_next(), if !background_tasks.is_empty() => {
                if let Err(err) = join_res
                    && !err.is_cancelled()
                {
                    app.apply_event(TuiEvent::WorkerError(format!(
                        "background worker failed: {err}"
                    )));
                }
            }
            _ = signals.recv() => {
                app.request_quit();
            }
        }

        if dispatch_effects(
            paths,
            app.take_effects(),
            &worker_tx,
            &action_tx,
            &mut background_tasks,
        ) {
            stop_on_quit = true;
        }
    }

    drop(events);
    drop(action_guard);
    background_tasks.abort_all();
    while background_tasks.join_next().await.is_some() {}

    drop(session);

    if stop_on_quit {
        stop_daemon_on_quit(paths, &mut signals).await?;
    }

    Ok(())
}

fn dispatch_effects(
    paths: &AppPaths,
    effects: Vec<TuiEffect>,
    worker_tx: &mpsc::UnboundedSender<TuiEvent>,
    action_tx: &mpsc::UnboundedSender<QueuedIpcAction>,
    background_tasks: &mut JoinSet<()>,
) -> bool {
    let mut stop_on_quit = false;
    for effect in effects {
        match effect {
            TuiEffect::PollStatus { generation } => {
                let paths = paths.clone();
                let tx = worker_tx.clone();
                background_tasks.spawn(async move {
                    let result = ipc::send_request(&paths, &IpcRequest::GetStatus).await;
                    let _ = tx.send(TuiEvent::StatusPolled { generation, result });
                });
            }
            TuiEffect::EnsureAndSend {
                generation,
                request,
            } => {
                if action_tx
                    .send(QueuedIpcAction::Ipc {
                        generation,
                        ensure_daemon: true,
                        request,
                    })
                    .is_err()
                {
                    let _ = worker_tx.send(TuiEvent::ActionCompleted {
                        generation,
                        is_play: true,
                        result: Err(TunrError::Async(
                            "playback action worker unavailable".to_string(),
                        )),
                    });
                }
            }
            TuiEffect::SendControl {
                generation,
                request,
            } => {
                if action_tx
                    .send(QueuedIpcAction::Ipc {
                        generation,
                        ensure_daemon: false,
                        request,
                    })
                    .is_err()
                {
                    let _ = worker_tx.send(TuiEvent::ActionCompleted {
                        generation,
                        is_play: false,
                        result: Err(TunrError::Async(
                            "playback action worker unavailable".to_string(),
                        )),
                    });
                }
            }
            TuiEffect::SetVolume { generation, volume } => {
                if action_tx
                    .send(QueuedIpcAction::SetVolume { generation, volume })
                    .is_err()
                {
                    let _ = worker_tx.send(TuiEvent::VolumeUpdated {
                        generation,
                        result: Err(TunrError::Async(
                            "playback action worker unavailable".to_string(),
                        )),
                    });
                }
            }
            TuiEffect::StopOnQuit { .. } => {
                stop_on_quit = true;
            }
            TuiEffect::CheckLibraryReload {
                generation,
                known_stamp,
            } => {
                let lib_path = paths.library_path.clone();
                let tx = worker_tx.clone();
                background_tasks.spawn(async move {
                    let ev = match tokio::task::spawn_blocking(move || {
                        check_library_reload_blocking(&lib_path, known_stamp)
                    })
                    .await
                    {
                        Ok((observed_stamp, result)) => TuiEvent::LibraryChecked {
                            generation,
                            observed_stamp,
                            result,
                        },
                        Err(join_err) => TuiEvent::LibraryChecked {
                            generation,
                            observed_stamp: known_stamp,
                            result: Err(TunrError::Async(format!(
                                "library reload task failed: {join_err}"
                            ))),
                        },
                    };
                    let _ = tx.send(ev);
                });
            }
            TuiEffect::DeleteTarget { generation, target } => {
                let lib_path = paths.library_path.clone();
                let tx = worker_tx.clone();
                background_tasks.spawn(async move {
                    let result = match tokio::task::spawn_blocking(move || {
                        execute_delete_blocking(&lib_path, &target)
                    })
                    .await
                    {
                        Ok(res) => res,
                        Err(join_err) => Err(TunrError::Async(format!(
                            "delete task failed: {join_err}"
                        ))),
                    };
                    let _ = tx.send(TuiEvent::DeleteFinished { generation, result });
                });
            }
        }
    }
    stop_on_quit
}

async fn run_ipc_action_worker(
    paths: AppPaths,
    mut rx: mpsc::UnboundedReceiver<QueuedIpcAction>,
    worker_tx: mpsc::UnboundedSender<TuiEvent>,
) {
    while let Some(action) = rx.recv().await {
        let event = match action {
            QueuedIpcAction::Ipc {
                generation,
                ensure_daemon,
                request,
            } => {
                let result = if ensure_daemon {
                    ipc::ensure_daemon_and_send(&paths, &request).await
                } else {
                    ipc::send_request(&paths, &request).await
                };
                TuiEvent::ActionCompleted {
                    generation,
                    is_play: ensure_daemon,
                    result,
                }
            }
            QueuedIpcAction::SetVolume { generation, volume } => {
                let result = execute_set_volume(&paths, volume).await;
                TuiEvent::VolumeUpdated { generation, result }
            }
        };
        if worker_tx.send(event).is_err() {
            break;
        }
    }
}

async fn execute_set_volume(paths: &AppPaths, volume: u8) -> Result<Option<PlaybackStatus>> {
    let paths_for_save = paths.clone();
    tokio::task::spawn_blocking(move || paths::set_workspace_volume(&paths_for_save, volume))
        .await
        .map_err(|err| TunrError::Async(format!("volume save task failed: {err}")))??;

    match ipc::send_request(paths, &IpcRequest::SetVolume(volume)).await {
        Ok(status) => Ok(Some(status)),
        Err(err) if is_daemon_offline_error(&err) => Ok(None),
        Err(err) => Err(err),
    }
}

fn load_initial_volume(paths: &AppPaths) -> u8 {
    paths::load_workspace_config(paths)
        .ok()
        .map(|c| paths::effective_volume(&c))
        .unwrap_or(DEFAULT_VOLUME)
}

async fn stop_daemon_on_quit(
    paths: &AppPaths,
    signals: &mut ShutdownSignals,
) -> Result<()> {
    let stop_fut = tokio::time::timeout(
        STOP_ON_QUIT_TIMEOUT,
        ipc::send_request(paths, &IpcRequest::Stop),
    );
    tokio::select! {
        res = stop_fut => match res {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(err)) if is_daemon_offline_error(&err) => Ok(()),
            Ok(Err(err)) => Err(err),
            Err(_) => Err(TunrError::Ipc(
                "timed out waiting for daemon stop".to_string(),
            )),
        },
        _ = signals.recv() => Ok(()),
    }
}

fn check_library_reload_blocking(
    lib_path: &Path,
    known_stamp: Option<(SystemTime, u64)>,
) -> (Option<(SystemTime, u64)>, Result<Option<Library>>) {
    let current_stamp = file_stamp(lib_path);
    if current_stamp == known_stamp {
        return (current_stamp, Ok(None));
    }
    match Library::load(lib_path) {
        Ok(lib) => (current_stamp, Ok(Some(lib))),
        Err(err) => (current_stamp, Err(err)),
    }
}

fn execute_delete_blocking(
    lib_path: &Path,
    target: &ConfirmDelete,
) -> Result<(Library, Option<(SystemTime, u64)>, DeleteOutcome)> {
    let mut updated_library = None;
    let outcome = Library::update(lib_path, |lib| {
        let outcome = lib.delete_target(&target.query, target.kind)?;
        updated_library = Some(lib.clone());
        Ok(outcome)
    })?;
    let stamp = file_stamp(lib_path);
    let library = updated_library.ok_or_else(|| {
        TunrError::Async("delete transaction completed without updated library".to_string())
    })?;
    Ok((library, stamp, outcome))
}

fn pane_border_style(focused: bool) -> Style {
    if focused {
        Style::new().fg(Color::Cyan)
    } else {
        Style::new().fg(Color::DarkGray)
    }
}

fn pane_title_style(focused: bool) -> Style {
    if focused {
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(Color::Gray)
    }
}

fn file_stamp(path: &Path) -> Option<(SystemTime, u64)> {
    let meta = fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    Some((modified, meta.len()))
}

fn is_daemon_offline_error(err: &TunrError) -> bool {
    match err {
        TunrError::Ipc(msg) => {
            let lower = msg.to_ascii_lowercase();
            lower.contains("not running")
                || lower.contains("no such file")
                || lower.contains("connection refused")
        }
        TunrError::Io(io_err) => matches!(
            io_err.kind(),
            io::ErrorKind::NotFound
                | io::ErrorKind::ConnectionRefused
                | io::ErrorKind::ConnectionReset
                | io::ErrorKind::BrokenPipe
                | io::ErrorKind::NotConnected
        ),
        _ => false,
    }
}

fn border_type(ascii: bool) -> BorderType {
    if ascii {
        BorderType::Plain
    } else {
        BorderType::Rounded
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use uuid::Uuid;

    use super::*;
    use crate::library::Playlist;

    #[derive(Clone, Default)]
    struct MockState {
        requests: Vec<IpcRequest>,
        next_status: Option<PlaybackStatus>,
        offline: bool,
    }

    impl MockState {
        fn handle_send(&mut self, req: &IpcRequest) -> Result<PlaybackStatus> {
            self.requests.push(req.clone());
            if self.offline {
                return Err(TunrError::Ipc("daemon not running".to_string()));
            }
            if let IpcRequest::SetVolume(vol) = req {
                let mut status = self.next_status.clone().unwrap_or_default();
                status.volume = *vol;
                self.next_status = Some(status.clone());
                return Ok(status);
            }
            Ok(self.next_status.clone().unwrap_or_default())
        }

        fn handle_ensure(&mut self, req: &IpcRequest) -> Result<PlaybackStatus> {
            self.requests.push(req.clone());
            if let IpcRequest::Play {
                queue,
                start_index,
                mode,
                playlist_name,
                loop_playback,
            } = req
            {
                let idx = start_index.unwrap_or(0);
                let current_track = queue.get(idx).cloned();
                let status = PlaybackStatus {
                    state: PlaybackState::Playing,
                    mode: *mode,
                    playlist_name: playlist_name.clone(),
                    loop_playback: *loop_playback,
                    current_track,
                    queue_index: idx,
                    queue_len: queue.len(),
                    position_secs: 0.0,
                    duration_secs: queue.get(idx).and_then(|s| s.duration_secs),
                    queue: queue.clone(),
                    volume: self.next_status.as_ref().map_or(DEFAULT_VOLUME, |s| s.volume),
                };
                self.next_status = Some(status.clone());
                return Ok(status);
            }
            Ok(self.next_status.clone().unwrap_or_default())
        }
    }

    fn drive_effects(app: &mut TuiApp, paths: &AppPaths, mock: &mut MockState) {
        for effect in app.take_effects() {
            match effect {
                TuiEffect::PollStatus { generation } => {
                    let result = mock.handle_send(&IpcRequest::GetStatus);
                    app.apply_event(TuiEvent::StatusPolled { generation, result });
                }
                TuiEffect::EnsureAndSend {
                    generation,
                    request,
                } => {
                    let result = mock.handle_ensure(&request);
                    app.apply_event(TuiEvent::ActionCompleted {
                        generation,
                        is_play: true,
                        result,
                    });
                }
                TuiEffect::SendControl {
                    generation,
                    request,
                } => {
                    let result = mock.handle_send(&request);
                    app.apply_event(TuiEvent::ActionCompleted {
                        generation,
                        is_play: false,
                        result,
                    });
                }
                TuiEffect::SetVolume { generation, volume } => {
                    let result = match mock.handle_send(&IpcRequest::SetVolume(volume)) {
                        Ok(status) => Ok(Some(status)),
                        Err(err) if is_daemon_offline_error(&err) => Ok(None),
                        Err(err) => Err(err),
                    };
                    app.apply_event(TuiEvent::VolumeUpdated { generation, result });
                }
                TuiEffect::StopOnQuit { .. } => {
                    let _ = mock.handle_send(&IpcRequest::Stop);
                }
                TuiEffect::CheckLibraryReload {
                    generation,
                    known_stamp,
                } => {
                    let (observed_stamp, result) =
                        check_library_reload_blocking(&paths.library_path, known_stamp);
                    app.apply_event(TuiEvent::LibraryChecked {
                        generation,
                        observed_stamp,
                        result,
                    });
                }
                TuiEffect::DeleteTarget { generation, target } => {
                    let result = execute_delete_blocking(&paths.library_path, &target);
                    app.apply_event(TuiEvent::DeleteFinished { generation, result });
                }
            }
        }
    }

    fn temp_paths(label: &str) -> (PathBuf, AppPaths) {
        let root = std::env::temp_dir().join(format!(
            "tunr-tui-test-{label}-{}",
            Uuid::new_v4()
        ));
        fs::create_dir_all(&root).unwrap();
        let paths = AppPaths::from_roots(&root, &root);
        (root, paths)
    }

    fn sample_song(id: &str, title: &str, artist: Option<&str>, duration_secs: f64) -> Song {
        Song {
            id: id.to_string(),
            title: title.to_string(),
            artist: artist.map(ToOwned::to_owned),
            album: None,
            path: PathBuf::from(format!("/music/{title}.mp3")),
            duration_secs: Some(duration_secs),
        }
    }

    fn sample_library() -> Library {
        let s1 = sample_song("s1", "Alpha Dawn", Some("Artist A"), 125.0);
        let s2 = sample_song("s2", "Beta Drift", Some("Artist B"), 200.0);
        let s3 = sample_song("s3", "Gamma Pulse", None, 90.0);
        Library {
            version: 1,
            songs: vec![s1, s2, s3],
            playlists: vec![
                Playlist {
                    name: "Chill".to_string(),
                    song_ids: vec!["s2".to_string(), "s3".to_string()],
                },
                Playlist {
                    name: "EmptyMix".to_string(),
                    song_ids: vec![],
                },
            ],
        }
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn navigation_filtering_and_enter_behavior() {
        let (root, paths) = temp_paths("nav");
        let lib = sample_library();
        let mut mock = MockState::default();
        let mut app = TuiApp::from_loaded(lib, None);

        assert_eq!(app.focus(), FocusPane::Playlists);
        assert_eq!(app.selected_playlist_index(), 0);
        assert_eq!(app.selected_playlist_name(), None);
        assert_eq!(app.visible_tracks().len(), 3);
        assert_eq!(app.selected_track_index(), Some(0));

        // Move down to "Chill" playlist -> Tracks preview updates immediately.
        app.handle_key(press(KeyCode::Down));
        assert_eq!(app.selected_playlist_index(), 1);
        assert_eq!(app.selected_playlist_name().as_deref(), Some("Chill"));
        let chill_tracks = app.visible_tracks();
        assert_eq!(chill_tracks.len(), 2);
        assert_eq!(chill_tracks[0].title, "Beta Drift");
        assert_eq!(chill_tracks[1].title, "Gamma Pulse");
        assert_eq!(app.selected_track_index(), Some(0));

        // Move down to "EmptyMix" -> Enter on Playlists stays safely when empty.
        app.handle_key(press(KeyCode::Down));
        assert_eq!(app.selected_playlist_index(), 2);
        assert!(app.visible_tracks().is_empty());
        assert_eq!(app.selected_track_index(), None);
        app.handle_key(press(KeyCode::Enter));
        assert_eq!(app.focus(), FocusPane::Playlists);

        // Move back up to "Chill", press Enter -> focuses Tracks.
        app.handle_key(press(KeyCode::Up));
        assert_eq!(app.selected_playlist_index(), 1);
        app.handle_key(press(KeyCode::Enter));
        assert_eq!(app.focus(), FocusPane::Tracks);
        assert_eq!(app.selected_track_index(), Some(0));

        // Navigate down inside Tracks and press Enter -> emits Play for second song in Chill.
        app.handle_key(press(KeyCode::Down));
        assert_eq!(app.selected_track_index(), Some(1));
        app.handle_key(press(KeyCode::Enter));
        drive_effects(&mut app, &paths, &mut mock);

        assert_eq!(mock.requests.len(), 1);
        assert_eq!(
            mock.requests[0],
            IpcRequest::Play {
                queue: chill_tracks,
                start_index: Some(1),
                mode: PlayMode::Next,
                playlist_name: Some("Chill".to_string()),
                loop_playback: false,
            }
        );

        // Left switches focus back to Playlists; Right returns to Tracks.
        app.handle_key(press(KeyCode::Left));
        assert_eq!(app.focus(), FocusPane::Playlists);
        app.handle_key(press(KeyCode::Right));
        assert_eq!(app.focus(), FocusPane::Tracks);
        app.handle_key(press(KeyCode::Tab));
        assert_eq!(app.focus(), FocusPane::Playlists);

        // Non-press key events must be ignored.
        app.handle_key(KeyEvent::new_with_kind(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        ));
        assert!(!app.should_quit());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn mode_toggle_and_playback_controls() {
        let (root, paths) = temp_paths("controls");
        let lib = sample_library();
        let mut mock = MockState::default();
        let mut app = TuiApp::from_loaded(lib, None);

        assert_eq!(app.mode(), PlayMode::Next);
        app.handle_key(press(KeyCode::Char('s')));
        assert_eq!(app.mode(), PlayMode::Shuffle);

        // Pressing 'p' on Playlists plays [All Songs] immediately with Shuffle mode (start_index: None).
        app.handle_key(press(KeyCode::Char('p')));
        app.handle_key(press(KeyCode::Char(' ')));
        app.handle_key(press(KeyCode::Char('n')));
        app.handle_key(press(KeyCode::Char('b')));
        app.handle_key(press(KeyCode::Char('[')));
        app.handle_key(press(KeyCode::Char(']')));
        // Pressing 'p' on Tracks passes Some(selected_index).
        app.handle_key(press(KeyCode::Right));
        app.handle_key(press(KeyCode::Down));
        app.handle_key(press(KeyCode::Char('p')));
        app.handle_key(press(KeyCode::Char('Q')));

        drive_effects(&mut app, &paths, &mut mock);

        assert!(app.should_quit());
        assert!(matches!(
            &mock.requests[0],
            IpcRequest::Play {
                start_index: None,
                mode: PlayMode::Shuffle,
                playlist_name: None,
                loop_playback: false,
                queue,
            } if queue.len() == 3
        ));
        assert_eq!(mock.requests[1], IpcRequest::TogglePause);
        assert_eq!(mock.requests[2], IpcRequest::Next);
        assert_eq!(mock.requests[3], IpcRequest::Prev);
        assert_eq!(mock.requests[4], IpcRequest::SeekRelative(-5.0));
        assert_eq!(mock.requests[5], IpcRequest::SeekRelative(5.0));
        assert!(matches!(
            &mock.requests[6],
            IpcRequest::Play {
                start_index: Some(1),
                mode: PlayMode::Shuffle,
                playlist_name: None,
                loop_playback: false,
                queue,
            } if queue.len() == 3
        ));
        assert_eq!(mock.requests[7], IpcRequest::Stop);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn offline_daemon_poll_shows_stopped_and_q_quits_without_stop() {
        let (root, paths) = temp_paths("offline");
        let mut mock = MockState {
            offline: true,
            ..MockState::default()
        };
        let mut app = TuiApp::from_loaded(sample_library(), None);

        app.request_poll_status();
        drive_effects(&mut app, &paths, &mut mock);
        assert_eq!(app.status.state, PlaybackState::Stopped);

        app.handle_key(press(KeyCode::Char('q')));
        drive_effects(&mut app, &paths, &mut mock);
        assert!(app.should_quit());
        // Only GetStatus was sent; q must not send Stop or Shutdown.
        assert_eq!(mock.requests, vec![IpcRequest::GetStatus]);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn delete_modal_and_external_reload() {
        let (root, paths) = temp_paths("delete");
        let lib = sample_library();
        lib.save(&paths.library_path).unwrap();
        let stamp = file_stamp(&paths.library_path);

        let mut mock = MockState::default();
        let mut app = TuiApp::from_loaded(lib, stamp);

        // 'd' on [All Songs] does not open modal.
        app.handle_key(press(KeyCode::Char('d')));
        assert!(app.confirm_delete.is_none());

        // Move to "Chill" playlist, open modal, cancel with 'n'.
        app.handle_key(press(KeyCode::Down));
        app.handle_key(press(KeyCode::Char('d')));
        assert_eq!(
            app.confirm_delete,
            Some(ConfirmDelete {
                label: "Chill".to_string(),
                query: "Chill".to_string(),
                kind: TargetKind::Playlist,
            })
        );
        app.handle_key(press(KeyCode::Char('n')));
        assert!(app.confirm_delete.is_none());
        assert_eq!(app.library.playlists.len(), 2);

        // Confirm delete playlist with 'y' -> emits DeleteTarget, removes playlist via Library::update.
        app.handle_key(press(KeyCode::Char('d')));
        app.handle_key(press(KeyCode::Char('y')));
        assert!(app.confirm_delete.is_none());
        drive_effects(&mut app, &paths, &mut mock);
        assert_eq!(app.library.playlists.len(), 1);
        assert_eq!(Library::load(&paths.library_path).unwrap().playlists.len(), 1);

        // Simulate external CLI edit modifying library.json.
        std::thread::sleep(Duration::from_millis(20));
        Library::update(&paths.library_path, |external| {
            external
                .songs
                .push(sample_song("s4", "Delta Wave", Some("Artist D"), 180.0));
            Ok(())
        })
        .unwrap();

        app.request_library_check();
        drive_effects(&mut app, &paths, &mut mock);
        assert_eq!(app.library.songs.len(), 4);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn renders_layout_and_modal_into_test_backend() {
        let (root, paths) = temp_paths("render");
        let lib = sample_library();
        let active_song = lib.songs[1].clone();
        let mut mock = MockState {
            next_status: Some(PlaybackStatus {
                state: PlaybackState::Playing,
                mode: PlayMode::Next,
                playlist_name: Some("Chill".to_string()),
                loop_playback: false,
                current_track: Some(active_song.clone()),
                queue_index: 0,
                queue_len: 2,
                position_secs: 65.0,
                duration_secs: Some(200.0),
                queue: vec![active_song, lib.songs[2].clone()],
                ..PlaybackStatus::default()
            }),
            ..MockState::default()
        };

        let mut app = TuiApp::from_loaded(lib, None);
        app.request_poll_status();
        drive_effects(&mut app, &paths, &mut mock);
        app.handle_key(press(KeyCode::Char('s'))); // toggle mode to Shuffle
        app.handle_key(press(KeyCode::Right)); // focus Tracks

        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();

        let screen = terminal.backend().to_string();
        assert!(screen.contains("Tunr v1.0.0"), "missing header title:\n{screen}");
        assert!(screen.contains("[Mode: Shuffle]"), "missing mode:\n{screen}");
        assert!(screen.contains("Playing"), "missing playback state:\n{screen}");
        assert!(screen.contains("[All Songs] (3)"), "missing [All Songs]:\n{screen}");
        assert!(screen.contains("Chill (2)"), "missing Chill playlist:\n{screen}");
        assert!(screen.contains("▶ Beta Drift"), "missing active track marker:\n{screen}");
        assert!(screen.contains("03:20"), "missing formatted track duration:\n{screen}");
        assert!(screen.contains("01:05 / 03:20"), "missing gauge progress label:\n{screen}");

        // Trigger delete modal on selected track and verify popup renders.
        app.handle_key(press(KeyCode::Char('d')));
        terminal.draw(|frame| app.render(frame)).unwrap();
        let modal_screen = terminal.backend().to_string();
        assert!(modal_screen.contains("Confirm Delete"), "missing modal:\n{modal_screen}");
        assert!(modal_screen.contains("Alpha Dawn"), "missing delete target:\n{modal_screen}");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn emits_effects_and_deduplicates_polls_and_reloads() {
        let mut app = TuiApp::from_loaded(sample_library(), None);

        // Poll deduplication while in flight.
        app.request_poll_status();
        app.request_poll_status();
        let first_effects = app.take_effects();
        assert_eq!(first_effects, vec![TuiEffect::PollStatus { generation: 1 }]);

        // Reload check deduplication while in flight.
        app.request_library_check();
        app.request_library_check();
        let reload_effects = app.take_effects();
        assert_eq!(
            reload_effects,
            vec![TuiEffect::CheckLibraryReload {
                generation: 1,
                known_stamp: None,
            }]
        );

        // Play action skips subsequent PollStatus while action is in flight.
        app.handle_key(press(KeyCode::Char('p')));
        app.request_poll_status();
        let action_effects = app.take_effects();
        assert_eq!(action_effects.len(), 1);
        assert!(matches!(
            action_effects[0],
            TuiEffect::EnsureAndSend {
                generation: 2,
                request: IpcRequest::Play { .. },
            }
        ));

        // Delete skips subsequent library reload check while delete is in flight.
        app.handle_key(press(KeyCode::Down));
        app.handle_key(press(KeyCode::Char('d')));
        app.handle_key(press(KeyCode::Char('y')));
        app.request_library_check();
        let delete_effects = app.take_effects();
        assert_eq!(
            delete_effects,
            vec![TuiEffect::DeleteTarget {
                generation: 2,
                target: ConfirmDelete {
                    label: "Chill".to_string(),
                    query: "Chill".to_string(),
                    kind: TargetKind::Playlist,
                },
            }]
        );
    }

    #[test]
    fn rejects_stale_get_status_responses_across_generations() {
        let lib = sample_library();
        let track = lib.songs[0].clone();
        let mut app = TuiApp::from_loaded(lib, None);

        // 1. Poll starts at generation 1.
        app.request_poll_status();
        assert_eq!(app.take_effects(), vec![TuiEffect::PollStatus { generation: 1 }]);

        // 2. Play starts at generation 2 (advancing min_status_gen = 2).
        app.handle_key(press(KeyCode::Char('p')));
        let effects = app.take_effects();
        assert!(matches!(effects[0], TuiEffect::EnsureAndSend { generation: 2, .. }));

        // 3. Play finishes first with Playing state.
        let playing_status = PlaybackStatus {
            state: PlaybackState::Playing,
            mode: PlayMode::Next,
            playlist_name: None,
            loop_playback: false,
            current_track: Some(track.clone()),
            queue_index: 0,
            queue_len: 3,
            position_secs: 1.0,
            duration_secs: track.duration_secs,
            queue: vec![track],
            ..PlaybackStatus::default()
        };
        app.apply_event(TuiEvent::ActionCompleted {
            generation: 2,
            is_play: true,
            result: Ok(playing_status),
        });
        assert_eq!(app.status.state, PlaybackState::Playing);

        // 4. Stale GetStatus (gen 1) arrives later with Stopped -> must be rejected!
        app.apply_event(TuiEvent::StatusPolled {
            generation: 1,
            result: Ok(PlaybackStatus::default()),
        });
        assert_eq!(app.status.state, PlaybackState::Playing);
        assert!(app.status.current_track.is_some());

        // 5. Next poll at generation 3 succeeds and updates cleanly to Paused.
        app.request_poll_status();
        assert_eq!(app.take_effects(), vec![TuiEffect::PollStatus { generation: 3 }]);
        app.apply_event(TuiEvent::StatusPolled {
            generation: 3,
            result: Ok(PlaybackStatus {
                state: PlaybackState::Paused,
                ..PlaybackStatus::default()
            }),
        });
        assert_eq!(app.status.state, PlaybackState::Paused);

        // 6. Also reject stale poll arriving *before* an in-flight control finishes.
        app.request_poll_status();
        assert_eq!(app.take_effects(), vec![TuiEffect::PollStatus { generation: 4 }]);
        app.handle_key(press(KeyCode::Char(' '))); // generation 5
        assert!(matches!(
            app.take_effects().as_slice(),
            [TuiEffect::SendControl { generation: 5, request: IpcRequest::TogglePause }]
        ));
        app.apply_event(TuiEvent::StatusPolled {
            generation: 4,
            result: Ok(PlaybackStatus::default()),
        });
        assert_eq!(app.status.state, PlaybackState::Paused);

        app.apply_event(TuiEvent::ActionCompleted {
            generation: 5,
            is_play: false,
            result: Ok(PlaybackStatus {
                state: PlaybackState::Playing,
                ..PlaybackStatus::default()
            }),
        });
        assert_eq!(app.status.state, PlaybackState::Playing);
    }

    #[test]
    fn rejects_stale_library_reload_when_delete_starts() {
        let mut app = TuiApp::from_loaded(sample_library(), None);

        app.request_library_check();
        assert_eq!(
            app.take_effects(),
            vec![TuiEffect::CheckLibraryReload {
                generation: 1,
                known_stamp: None,
            }]
        );

        // Start delete (library_gen advances to 2).
        app.handle_key(press(KeyCode::Down));
        app.handle_key(press(KeyCode::Char('d')));
        app.handle_key(press(KeyCode::Char('y')));
        let _ = app.take_effects();

        // Stale reload from generation 1 tries to restore extra songs -> must be ignored!
        let mut bloated = sample_library();
        bloated
            .songs
            .push(sample_song("s99", "Stale Ghost", None, 42.0));
        app.apply_event(TuiEvent::LibraryChecked {
            generation: 1,
            observed_stamp: None,
            result: Ok(Some(bloated)),
        });
        assert_eq!(app.library.songs.len(), 3);

        // Delete completes cleanly.
        let mut after_delete = sample_library();
        let outcome = after_delete.delete_target("Chill", TargetKind::Playlist).unwrap();
        app.apply_event(TuiEvent::DeleteFinished {
            generation: 2,
            result: Ok((after_delete, None, outcome)),
        });
        assert_eq!(app.library.playlists.len(), 1);
    }

    #[test]
    fn offline_error_classification_keeps_real_ipc_failures_visible() {
        assert!(is_daemon_offline_error(&TunrError::Ipc(
            "playback daemon is not running at /tmp/x.sock".to_string()
        )));
        assert!(is_daemon_offline_error(&TunrError::Io(
            io::Error::from(io::ErrorKind::ConnectionRefused)
        )));
        assert!(!is_daemon_offline_error(&TunrError::Ipc(
            "timed out waiting for daemon stop".to_string()
        )));
        assert!(!is_daemon_offline_error(&TunrError::Daemon(
            "engine failed".to_string()
        )));
        assert!(!is_daemon_offline_error(&TunrError::Io(
            io::Error::from(io::ErrorKind::TimedOut)
        )));
        assert!(is_daemon_offline_error(&TunrError::Io(
            io::Error::from(io::ErrorKind::NotConnected)
        )));
    }

    #[test]
    fn repeat_events_allow_navigation_and_seek_but_ignore_actions_and_quit() {
        let mut app = TuiApp::from_loaded(sample_library(), None);

        // Switch pane with repeat 'l' and move down with repeat Down.
        app.handle_key(KeyEvent::new_with_kind(
            KeyCode::Char('l'),
            KeyModifiers::NONE,
            KeyEventKind::Repeat,
        ));
        assert_eq!(app.focus(), FocusPane::Tracks);

        app.handle_key(KeyEvent::new_with_kind(
            KeyCode::Down,
            KeyModifiers::NONE,
            KeyEventKind::Repeat,
        ));
        assert_eq!(app.track_state.selected(), Some(1));

        // Repeat '[' / ']' still dispatches seek requests.
        app.handle_key(KeyEvent::new_with_kind(
            KeyCode::Char(']'),
            KeyModifiers::NONE,
            KeyEventKind::Repeat,
        ));
        assert_eq!(
            app.take_effects(),
            vec![TuiEffect::SendControl {
                generation: 1,
                request: IpcRequest::SeekRelative(SEEK_STEP_SECS),
            }]
        );

        // Repeat on toggles/playback/delete/quit must be ignored.
        for code in [
            KeyCode::Enter,
            KeyCode::Tab,
            KeyCode::Char(' '),
            KeyCode::Char('p'),
            KeyCode::Char('s'),
            KeyCode::Char('d'),
            KeyCode::Char('q'),
            KeyCode::Char('Q'),
            KeyCode::Esc,
        ] {
            app.handle_key(KeyEvent::new_with_kind(
                code,
                KeyModifiers::NONE,
                KeyEventKind::Repeat,
            ));
        }
        assert!(!app.should_quit());
        assert_eq!(app.mode(), PlayMode::Next);
        assert_eq!(app.focus(), FocusPane::Tracks);
        assert!(app.confirm_delete.is_none());
        assert!(app.take_effects().is_empty());

        // Ctrl+Alt+C must not trigger Ctrl+C quit.
        app.handle_key(KeyEvent::new_with_kind(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
            KeyEventKind::Press,
        ));
        assert!(!app.should_quit());

        // Vim 'h' switches back to Playlists.
        app.handle_key(press(KeyCode::Char('h')));
        assert_eq!(app.focus(), FocusPane::Playlists);
    }

    #[cfg(windows)]
    #[test]
    fn windows_altgr_punctuation_reaches_seek_binding() {
        let mut app = TuiApp::from_loaded(sample_library(), None);
        app.handle_key(KeyEvent::new_with_kind(
            KeyCode::Char('['),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
            KeyEventKind::Press,
        ));
        assert_eq!(
            app.take_effects(),
            vec![TuiEffect::SendControl {
                generation: 1,
                request: IpcRequest::SeekRelative(-SEEK_STEP_SECS),
            }]
        );
    }

    #[test]
    fn narrow_terminal_and_ascii_mode_render_cleanly() {
        let mut app = TuiApp::from_loaded(sample_library(), None);
        app.status.state = PlaybackState::Paused;
        app.status.current_track = Some(app.library.songs[0].clone());

        let backend = TestBackend::new(50, 18);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| app.render_with_ascii(frame, true))
            .unwrap();
        let screen = terminal.backend().to_string();

        assert!(screen.contains("Playlists"), "missing playlists pane:\n{screen}");
        assert!(screen.contains("Tracks:"), "missing tracks pane:\n{screen}");
        assert!(screen.contains("|| Alpha Dawn"), "missing ASCII pause icon:\n{screen}");
        assert!(!screen.contains('╭'), "unexpected rounded border in ASCII mode:\n{screen}");
        assert!(!screen.contains('▶'), "unexpected unicode play glyph in ASCII mode:\n{screen}");
    }

    #[test]
    fn render_header_displays_loop_badge_only_when_loop_playback_is_enabled() {
        let mut app = TuiApp::from_loaded(sample_library(), None);
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        app.status.state = PlaybackState::Playing;
        app.status.loop_playback = false;
        terminal.draw(|frame| app.render(frame)).unwrap();
        let without_loop = terminal.backend().to_string();
        assert!(without_loop.contains("[Mode: Next]"), "missing mode:\n{without_loop}");
        assert!(
            !without_loop.contains("[Loop]"),
            "unexpected [Loop] when disabled:\n{without_loop}"
        );

        app.status.loop_playback = true;
        terminal.draw(|frame| app.render(frame)).unwrap();
        let with_loop = terminal.backend().to_string();
        assert!(with_loop.contains("[Mode: Next]"), "missing mode:\n{with_loop}");
        assert!(with_loop.contains("[Loop]"), "missing [Loop] when enabled:\n{with_loop}");
    }

    #[test]
    fn volume_keybindings_support_press_repeat_and_clamp_bounds() {
        let mut app = TuiApp::from_loaded(sample_library(), None);
        assert_eq!(app.volume(), DEFAULT_VOLUME);
        assert_eq!(app.status.volume, DEFAULT_VOLUME);

        // '-' Press decrements by 5.
        app.handle_key(press(KeyCode::Char('-')));
        assert_eq!(app.volume(), 45);
        assert_eq!(app.status.volume, 45);
        assert_eq!(
            app.take_effects(),
            vec![TuiEffect::SetVolume {
                generation: 1,
                volume: 45,
            }]
        );

        // '+' Repeat increments by 5.
        app.handle_key(KeyEvent::new_with_kind(
            KeyCode::Char('+'),
            KeyModifiers::NONE,
            KeyEventKind::Repeat,
        ));
        assert_eq!(app.volume(), 50);
        assert_eq!(
            app.take_effects(),
            vec![TuiEffect::SetVolume {
                generation: 2,
                volume: 50,
            }]
        );

        // '=' Press and Repeat increment by 5 each.
        app.handle_key(press(KeyCode::Char('=')));
        app.handle_key(KeyEvent::new_with_kind(
            KeyCode::Char('='),
            KeyModifiers::NONE,
            KeyEventKind::Repeat,
        ));
        assert_eq!(app.volume(), 60);
        assert_eq!(
            app.take_effects(),
            vec![
                TuiEffect::SetVolume {
                    generation: 3,
                    volume: 55,
                },
                TuiEffect::SetVolume {
                    generation: 4,
                    volume: 60,
                },
            ]
        );

        // Clamps cleanly at 0.
        let mut low = TuiApp::from_loaded(sample_library(), None).with_volume(3);
        low.handle_key(press(KeyCode::Char('-')));
        assert_eq!(low.volume(), 0);
        assert_eq!(low.status.volume, 0);
        low.handle_key(press(KeyCode::Char('-')));
        assert_eq!(low.volume(), 0);

        // Clamps cleanly at MAX_VOLUME (100).
        let mut high = TuiApp::from_loaded(sample_library(), None).with_volume(97);
        high.handle_key(press(KeyCode::Char('+')));
        assert_eq!(high.volume(), MAX_VOLUME);
        assert_eq!(high.status.volume, MAX_VOLUME);
        high.handle_key(press(KeyCode::Char('=')));
        assert_eq!(high.volume(), MAX_VOLUME);
    }

    #[test]
    fn volume_generation_protects_against_stale_status_and_offline_preserves_state() {
        let (root, paths) = temp_paths("vol-gen");
        let lib = sample_library();
        let mut app = TuiApp::from_loaded(lib, None);

        // Toggle local mode to Shuffle and start a non-volume control at gen 1.
        app.handle_key(press(KeyCode::Char('s')));
        assert_eq!(app.mode(), PlayMode::Shuffle);
        app.handle_key(press(KeyCode::Char(' '))); // gen 1
        // Rapidly press '+' twice (gen 2 -> 55, gen 3 -> 60).
        app.handle_key(press(KeyCode::Char('+')));
        app.handle_key(press(KeyCode::Char('+')));
        let effects = app.take_effects();
        assert_eq!(effects.len(), 3);
        assert_eq!(app.volume(), 60);
        assert_eq!(app.volume_gen, 3);

        // Earlier TogglePause (gen 1) completes with old volume 50 -> must NOT overwrite 60.
        app.apply_event(TuiEvent::ActionCompleted {
            generation: 1,
            is_play: false,
            result: Ok(PlaybackStatus {
                state: PlaybackState::Paused,
                mode: PlayMode::Shuffle,
                volume: 50,
                ..PlaybackStatus::default()
            }),
        });
        assert_eq!(app.status.state, PlaybackState::Paused);
        assert_eq!(app.volume(), 60);
        assert_eq!(app.status.volume, 60);

        // Earlier SetVolume (gen 2) completes with 55 -> must NOT overwrite 60.
        app.apply_event(TuiEvent::VolumeUpdated {
            generation: 2,
            result: Ok(Some(PlaybackStatus {
                state: PlaybackState::Paused,
                mode: PlayMode::Shuffle,
                volume: 55,
                ..PlaybackStatus::default()
            })),
        });
        assert_eq!(app.volume(), 60);
        assert_eq!(app.status.volume, 60);

        // Latest SetVolume (gen 3) completes with 60.
        app.apply_event(TuiEvent::VolumeUpdated {
            generation: 3,
            result: Ok(Some(PlaybackStatus {
                state: PlaybackState::Paused,
                mode: PlayMode::Shuffle,
                volume: 60,
                ..PlaybackStatus::default()
            })),
        });
        assert_eq!(app.volume(), 60);
        assert_eq!(app.status.volume, 60);

        // Offline poll calls reset_status_stopped and must preserve volume and mode.
        app.request_poll_status(); // gen 4
        let mut offline_mock = MockState {
            offline: true,
            ..MockState::default()
        };
        drive_effects(&mut app, &paths, &mut offline_mock);
        assert_eq!(app.status.state, PlaybackState::Stopped);
        assert_eq!(app.mode(), PlayMode::Shuffle);
        assert_eq!(app.volume(), 60);
        assert_eq!(app.status.volume, 60);

        // Offline SetVolume is benign: no error message, preserves mode and updated volume.
        app.handle_key(press(KeyCode::Char('-'))); // 55
        drive_effects(&mut app, &paths, &mut offline_mock);
        assert_eq!(app.volume(), 55);
        assert_eq!(app.status.volume, 55);
        assert_eq!(app.mode(), PlayMode::Shuffle);
        assert_eq!(app.status.state, PlaybackState::Stopped);
        assert!(app.status_message.is_none());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn tui_app_new_loads_persisted_workspace_volume_and_with_volume_clamps() {
        let root = std::env::temp_dir().join(format!("tunr-tui-init-vol-{}", Uuid::new_v4()));
        let cfg_home = root.join("xdg_config");
        let run_dir = root.join("run");
        let ws = root.join("ws");

        paths::initialize_workspace_with_env(
            &ws,
            &root,
            Some(cfg_home.clone().into_os_string()),
            Some(root.join("home")),
        )
        .unwrap();
        let paths = AppPaths::resolve_from_env(
            Some(cfg_home.into_os_string()),
            Some(root.join("home")),
            Some(run_dir.into_os_string()),
            "1000",
        )
        .unwrap();
        sample_library().save(&paths.library_path).unwrap();

        // Unset workspace volume defaults to DEFAULT_VOLUME (50).
        let default_app = TuiApp::new(paths.clone()).unwrap();
        assert_eq!(default_app.volume(), DEFAULT_VOLUME);
        assert_eq!(default_app.status.volume, DEFAULT_VOLUME);

        // Persisted workspace volume is loaded on startup.
        paths::set_workspace_volume(&paths, 80).unwrap();
        let loaded_app = TuiApp::new(paths).unwrap();
        assert_eq!(loaded_app.volume(), 80);
        assert_eq!(loaded_app.status.volume, 80);

        // with_volume clamps out-of-range inputs cleanly.
        let clamped = TuiApp::from_loaded(sample_library(), None).with_volume(150);
        assert_eq!(clamped.volume(), MAX_VOLUME);
        assert_eq!(clamped.status.volume, MAX_VOLUME);

        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn execute_set_volume_persists_to_config_when_daemon_offline() {
        let root = std::env::temp_dir().join(format!("tunr-tui-exec-vol-{}", Uuid::new_v4()));
        let cfg_home = root.join("xdg_config");
        let run_dir = root.join("run");
        let ws = root.join("ws");

        paths::initialize_workspace_with_env(
            &ws,
            &root,
            Some(cfg_home.clone().into_os_string()),
            Some(root.join("home")),
        )
        .unwrap();
        let paths = AppPaths::resolve_from_env(
            Some(cfg_home.into_os_string()),
            Some(root.join("home")),
            Some(run_dir.into_os_string()),
            "1000",
        )
        .unwrap();

        let outcome = execute_set_volume(&paths, 35).await.unwrap();
        assert!(outcome.is_none());
        let cfg = paths::load_workspace_config(&paths).unwrap();
        assert_eq!(paths::effective_volume(&cfg), 35);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn renders_volume_in_header_and_help_in_unicode_and_ascii() {
        let mut app = TuiApp::from_loaded(sample_library(), None).with_volume(75);
        app.status.state = PlaybackState::Playing;

        // Wide threshold (130 outer -> 128 inner) renders full wide help without clipping in ASCII & Unicode.
        for ascii in [false, true] {
            let backend = TestBackend::new(130, 24);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| app.render_with_ascii(frame, ascii))
                .unwrap();
            let screen = terminal.backend().to_string();
            assert!(
                screen.contains("Vol: 75%"),
                "missing header volume (ascii={ascii}):\n{screen}"
            );
            assert!(
                screen.contains("-/+:Vol"),
                "missing footer volume shortcut (ascii={ascii}):\n{screen}"
            );
            assert!(
                screen.contains("Q:Stop+Quit"),
                "wide footer help clipped at 128 inner width (ascii={ascii}):\n{screen}"
            );
        }

        // Compact width (100 outer) also shows both header volume and compact -/+:Vol shortcut.
        for ascii in [false, true] {
            let backend = TestBackend::new(100, 24);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| app.render_with_ascii(frame, ascii))
                .unwrap();
            let screen = terminal.backend().to_string();
            assert!(
                screen.contains("Vol: 75%"),
                "missing compact header volume (ascii={ascii}):\n{screen}"
            );
            assert!(
                screen.contains("-/+:Vol"),
                "missing compact footer volume shortcut (ascii={ascii}):\n{screen}"
            );
        }
    }
}
