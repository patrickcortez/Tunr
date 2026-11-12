use std::fs;
use std::io::{self, stdout};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

use crossterm::cursor::Show;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Clear, Gauge, HighlightSpacing, List, ListItem, ListState, Paragraph, Wrap,
};

use crate::error::{Result, TermusicError};
use crate::ipc::{self, IpcRequest, PlayMode, PlaybackState, PlaybackStatus, TargetKind};
use crate::library::{DeleteOutcome, Library, Song};
use crate::paths::AppPaths;

const SYNC_INTERVAL: Duration = Duration::from_millis(150);
const SEEK_STEP_SECS: f64 = 5.0;
const STATUS_TTL: Duration = Duration::from_secs(5);

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

trait IpcClient {
    fn send(&mut self, paths: &AppPaths, req: &IpcRequest) -> Result<PlaybackStatus>;
    fn ensure_and_send(&mut self, paths: &AppPaths, req: &IpcRequest) -> Result<PlaybackStatus>;
}

struct RealIpcClient;

impl IpcClient for RealIpcClient {
    fn send(&mut self, paths: &AppPaths, req: &IpcRequest) -> Result<PlaybackStatus> {
        ipc::send_request(paths, req)
    }

    fn ensure_and_send(&mut self, paths: &AppPaths, req: &IpcRequest) -> Result<PlaybackStatus> {
        ipc::ensure_daemon_and_send(paths, req)
    }
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(stdout(), Show);
        ratatui::restore();
    }
}

/// Stateful model and renderer for the interactive Termusic terminal interface.
pub struct TuiApp {
    paths: AppPaths,
    library: Library,
    library_stamp: Option<(SystemTime, u64)>,
    focus: FocusPane,
    playlist_state: ListState,
    track_state: ListState,
    mode: PlayMode,
    mode_locked: bool,
    status: PlaybackStatus,
    confirm_delete: Option<ConfirmDelete>,
    status_message: Option<StatusMessage>,
    should_quit: bool,
    ipc: Box<dyn IpcClient>,
}

impl TuiApp {
    /// Create a new TUI state backed by the real library file and Unix socket IPC client.
    pub fn new(paths: AppPaths) -> Result<Self> {
        let library = Library::load(&paths.library_path)?;
        let library_stamp = file_stamp(&paths.library_path);
        Ok(Self::build(paths, library, library_stamp, Box::new(RealIpcClient)))
    }

    fn build(
        paths: AppPaths,
        library: Library,
        library_stamp: Option<(SystemTime, u64)>,
        ipc: Box<dyn IpcClient>,
    ) -> Self {
        let mut playlist_state = ListState::default();
        playlist_state.select(Some(0));

        let mut track_state = ListState::default();
        if !library.songs.is_empty() {
            track_state.select(Some(0));
        }

        Self {
            paths,
            library,
            library_stamp,
            focus: FocusPane::Playlists,
            playlist_state,
            track_state,
            mode: PlayMode::Next,
            mode_locked: false,
            status: PlaybackStatus::default(),
            confirm_delete: None,
            status_message: None,
            should_quit: false,
            ipc,
        }
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

    /// Reload `library.json` from disk if its modification timestamp or byte size changed externally.
    pub fn reload_library_if_changed(&mut self) {
        let current_stamp = file_stamp(&self.paths.library_path);
        if current_stamp == self.library_stamp {
            return;
        }

        let prev_playlist_name = self.selected_playlist_name();
        let prev_track_id = self.selected_track().map(|s| s.id);
        let prev_track_idx = self.track_state.selected();

        match Library::load(&self.paths.library_path) {
            Ok(new_lib) => {
                self.library = new_lib;
                self.library_stamp = current_stamp;
                self.restore_selection(prev_playlist_name.as_deref(), prev_track_id.as_deref(), prev_track_idx);
            }
            Err(err) => {
                self.library_stamp = current_stamp;
                self.set_message(format!("Library reload failed: {err}"), true);
            }
        }
    }

    /// Query daemon playback status, treating an absent daemon cleanly as `Stopped`.
    pub fn poll_status(&mut self) {
        match self.ipc.send(&self.paths, &IpcRequest::GetStatus) {
            Ok(status) => {
                if !self.mode_locked && status.state != PlaybackState::Stopped {
                    self.mode = status.mode;
                    self.mode_locked = true;
                }
                self.status = status;
            }
            Err(err) if is_daemon_offline_error(&err) => {
                self.status = PlaybackStatus {
                    mode: self.mode,
                    ..PlaybackStatus::default()
                };
            }
            Err(err) => {
                self.status = PlaybackStatus {
                    mode: self.mode,
                    ..PlaybackStatus::default()
                };
                self.set_message(format!("Status error: {err}"), true);
            }
        }
    }

    /// Handle a single terminal key event. Non-press events are ignored.
    pub fn handle_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
        {
            self.should_quit = true;
            return;
        }

        if key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
            return;
        }

        if self.confirm_delete.is_some() {
            self.handle_modal_key(key);
            return;
        }

        match key.code {
            KeyCode::Left => {
                self.focus = FocusPane::Playlists;
            }
            KeyCode::Right => {
                self.focus = FocusPane::Tracks;
                self.ensure_track_selected();
            }
            KeyCode::Tab | KeyCode::BackTab => {
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
            KeyCode::Enter => self.handle_enter(),
            KeyCode::Char('p') => self.handle_play_shortcut(),
            KeyCode::Char(' ') => self.send_control(IpcRequest::TogglePause),
            KeyCode::Char('n') => self.send_control(IpcRequest::Next),
            KeyCode::Char('b') => self.send_control(IpcRequest::Prev),
            KeyCode::Char('[') => self.send_control(IpcRequest::SeekRelative(-SEEK_STEP_SECS)),
            KeyCode::Char(']') => self.send_control(IpcRequest::SeekRelative(SEEK_STEP_SECS)),
            KeyCode::Char('s') => self.toggle_mode(),
            KeyCode::Char('d') => self.open_delete_modal(),
            KeyCode::Char('q') | KeyCode::Esc => {
                self.should_quit = true;
            }
            KeyCode::Char('Q') => {
                match self.ipc.send(&self.paths, &IpcRequest::Stop) {
                    Ok(status) => self.status = status,
                    Err(err) if is_daemon_offline_error(&err) => {}
                    Err(_) => {}
                }
                self.should_quit = true;
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
        let req = IpcRequest::Play {
            queue,
            start_index,
            mode: self.mode,
            playlist_name,
        };
        match self.ipc.ensure_and_send(&self.paths, &req) {
            Ok(status) => {
                self.status = status;
                self.status_message = None;
            }
            Err(err) => {
                self.set_message(format!("Play failed: {err}"), true);
            }
        }
    }

    fn send_control(&mut self, req: IpcRequest) {
        match self.ipc.send(&self.paths, &req) {
            Ok(status) => {
                self.status = status;
                self.status_message = None;
            }
            Err(err) if is_daemon_offline_error(&err) => {
                self.status = PlaybackStatus {
                    mode: self.mode,
                    ..PlaybackStatus::default()
                };
                self.set_message("Playback daemon is not running.", false);
            }
            Err(err) => {
                self.set_message(format!("{err}"), true);
            }
        }
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
        let Some(pending) = self.confirm_delete.take() else {
            return;
        };

        let prev_playlist_name = self.selected_playlist_name();
        let prev_track_id = self.selected_track().map(|s| s.id);
        let prev_track_idx = self.track_state.selected();

        match self.library.delete_target(&pending.query, pending.kind) {
            Ok(outcome) => {
                if let Err(err) = self.library.save(&self.paths.library_path) {
                    self.set_message(format!("Save failed: {err}"), true);
                    return;
                }
                self.library_stamp = file_stamp(&self.paths.library_path);
                self.restore_selection(
                    prev_playlist_name.as_deref(),
                    prev_track_id.as_deref(),
                    prev_track_idx,
                );
                match outcome {
                    DeleteOutcome::Song(song) => {
                        self.set_message(format!("Removed song '{}' from library.", song.title), false);
                    }
                    DeleteOutcome::Playlist(pl) => {
                        self.set_message(format!("Deleted playlist '{}'.", pl.name), false);
                    }
                }
            }
            Err(err) => {
                self.set_message(format!("Delete failed: {err}"), true);
            }
        }
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
        let area = frame.area();
        let [header_area, main_area, footer_area] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(5),
        ])
        .areas(area);

        self.render_header(frame, header_area);
        self.render_main(frame, main_area);
        self.render_footer(frame, footer_area);

        if let Some(modal) = self.confirm_delete.clone() {
            self.render_delete_modal(frame, area, &modal);
        }
    }

    fn render_header(&self, frame: &mut Frame, area: Rect) {
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

        let title_line = Line::from(vec![
            Span::styled(
                concat!(" Termusic v", env!("CARGO_PKG_VERSION")),
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
            Span::styled("   |   ", Style::new().fg(Color::DarkGray)),
            Span::styled(
                mode_text,
                Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD),
            ),
            Span::styled("   |   ", Style::new().fg(Color::DarkGray)),
            Span::styled(state_text, state_style),
        ]);

        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(Color::DarkGray));
        let paragraph = Paragraph::new(title_line).block(block);
        frame.render_widget(paragraph, area);
    }

    fn render_main(&mut self, frame: &mut Frame, area: Rect) {
        if area.is_empty() {
            return;
        }
        let [playlists_area, tracks_area] = Layout::horizontal([
            Constraint::Percentage(30),
            Constraint::Percentage(70),
        ])
        .areas(area);

        self.render_playlists(frame, playlists_area);
        self.render_tracks(frame, tracks_area);
    }

    fn render_playlists(&mut self, frame: &mut Frame, area: Rect) {
        if area.is_empty() {
            return;
        }
        let focused = self.focus == FocusPane::Playlists;
        let border_style = pane_border_style(focused);
        let title_style = pane_title_style(focused);

        let block = Block::bordered()
            .border_type(BorderType::Rounded)
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

    fn render_tracks(&mut self, frame: &mut Frame, area: Rect) {
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
            .border_type(BorderType::Rounded)
            .border_style(border_style)
            .title(Span::styled(format!(" Tracks: {context_label} "), title_style));

        let tracks = self.visible_tracks();
        if tracks.is_empty() {
            self.track_state.select(None);
            let inner = block.inner(area);
            frame.render_widget(block, area);
            if !inner.is_empty() {
                let empty = Paragraph::new(
                    "No tracks in this view.\nRun `termusic add <path>` to import music.",
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
                let prefix = if is_active { "▶ " } else { "  " };
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

    fn render_footer(&self, frame: &mut Frame, area: Rect) {
        if area.is_empty() {
            return;
        }
        let mut block = Block::bordered()
            .border_type(BorderType::Rounded)
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
                    PlaybackState::Playing => "▶ ",
                    PlaybackState::Paused => "⏸ ",
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

        let help_line = Line::from(Span::styled(
            "←/→/Tab:Pane  ↑/↓/j/k:Nav  Enter:Play  p:PlayList  Space:Pause  n/b:Next/Prev  [/]:±5s  s:Mode  d:Del  q:Quit  Q:Stop+Quit",
            Style::new().fg(Color::Gray),
        ));
        frame.render_widget(Paragraph::new(help_line), help_area);
    }

    fn render_delete_modal(&self, frame: &mut Frame, area: Rect, modal: &ConfirmDelete) {
        if area.width < 10 || area.height < 5 {
            return;
        }
        let popup = area.centered(
            Constraint::Length(area.width.min(64)),
            Constraint::Length(area.height.min(7)),
        );
        frame.render_widget(Clear, popup);

        let block = Block::bordered()
            .border_type(BorderType::Double)
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
pub fn run_tui(paths: &AppPaths) -> Result<()> {
    let mut app = TuiApp::new(paths.clone())?;
    app.poll_status();

    let mut terminal = match ratatui::try_init() {
        Ok(term) => term,
        Err(err) => {
            ratatui::restore();
            return Err(err.into());
        }
    };
    let _guard = TerminalGuard;

    let mut last_sync = Instant::now();

    while !app.should_quit {
        terminal.draw(|frame| app.render(frame))?;

        let timeout = SYNC_INTERVAL.saturating_sub(last_sync.elapsed());
        if event::poll(timeout)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    app.handle_key(key);
                }
                Event::Resize(_, _) => {}
                _ => {}
            }
        }

        if last_sync.elapsed() >= SYNC_INTERVAL {
            app.reload_library_if_changed();
            app.poll_status();
            app.clear_expired_message();
            last_sync = Instant::now();
        }
    }

    let _ = terminal.show_cursor();
    Ok(())
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

fn is_daemon_offline_error(err: &TermusicError) -> bool {
    match err {
        TermusicError::Ipc(_) => true,
        TermusicError::Io(io_err) => matches!(
            io_err.kind(),
            io::ErrorKind::NotFound
                | io::ErrorKind::ConnectionRefused
                | io::ErrorKind::ConnectionReset
                | io::ErrorKind::BrokenPipe
        ),
        _ => false,
    }
}

fn fit_to_width(text: &str, max_width: usize) -> String {
    let width = Span::raw(text).width();
    if width <= max_width {
        let mut out = String::with_capacity(text.len() + (max_width - width));
        out.push_str(text);
        for _ in 0..(max_width - width) {
            out.push(' ');
        }
        return out;
    }
    if max_width == 0 {
        return String::new();
    }
    if max_width == 1 {
        return "…".to_string();
    }
    let target = max_width - 1;
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let mut buf = [0u8; 4];
        let ch_str = ch.encode_utf8(&mut buf);
        let ch_w = Span::raw(&*ch_str).width();
        if used + ch_w > target {
            break;
        }
        out.push(ch);
        used += ch_w;
    }
    out.push('…');
    while used + 1 < max_width {
        out.push(' ');
        used += 1;
    }
    out
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
    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::rc::Rc;

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

    struct MockIpcClient {
        state: Rc<RefCell<MockState>>,
    }

    impl MockIpcClient {
        fn new(state: Rc<RefCell<MockState>>) -> Self {
            Self { state }
        }
    }

    impl IpcClient for MockIpcClient {
        fn send(&mut self, _paths: &AppPaths, req: &IpcRequest) -> Result<PlaybackStatus> {
            let mut st = self.state.borrow_mut();
            st.requests.push(req.clone());
            if st.offline {
                return Err(TermusicError::Ipc("daemon not running".to_string()));
            }
            Ok(st.next_status.clone().unwrap_or_default())
        }

        fn ensure_and_send(
            &mut self,
            _paths: &AppPaths,
            req: &IpcRequest,
        ) -> Result<PlaybackStatus> {
            let mut st = self.state.borrow_mut();
            st.requests.push(req.clone());
            if let IpcRequest::Play {
                queue,
                start_index,
                mode,
                playlist_name,
            } = req
            {
                let idx = start_index.unwrap_or(0);
                let current_track = queue.get(idx).cloned();
                let status = PlaybackStatus {
                    state: PlaybackState::Playing,
                    mode: *mode,
                    playlist_name: playlist_name.clone(),
                    current_track,
                    queue_index: idx,
                    queue_len: queue.len(),
                    position_secs: 0.0,
                    duration_secs: queue.get(idx).and_then(|s| s.duration_secs),
                    queue: queue.clone(),
                };
                st.next_status = Some(status.clone());
                return Ok(status);
            }
            Ok(st.next_status.clone().unwrap_or_default())
        }
    }

    fn temp_paths(label: &str) -> (PathBuf, AppPaths) {
        let root = std::env::temp_dir().join(format!(
            "termusic-tui-test-{label}-{}",
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
        let mock = Rc::new(RefCell::new(MockState::default()));
        let mut app = TuiApp::build(paths, lib, None, Box::new(MockIpcClient::new(mock.clone())));

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

        // Navigate down inside Tracks and press Enter -> sends Play for second song in Chill.
        app.handle_key(press(KeyCode::Down));
        assert_eq!(app.selected_track_index(), Some(1));
        app.handle_key(press(KeyCode::Enter));

        {
            let recorded = &mock.borrow().requests;
            assert_eq!(recorded.len(), 1);
            assert_eq!(
                recorded[0],
                IpcRequest::Play {
                    queue: chill_tracks,
                    start_index: Some(1),
                    mode: PlayMode::Next,
                    playlist_name: Some("Chill".to_string()),
                }
            );
        }

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
        let mock = Rc::new(RefCell::new(MockState::default()));
        let mut app = TuiApp::build(paths, lib, None, Box::new(MockIpcClient::new(mock.clone())));

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

        assert!(app.should_quit());
        let recorded = &mock.borrow().requests;
        assert!(matches!(
            &recorded[0],
            IpcRequest::Play {
                start_index: None,
                mode: PlayMode::Shuffle,
                playlist_name: None,
                queue,
            } if queue.len() == 3
        ));
        assert_eq!(recorded[1], IpcRequest::TogglePause);
        assert_eq!(recorded[2], IpcRequest::Next);
        assert_eq!(recorded[3], IpcRequest::Prev);
        assert_eq!(recorded[4], IpcRequest::SeekRelative(-5.0));
        assert_eq!(recorded[5], IpcRequest::SeekRelative(5.0));
        assert!(matches!(
            &recorded[6],
            IpcRequest::Play {
                start_index: Some(1),
                mode: PlayMode::Shuffle,
                playlist_name: None,
                queue,
            } if queue.len() == 3
        ));
        assert_eq!(recorded[7], IpcRequest::Stop);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn offline_daemon_poll_shows_stopped_and_q_quits_without_stop() {
        let (root, paths) = temp_paths("offline");
        let mock = Rc::new(RefCell::new(MockState {
            offline: true,
            ..MockState::default()
        }));
        let mut app = TuiApp::build(
            paths,
            sample_library(),
            None,
            Box::new(MockIpcClient::new(mock.clone())),
        );

        app.poll_status();
        assert_eq!(app.status.state, PlaybackState::Stopped);

        app.handle_key(press(KeyCode::Char('q')));
        assert!(app.should_quit());
        // Only GetStatus was sent; q must not send Stop or Shutdown.
        assert_eq!(mock.borrow().requests, vec![IpcRequest::GetStatus]);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn delete_modal_and_external_reload() {
        let (root, paths) = temp_paths("delete");
        let lib = sample_library();
        lib.save(&paths.library_path).unwrap();
        let stamp = file_stamp(&paths.library_path);

        let mock = Rc::new(RefCell::new(MockState::default()));
        let mut app = TuiApp::build(
            paths.clone(),
            lib,
            stamp,
            Box::new(MockIpcClient::new(mock)),
        );

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

        // Confirm delete playlist with 'y' -> removes playlist and saves library.json.
        app.handle_key(press(KeyCode::Char('d')));
        app.handle_key(press(KeyCode::Char('y')));
        assert!(app.confirm_delete.is_none());
        assert_eq!(app.library.playlists.len(), 1);
        assert_eq!(Library::load(&paths.library_path).unwrap().playlists.len(), 1);

        // Simulate external CLI edit modifying library.json.
        std::thread::sleep(Duration::from_millis(20));
        let mut external = Library::load(&paths.library_path).unwrap();
        external
            .songs
            .push(sample_song("s4", "Delta Wave", Some("Artist D"), 180.0));
        external.save(&paths.library_path).unwrap();

        app.reload_library_if_changed();
        assert_eq!(app.library.songs.len(), 4);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn renders_layout_and_modal_into_test_backend() {
        let (root, paths) = temp_paths("render");
        let lib = sample_library();
        let active_song = lib.songs[1].clone();
        let mock = Rc::new(RefCell::new(MockState {
            next_status: Some(PlaybackStatus {
                state: PlaybackState::Playing,
                mode: PlayMode::Next,
                playlist_name: Some("Chill".to_string()),
                current_track: Some(active_song.clone()),
                queue_index: 0,
                queue_len: 2,
                position_secs: 65.0,
                duration_secs: Some(200.0),
                queue: vec![active_song, lib.songs[2].clone()],
            }),
            ..MockState::default()
        }));

        let mut app = TuiApp::build(paths, lib, None, Box::new(MockIpcClient::new(mock)));
        app.poll_status();
        app.handle_key(press(KeyCode::Char('s'))); // toggle mode to Shuffle
        app.handle_key(press(KeyCode::Right)); // focus Tracks

        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();

        let screen = terminal.backend().to_string();
        assert!(screen.contains("Termusic v0.1.0"), "missing header title:\n{screen}");
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
}
