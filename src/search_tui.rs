use std::path::PathBuf;
use std::time::Duration;

use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, HighlightSpacing, List, ListItem, ListState, Paragraph, Wrap,
};
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::download::{DownloadFormat, DownloadOptions, DownloadOutcome};
use crate::download_tui::{self, DownloadTuiOutcome};
use crate::error::{Result, TunrError};
use crate::paths::AppPaths;
use crate::platform::{ClipboardMethod, ShutdownSignals, copy_to_clipboard};
use crate::search::{self, SearchProvider, SearchResult};
use crate::theme::Theme;
use crate::tui_common::{
    SPINNER_FRAMES, TerminalSession, active_spinner, border_type, format_duration,
    paint_background, passive_highlight_style, truncate_to_width, use_ascii_glyphs,
};

const TUI_TICK_INTERVAL: Duration = Duration::from_millis(80);
const DEFAULT_PAGE_STEP: usize = 5;

/// Runtime configuration shared between CLI search dispatch, TUI, and plain output.
#[derive(Debug, Clone)]
pub struct SearchRunConfig {
    pub limit: usize,
    pub provider: SearchProvider,
    pub download_mode: bool,
    pub format: DownloadFormat,
    pub paths: Option<AppPaths>,
    pub dest: Option<PathBuf>,
    pub theme: Theme,
    pub theme_warning: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SearchPhase {
    Idle,
    Searching { tick: usize },
    Results,
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SearchAction {
    StartSearch(String),
    CopyUrl(String),
    DownloadUrl { url: String, title: String },
    Quit,
}

/// Interactive ratatui state machine for search results browsing, URL copying, and downloads.
#[derive(Debug, Clone)]
pub struct SearchAppState {
    query_input: String,
    cursor_pos: usize,
    active_query: Option<String>,
    limit: usize,
    provider: SearchProvider,
    download_mode: bool,
    format: DownloadFormat,
    editing_query: bool,
    phase: SearchPhase,
    results: Vec<SearchResult>,
    list_state: ListState,
    status_toast: Option<(String, bool)>,
    ascii: bool,
    page_step: usize,
    theme: Theme,
}

impl SearchAppState {
    /// Initialize search TUI state from an optional initial query and result limit.
    pub fn new(initial_query: Option<String>, limit: usize) -> Self {
        Self::with_ascii(initial_query, limit, use_ascii_glyphs())
    }

    /// Initialize search TUI state from an optional initial query and full [`SearchRunConfig`].
    pub fn from_run_config(initial_query: Option<String>, cfg: &SearchRunConfig) -> Self {
        let mut state = Self::with_ascii_and_config(
            initial_query,
            cfg.limit,
            cfg.provider,
            cfg.download_mode,
            cfg.format,
            use_ascii_glyphs(),
        )
        .with_theme(cfg.theme.clone());
        if let Some(warn) = &cfg.theme_warning {
            state.status_toast = Some((warn.clone(), true));
        }
        state
    }

    /// Apply a [`Theme`] to the search TUI state.
    pub fn with_theme(mut self, theme: Theme) -> Self {
        self.theme = theme;
        self
    }

    /// Return the active [`Theme`].
    pub fn theme(&self) -> &Theme {
        &self.theme
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn with_ascii(initial_query: Option<String>, limit: usize, ascii: bool) -> Self {
        Self::with_ascii_and_config(
            initial_query,
            limit,
            SearchProvider::YouTube,
            false,
            DownloadFormat::Mp3,
            ascii,
        )
    }

    fn with_ascii_and_config(
        initial_query: Option<String>,
        limit: usize,
        provider: SearchProvider,
        download_mode: bool,
        format: DownloadFormat,
        ascii: bool,
    ) -> Self {
        let trimmed = initial_query
            .map(|q| q.trim().to_string())
            .filter(|q| !q.is_empty());

        let (query_input, cursor_pos, active_query, editing_query, phase) = match trimmed {
            Some(q) => {
                let len = q.len();
                (q.clone(), len, Some(q), false, SearchPhase::Searching { tick: 0 })
            }
            None => (String::new(), 0, None, true, SearchPhase::Idle),
        };

        Self {
            query_input,
            cursor_pos,
            active_query,
            limit,
            provider,
            download_mode,
            format,
            editing_query,
            phase,
            results: Vec::new(),
            list_state: ListState::default(),
            status_toast: None,
            ascii,
            page_step: DEFAULT_PAGE_STEP,
            theme: Theme::default(),
        }
    }

    /// Advance the loading spinner frame while a search is active.
    pub fn tick(&mut self) {
        if let SearchPhase::Searching { tick } = &mut self.phase {
            *tick = (*tick + 1) % SPINNER_FRAMES.len();
        }
    }

    fn apply_search_result(&mut self, res: Result<Vec<SearchResult>>) {
        match res {
            Ok(items) => {
                self.results = items;
                *self.list_state.offset_mut() = 0;
                if self.results.is_empty() {
                    self.list_state.select(None);
                } else {
                    self.list_state.select(Some(0));
                }
                self.phase = SearchPhase::Results;
            }
            Err(err) => {
                self.results.clear();
                self.list_state.select(None);
                self.phase = SearchPhase::Error(err.to_string());
            }
        }
    }

    fn apply_copy_result(&mut self, url: &str, res: Result<ClipboardMethod>) {
        match res {
            Ok(method) => {
                self.status_toast = Some((format!("Copied via {}: {url}", method.label()), false));
            }
            Err(err) => {
                self.status_toast = Some((format!("Copy failed: {err}"), true));
            }
        }
    }

    fn selected_result(&self) -> Option<&SearchResult> {
        let idx = self.list_state.selected()?;
        self.results.get(idx)
    }

    fn move_by(&mut self, delta: isize) {
        let len = self.results.len();
        if len == 0 {
            self.list_state.select(None);
            return;
        }
        let cur = self.list_state.selected().unwrap_or(0).min(len - 1);
        let next = if delta < 0 {
            cur.saturating_sub(delta.unsigned_abs())
        } else {
            cur.saturating_add(delta as usize).min(len - 1)
        };
        self.list_state.select(Some(next));
    }

    fn jump_top(&mut self) {
        if self.results.is_empty() {
            self.list_state.select(None);
        } else {
            self.list_state.select(Some(0));
        }
    }

    fn jump_bottom(&mut self) {
        if self.results.is_empty() {
            self.list_state.select(None);
        } else {
            self.list_state.select(Some(self.results.len() - 1));
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> Option<SearchAction> {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return None;
        }
        let is_press = key.kind == KeyEventKind::Press;

        if key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT)
            && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
        {
            return is_press.then_some(SearchAction::Quit);
        }

        if self.editing_query {
            return self.handle_edit_key(key, is_press);
        }

        if key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            && !is_windows_altgr_char(&key)
        {
            return None;
        }

        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_by(-1);
                None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_by(1);
                None
            }
            KeyCode::PageUp => {
                let step = self.page_step as isize;
                self.move_by(-step);
                None
            }
            KeyCode::PageDown => {
                let step = self.page_step as isize;
                self.move_by(step);
                None
            }
            KeyCode::Char('g') | KeyCode::Home => {
                self.jump_top();
                None
            }
            KeyCode::Char('G') | KeyCode::End => {
                self.jump_bottom();
                None
            }
            KeyCode::Enter if is_press => self.selected_result().map(|item| {
                if self.download_mode {
                    SearchAction::DownloadUrl {
                        url: item.url.clone(),
                        title: item.title.clone(),
                    }
                } else {
                    SearchAction::CopyUrl(item.url.clone())
                }
            }),
            KeyCode::Char('y') if is_press => self
                .selected_result()
                .map(|item| SearchAction::CopyUrl(item.url.clone())),
            KeyCode::Char('/') if is_press => {
                self.editing_query = true;
                if self.query_input.is_empty()
                    && let Some(active) = &self.active_query
                {
                    self.query_input = active.clone();
                }
                self.cursor_pos = self.query_input.len();
                self.status_toast = None;
                None
            }
            KeyCode::Char('q') | KeyCode::Esc if is_press => Some(SearchAction::Quit),
            _ => None,
        }
    }

    fn handle_edit_key(&mut self, key: KeyEvent, is_press: bool) -> Option<SearchAction> {
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT)
            && matches!(key.code, KeyCode::Char('u') | KeyCode::Char('U'))
        {
            self.query_input.clear();
            self.cursor_pos = 0;
            return None;
        }

        if key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            && !is_windows_altgr_char(&key)
        {
            return None;
        }

        match key.code {
            KeyCode::Char(ch) => {
                let pos = clamp_char_boundary(&self.query_input, self.cursor_pos);
                self.query_input.insert(pos, ch);
                self.cursor_pos = pos + ch.len_utf8();
                None
            }
            KeyCode::Backspace => {
                let pos = clamp_char_boundary(&self.query_input, self.cursor_pos);
                if pos > 0 {
                    let prev = prev_char_boundary(&self.query_input, pos);
                    self.query_input.remove(prev);
                    self.cursor_pos = prev;
                }
                None
            }
            KeyCode::Delete => {
                let pos = clamp_char_boundary(&self.query_input, self.cursor_pos);
                if pos < self.query_input.len() {
                    self.query_input.remove(pos);
                    self.cursor_pos = pos;
                }
                None
            }
            KeyCode::Left => {
                let pos = clamp_char_boundary(&self.query_input, self.cursor_pos);
                self.cursor_pos = prev_char_boundary(&self.query_input, pos);
                None
            }
            KeyCode::Right => {
                let pos = clamp_char_boundary(&self.query_input, self.cursor_pos);
                self.cursor_pos = next_char_boundary(&self.query_input, pos);
                None
            }
            KeyCode::Home => {
                self.cursor_pos = 0;
                None
            }
            KeyCode::End => {
                self.cursor_pos = self.query_input.len();
                None
            }
            KeyCode::Enter if is_press => {
                let trimmed = self.query_input.trim().to_string();
                if trimmed.is_empty() {
                    self.status_toast = Some(("Enter a search query first".to_string(), true));
                    return None;
                }
                self.query_input = trimmed.clone();
                self.cursor_pos = self.query_input.len();
                self.editing_query = false;
                self.active_query = Some(trimmed.clone());
                self.results.clear();
                self.list_state.select(None);
                self.status_toast = None;
                self.phase = SearchPhase::Searching { tick: 0 };
                Some(SearchAction::StartSearch(trimmed))
            }
            KeyCode::Esc if is_press => {
                if self.active_query.is_some() || !self.results.is_empty() {
                    self.editing_query = false;
                    None
                } else {
                    Some(SearchAction::Quit)
                }
            }
            _ => None,
        }
    }

    /// Render the search interface into `frame`.
    pub fn render(&mut self, frame: &mut Frame) {
        self.render_with_ascii(frame, self.ascii);
    }

    fn render_with_ascii(&mut self, frame: &mut Frame, ascii: bool) {
        let area = frame.area();
        paint_background(frame, area, &self.theme);
        let [header_area, results_area, footer_area] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(3),
        ])
        .areas(area);

        self.render_header(frame, header_area, ascii);
        self.render_results(frame, results_area, ascii);
        self.render_footer(frame, footer_area, ascii);
    }

    fn render_header(&self, frame: &mut Frame, area: Rect, ascii: bool) {
        if area.is_empty() {
            return;
        }
        let block = Block::bordered()
            .border_type(border_type(ascii))
            .style(self.theme.base_style())
            .border_style(self.theme.border_style(self.editing_query));

        let inner_width = area.width.saturating_sub(2) as usize;
        let sep = if ascii { " | " } else { " · " };

        let provider_label = self.provider.display_name();
        let dl_badge = if self.download_mode {
            format!("{sep}DL:{}", self.format)
        } else {
            String::new()
        };
        let prefix_label = if area.width < 72 {
            format!(" Search (top {}){sep}{provider_label}{dl_badge}{sep}", self.limit)
        } else {
            format!(
                " Tunr Search (top {}){sep}{provider_label}{dl_badge}{sep}",
                self.limit
            )
        };
        let prefix_width = Span::raw(&prefix_label).width();
        let query_budget = inner_width.saturating_sub(prefix_width + 8);

        let mut spans = vec![Span::styled(
            prefix_label,
            self.theme.accent_bold_style(),
        )];

        if self.editing_query {
            spans.push(Span::styled("Query: ", self.theme.warning_bold_style()));
            let cursor = clamp_char_boundary(&self.query_input, self.cursor_pos);
            let before = &self.query_input[..cursor];
            let (cursor_ch, after) = if cursor < self.query_input.len() {
                let next = next_char_boundary(&self.query_input, cursor);
                (&self.query_input[cursor..next], &self.query_input[next..])
            } else {
                (" ", "")
            };
            let visible_before = tail_to_width(before, query_budget.saturating_sub(1));
            let remaining = query_budget
                .saturating_sub(Span::raw(&visible_before).width())
                .saturating_sub(Span::raw(cursor_ch).width());
            let visible_after = truncate_to_width(after, remaining);

            spans.push(Span::styled(visible_before, self.theme.emphasis_style()));
            spans.push(Span::styled(cursor_ch, self.theme.cursor_style()));
            spans.push(Span::styled(visible_after, self.theme.emphasis_style()));
        } else if let Some(active) = &self.active_query {
            let quoted = format!("\"{active}\"");
            let count_suffix = match &self.phase {
                SearchPhase::Results => format!(" ({})", self.results.len()),
                _ => String::new(),
            };
            let fitted = truncate_to_width(
                &quoted,
                query_budget.saturating_sub(Span::raw(&count_suffix).width()),
            );
            spans.push(Span::styled("Query: ", self.theme.secondary_style()));
            spans.push(Span::styled(fitted, self.theme.emphasis_bold_style()));
            if !count_suffix.is_empty() {
                spans.push(Span::styled(count_suffix, self.theme.playing_style()));
            }
        } else {
            spans.push(Span::styled(
                "Press / to search",
                self.theme.muted_style(),
            ));
        }

        frame.render_widget(
            Paragraph::new(Line::from(spans))
                .style(self.theme.base_style())
                .block(block),
            area,
        );
    }

    fn render_results(&mut self, frame: &mut Frame, area: Rect, ascii: bool) {
        if area.is_empty() {
            return;
        }
        let title_text = match &self.phase {
            SearchPhase::Results => format!(" Results ({}) ", self.results.len()),
            SearchPhase::Searching { .. } => " Searching ".to_string(),
            SearchPhase::Error(_) => " Search Error ".to_string(),
            SearchPhase::Idle => " Results ".to_string(),
        };
        let block = Block::bordered()
            .border_type(border_type(ascii))
            .style(self.theme.base_style())
            .border_style(self.theme.border_style(!self.editing_query))
            .title(Span::styled(title_text, self.theme.panel_title_style()));

        match &self.phase {
            SearchPhase::Idle => {
                let inner = block.inner(area);
                frame.render_widget(block, area);
                if !inner.is_empty() {
                    let prompt = Paragraph::new("Enter a search query above and press Enter.")
                        .style(self.theme.base_style().patch(self.theme.muted_style()))
                        .alignment(Alignment::Center)
                        .wrap(Wrap { trim: true });
                    frame.render_widget(prompt, inner);
                }
                return;
            }
            SearchPhase::Searching { tick } => {
                let spinner = active_spinner(*tick, ascii);
                let query = self.active_query.as_deref().unwrap_or("");
                let inner = block.inner(area);
                frame.render_widget(block, area);
                if !inner.is_empty() {
                    let text = Paragraph::new(format!(
                        "{spinner} Searching {} for \"{query}\"...",
                        self.provider.display_name()
                    ))
                    .style(self.theme.base_style().patch(self.theme.accent_bold_style()))
                    .alignment(Alignment::Center)
                    .wrap(Wrap { trim: true });
                    frame.render_widget(text, inner);
                }
                return;
            }
            SearchPhase::Error(err) => {
                let inner = block.inner(area);
                frame.render_widget(block, area);
                if !inner.is_empty() {
                    let msg = Paragraph::new(format!("Search failed: {err}\n\nPress / to try another query or q to quit."))
                        .style(self.theme.base_style().patch(self.theme.error_style()))
                        .alignment(Alignment::Center)
                        .wrap(Wrap { trim: true });
                    frame.render_widget(msg, inner);
                }
                return;
            }
            SearchPhase::Results if self.results.is_empty() => {
                self.list_state.select(None);
                let query = self.active_query.as_deref().unwrap_or("");
                let inner = block.inner(area);
                frame.render_widget(block, area);
                if !inner.is_empty() {
                    let empty = Paragraph::new(format!(
                        "No results found for \"{query}\".\nPress / to search again or q to quit."
                    ))
                    .style(self.theme.base_style().patch(self.theme.warning_style()))
                    .alignment(Alignment::Center)
                    .wrap(Wrap { trim: true });
                    frame.render_widget(empty, inner);
                }
                return;
            }
            SearchPhase::Results => {}
        }

        let inner_rows = area.height.saturating_sub(2) as usize;
        self.page_step = (inner_rows / 2).max(1);
        let inner_width = area.width.saturating_sub(4) as usize;
        let sep = if ascii { " | " } else { " · " };

        let list_items: Vec<ListItem> = self
            .results
            .iter()
            .map(|item| {
                let prefix = format!("{:>2}. ", item.index);
                let prefix_width = Span::raw(&prefix).width();
                let title = truncate_to_width(&item.title, inner_width.saturating_sub(prefix_width));

                let title_line = Line::from(vec![
                    Span::styled(prefix, self.theme.accent_bold_style()),
                    Span::styled(title, self.theme.emphasis_bold_style()),
                ]);

                let channel = item.channel.as_deref().unwrap_or("Unknown channel");
                let duration = if item.is_live {
                    "LIVE".to_string()
                } else {
                    item.duration_secs
                        .map(format_duration)
                        .unwrap_or_else(|| "--:--".to_string())
                };
                let mut detail_parts = vec![channel.to_string(), duration];
                if let Some(views) = item.view_count {
                    detail_parts.push(format_views(views));
                }
                detail_parts.push(item.url.clone());
                let detail = truncate_to_width(&detail_parts.join(sep), inner_width.saturating_sub(4));

                let detail_style = if item.is_live {
                    self.theme.live_style()
                } else {
                    self.theme.secondary_style()
                };
                let detail_line = Line::from(vec![
                    Span::raw("    "),
                    Span::styled(detail, detail_style),
                ]);

                ListItem::new(vec![title_line, detail_line])
            })
            .collect();

        let list = List::new(list_items)
            .style(self.theme.base_style())
            .block(block)
            .highlight_symbol("> ")
            .highlight_spacing(HighlightSpacing::Always)
            .highlight_style(passive_highlight_style(&self.theme));
        frame.render_stateful_widget(list, area, &mut self.list_state);
    }

    fn render_footer(&self, frame: &mut Frame, area: Rect, ascii: bool) {
        if area.is_empty() {
            return;
        }
        let block = Block::bordered()
            .border_type(border_type(ascii))
            .style(self.theme.base_style())
            .border_style(self.theme.border_style(false));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.is_empty() {
            return;
        }

        let hints: String = if self.editing_query {
            if inner.width < 70 {
                "Enter:Search  Ctrl+U:Clear  Esc:Cancel".to_string()
            } else {
                "Type query  Enter:Search  Left/Right:Move  Ctrl+U:Clear  Esc:Back/Quit"
                    .to_string()
            }
        } else {
            let nav = match (ascii, inner.width < 82) {
                (true, true) => "Up/Down/j/k:Scroll",
                (true, false) => "Up/Down/j/k/PgUp/PgDn/g/G:Scroll",
                (false, true) => "↑/↓/j/k:Scroll",
                (false, false) => "↑/↓/j/k/PgUp/PgDn/g/G:Scroll",
            };
            let action = if self.download_mode {
                format!("Enter:Download ({})  y:Copy", self.format)
            } else if inner.width < 82 {
                "Enter/y:Copy".to_string()
            } else {
                "Enter/y:Copy URL".to_string()
            };
            let tail = if inner.width < 82 {
                "/:Search  q:Quit"
            } else {
                "/:New Search  q/Esc:Quit"
            };
            format!("{nav}  {action}  {tail}")
        };

        if let Some((toast_msg, is_err)) = &self.status_toast {
            let toast_style = if *is_err {
                self.theme.error_bold_style()
            } else {
                self.theme.playing_bold_style()
            };
            let min_hints = 22usize;
            let total_w = inner.width as usize;
            let toast_w = Span::raw(toast_msg).width();
            if total_w > min_hints + 4 {
                let toast_alloc = toast_w.min(total_w - min_hints - 2) as u16;
                let [left_area, right_area] = Layout::horizontal([
                    Constraint::Min(min_hints as u16),
                    Constraint::Length(toast_alloc),
                ])
                .areas(inner);

                let left_text =
                    truncate_to_width(&hints, left_area.width.saturating_sub(1) as usize);
                let right_text = truncate_to_width(toast_msg, right_area.width as usize);
                frame.render_widget(
                    Paragraph::new(Span::styled(left_text, self.theme.secondary_style()))
                        .style(self.theme.base_style()),
                    left_area,
                );
                frame.render_widget(
                    Paragraph::new(Span::styled(right_text, toast_style))
                        .style(self.theme.base_style())
                        .alignment(Alignment::Right),
                    right_area,
                );
                return;
            }
            let right_text = truncate_to_width(toast_msg, total_w);
            frame.render_widget(
                Paragraph::new(Span::styled(right_text, toast_style))
                    .style(self.theme.base_style()),
                inner,
            );
            return;
        }

        let hint_text = truncate_to_width(&hints, inner.width as usize);
        frame.render_widget(
            Paragraph::new(Span::styled(hint_text, self.theme.secondary_style()))
                .style(self.theme.base_style()),
            inner,
        );
    }
}

fn format_views(views: u64) -> String {
    if views >= 1_000_000_000 {
        format!("{:.1}B views", views as f64 / 1_000_000_000.0)
    } else if views >= 1_000_000 {
        format!("{:.1}M views", views as f64 / 1_000_000.0)
    } else if views >= 1_000 {
        format!("{:.1}K views", views as f64 / 1_000.0)
    } else {
        format!("{views} views")
    }
}

fn is_windows_altgr_char(key: &KeyEvent) -> bool {
    #[cfg(windows)]
    {
        key.modifiers
            .contains(KeyModifiers::CONTROL | KeyModifiers::ALT)
            && matches!(key.code, KeyCode::Char(ch) if !ch.is_ascii_alphabetic())
    }
    #[cfg(not(windows))]
    {
        let _ = key;
        false
    }
}

fn clamp_char_boundary(s: &str, idx: usize) -> usize {
    let mut pos = idx.min(s.len());
    while pos > 0 && !s.is_char_boundary(pos) {
        pos -= 1;
    }
    pos
}

fn prev_char_boundary(s: &str, idx: usize) -> usize {
    let mut pos = clamp_char_boundary(s, idx);
    if pos == 0 {
        return 0;
    }
    pos -= 1;
    while pos > 0 && !s.is_char_boundary(pos) {
        pos -= 1;
    }
    pos
}

fn next_char_boundary(s: &str, idx: usize) -> usize {
    let pos = clamp_char_boundary(s, idx);
    if pos >= s.len() {
        return s.len();
    }
    let mut next = pos + 1;
    while next < s.len() && !s.is_char_boundary(next) {
        next += 1;
    }
    next
}

fn tail_to_width(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if Span::raw(text).width() <= max_width {
        return text.to_string();
    }
    let mut rev_chars = Vec::new();
    let mut used = 0;
    for ch in text.chars().rev() {
        let mut buf = [0u8; 4];
        let ch_str: &str = ch.encode_utf8(&mut buf);
        let ch_w = Span::raw(ch_str).width();
        if used + ch_w > max_width {
            break;
        }
        rev_chars.push(ch);
        used += ch_w;
    }
    rev_chars.into_iter().rev().collect()
}

fn spawn_search_task(
    query: String,
    limit: usize,
    provider: SearchProvider,
    request_id: u64,
) -> JoinHandle<(u64, Result<Vec<SearchResult>>)> {
    tokio::spawn(async move {
        let res = search::search_yt_dlp(&query, limit, provider).await;
        (request_id, res)
    })
}

fn build_search_download_options(
    url: String,
    cfg: &SearchRunConfig,
    no_tui: bool,
) -> DownloadOptions {
    DownloadOptions {
        urls: vec![url],
        targets: Vec::new(),
        dest: cfg.dest.clone(),
        set_dest: None,
        format: cfg.format,
        playlist: false,
        embed_thumbnail: true,
        auto_add: cfg.format.is_audio(),
        playlist_name: None,
        jobs: 1,
        no_tui,
    }
}

fn format_download_toast(title: &str, outcome: &DownloadOutcome) -> (String, bool) {
    if let Some(path) = outcome.downloaded_files.first() {
        let saved = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| title.to_string());
        (format!("Downloaded {saved}"), false)
    } else if let Some(failure) = outcome.failed.first() {
        (format!("Download failed: {}", failure.error), true)
    } else {
        (format!("Downloaded {title}"), false)
    }
}

/// Launch the interactive ratatui search TUI on the current terminal.
pub async fn run_search_tui(initial_query: Option<String>, cfg: SearchRunConfig) -> Result<()> {
    let mut state = SearchAppState::from_run_config(initial_query, &cfg);

    let mut session = TerminalSession::enter()?;
    let mut signals = ShutdownSignals::new()?;
    // Instantiate EventStream after TerminalSession so EventStream drops before raw mode restores.
    let mut events = EventStream::new();

    let mut tick = tokio::time::interval(TUI_TICK_INTERVAL);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

    let mut next_request_id: u64 = 0;
    let mut active_request_id: u64 = 0;
    let mut search_task: Option<JoinHandle<(u64, Result<Vec<SearchResult>>)>> = None;

    if let Some(initial) = state.active_query.clone() {
        next_request_id = next_request_id.wrapping_add(1);
        active_request_id = next_request_id;
        search_task = Some(spawn_search_task(
            initial,
            cfg.limit,
            cfg.provider,
            active_request_id,
        ));
    }

    let mut copy_task: Option<JoinHandle<(String, Result<ClipboardMethod>)>> = None;

    let run_res: Result<()> = loop {
        session.terminal_mut().draw(|frame| state.render(frame))?;

        tokio::select! {
            join_res = async {
                match search_task.as_mut() {
                    Some(handle) => handle.await,
                    None => std::future::pending().await,
                }
            } => {
                search_task = None;
                match join_res {
                    Ok((req_id, res)) if req_id == active_request_id => {
                        state.apply_search_result(res);
                    }
                    Ok(_) => {}
                    Err(err) if !err.is_cancelled() => {
                        state.apply_search_result(Err(TunrError::Search(
                            format!("search task panicked: {err}"),
                        )));
                    }
                    Err(_) => {}
                }
            }
            copy_join = async {
                match copy_task.as_mut() {
                    Some(handle) => handle.await,
                    None => std::future::pending().await,
                }
            } => {
                copy_task = None;
                match copy_join {
                    Ok((url, res)) => state.apply_copy_result(&url, res),
                    Err(err) if !err.is_cancelled() => {
                        state.status_toast = Some((format!("Copy task failed: {err}"), true));
                    }
                    Err(_) => {}
                }
            }
            _ = tick.tick() => {
                state.tick();
            }
            maybe_ev = events.next() => {
                match maybe_ev {
                    Some(Ok(Event::Key(key))) => {
                        if let Some(action) = state.handle_key(key) {
                            match action {
                                SearchAction::StartSearch(query) => {
                                    if let Some(prev) = search_task.take() {
                                        prev.abort();
                                        let _ = prev.await;
                                    }
                                    next_request_id = next_request_id.wrapping_add(1);
                                    active_request_id = next_request_id;
                                    search_task = Some(spawn_search_task(
                                        query,
                                        cfg.limit,
                                        cfg.provider,
                                        active_request_id,
                                    ));
                                }
                                SearchAction::CopyUrl(url) => {
                                    if let Some(prev) = copy_task.take() {
                                        prev.abort();
                                        let _ = prev.await;
                                    }
                                    copy_task = Some(tokio::spawn(async move {
                                        let res = copy_to_clipboard(&url, true).await;
                                        (url, res)
                                    }));
                                }
                                SearchAction::DownloadUrl { url, title } => {
                                    if let Some(prev) = copy_task.take() {
                                        prev.abort();
                                        let _ = prev.await;
                                    }
                                    let paths = match cfg.paths.clone() {
                                        Some(p) => p,
                                        None => match tokio::task::spawn_blocking(AppPaths::resolve).await {
                                            Ok(Ok(p)) => p,
                                            Ok(Err(err)) => {
                                                state.status_toast = Some((format!("Download failed: {err}"), true));
                                                continue;
                                            }
                                            Err(err) => {
                                                state.status_toast = Some((format!("Download failed: {err}"), true));
                                                continue;
                                            }
                                        },
                                    };
                                    let dl_opts = build_search_download_options(url, &cfg, false);
                                    match download_tui::run_download_tui_on_session(
                                        &mut session,
                                        &mut events,
                                        &mut signals,
                                        &paths,
                                        &dl_opts,
                                        &state.theme,
                                    )
                                    .await
                                    {
                                        Ok(DownloadTuiOutcome::Completed(outcome)) => {
                                            state.status_toast = Some(format_download_toast(&title, &outcome));
                                        }
                                        Ok(DownloadTuiOutcome::Cancelled) => {
                                            state.status_toast = Some(("Download cancelled".to_string(), true));
                                        }
                                        Err(err) => {
                                            if matches!(
                                                &err,
                                                TunrError::Download(msg) if msg.starts_with("download interrupted")
                                            ) {
                                                break Err(err);
                                            }
                                            state.status_toast = Some((format!("Download failed: {err}"), true));
                                        }
                                    }
                                }
                                SearchAction::Quit => break Ok(()),
                            }
                        }
                    }
                    Some(Ok(Event::Resize(_, _))) | Some(Ok(_)) => {}
                    Some(Err(err)) => break Err(err.into()),
                    None => break Ok(()),
                }
            }
            _ = signals.recv() => {
                break Ok(());
            }
        }
    };

    if let Some(handle) = search_task.take() {
        handle.abort();
        let _ = handle.await;
    }
    if let Some(handle) = copy_task.take() {
        handle.abort();
        let _ = handle.await;
    }
    drop(events);
    drop(session);

    run_res
}

/// Run a yt-dlp search and print numbered results and URLs to stdout without a TUI.
///
/// When `cfg.download_mode` is `true`, downloads the top result (`#1`) using line progress.
pub async fn run_search_plain(query: &str, cfg: SearchRunConfig) -> Result<()> {
    let results = search::search_yt_dlp(query, cfg.limit, cfg.provider).await?;
    if results.is_empty() {
        println!("No results found for \"{}\".", query.trim());
        return Ok(());
    }

    for item in &results {
        let channel = item.channel.as_deref().unwrap_or("Unknown channel");
        let dur = if item.is_live {
            "LIVE".to_string()
        } else {
            item.duration_secs
                .map(format_duration)
                .unwrap_or_else(|| "--:--".to_string())
        };
        let views = item
            .view_count
            .map(|v| format!(" · {}", format_views(v)))
            .unwrap_or_default();
        println!("{}. {} ({channel} · {dur}{views})", item.index, item.title);
        println!("   {}", item.url);
    }

    if cfg.download_mode {
        let top = &results[0];
        println!(
            "Downloading #1: {} ({}) as {}...",
            top.title, top.url, cfg.format
        );
        let paths = match cfg.paths.clone() {
            Some(p) => p,
            None => tokio::task::spawn_blocking(AppPaths::resolve)
                .await
                .map_err(|err| TunrError::Async(err.to_string()))??,
        };
        let dl_opts = build_search_download_options(top.url.clone(), &cfg, true);
        let outcome = download_tui::run_download_with_line_progress(&paths, &dl_opts).await?;
        for path in &outcome.downloaded_files {
            println!("Downloaded: {}", path.display());
        }
        for failure in &outcome.failed {
            eprintln!("Download failed ({}): {}", failure.url, failure.error);
        }
        if !outcome.failed.is_empty() && outcome.downloaded_files.is_empty() {
            return Err(TunrError::Download(outcome.failed[0].error.clone()));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::Color;

    use crate::error::TunrError;

    use super::*;

    fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
        let buf = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    fn sample_results(count: usize) -> Vec<SearchResult> {
        (1..=count)
            .map(|idx| SearchResult {
                index: idx,
                id: format!("id{idx}"),
                title: if idx == 2 {
                    "Live Broadcast Stream".to_string()
                } else {
                    format!("Sample Track {idx}")
                },
                url: format!("https://www.youtube.com/watch?v=id{idx}"),
                channel: Some(format!("Artist {idx}")),
                duration_secs: if idx == 2 { None } else { Some(180 + idx as u64) },
                view_count: Some(1_500_000 * idx as u64),
                is_live: idx == 2,
            })
            .collect()
    }

    fn key(code: KeyCode, modifiers: KeyModifiers, kind: KeyEventKind) -> KeyEvent {
        KeyEvent::new_with_kind(code, modifiers, kind)
    }

    #[test]
    fn renders_searching_results_toast_and_editing_states() {
        let mut state = SearchAppState::with_ascii(Some("lofi beats".to_string()), 10, false);
        state.tick();

        let mut terminal = Terminal::new(TestBackend::new(100, 16)).unwrap();
        terminal.draw(|f| state.render(f)).unwrap();
        let searching = buffer_text(&terminal);
        assert!(searching.contains("Tunr Search (top 10)"));
        assert!(searching.contains("\"lofi beats\""));
        assert!(searching.contains("Searching YouTube"));

        state.apply_search_result(Ok(sample_results(3)));
        state.apply_copy_result(
            "https://www.youtube.com/watch?v=id1",
            Ok(ClipboardMethod::WlCopy),
        );
        terminal.draw(|f| state.render(f)).unwrap();
        let rendered = buffer_text(&terminal);
        assert!(rendered.contains("Results (3)"));
        assert!(rendered.contains("1. Sample Track 1"));
        assert!(rendered.contains("Artist 1"));
        assert!(rendered.contains("03:01"));
        assert!(rendered.contains("1.5M views"));
        assert!(rendered.contains("Live Broadcast Stream"));
        assert!(rendered.contains("LIVE"));
        assert!(rendered.contains("Copied via wl-copy"));

        // Enter edit mode via '/'
        assert_eq!(
            state.handle_key(key(KeyCode::Char('/'), KeyModifiers::NONE, KeyEventKind::Press)),
            None
        );
        terminal.draw(|f| state.render(f)).unwrap();
        let editing = buffer_text(&terminal);
        assert!(editing.contains("Query: lofi beats"));
        assert!(editing.contains("Ctrl+U:Clear"));
    }

    #[test]
    fn narrow_terminal_and_ascii_mode_render_cleanly() {
        let mut state = SearchAppState::with_ascii(Some("日本語検索".to_string()), 5, true);
        state.apply_search_result(Ok(vec![SearchResult {
            index: 1,
            id: "abc".to_string(),
            title: "日本語のとても長いタイトルの楽曲テスト".to_string(),
            url: "https://www.youtube.com/watch?v=abc".to_string(),
            channel: Some("チャンネル名".to_string()),
            duration_secs: Some(3661),
            view_count: Some(950),
            is_live: false,
        }]));
        state.apply_copy_result(
            "https://www.youtube.com/watch?v=abc",
            Err(TunrError::NotFound("no clipboard tool".to_string())),
        );

        let mut terminal = Terminal::new(TestBackend::new(46, 12)).unwrap();
        terminal.draw(|f| state.render(f)).unwrap();
        let rendered = buffer_text(&terminal);
        assert!(rendered.contains("Search (top 5)"));
        assert!(rendered.contains("01:01:01"));
        assert!(rendered.contains("Copy failed:"));
        assert!(!rendered.contains('╭'));
        assert!(!rendered.contains('·'));
    }

    #[test]
    fn scrolling_navigation_bounds_and_actions_behave_cleanly() {
        let mut state = SearchAppState::with_ascii(Some("jazz".to_string()), 15, true);
        state.apply_search_result(Ok(sample_results(12)));
        state.page_step = 4;

        assert_eq!(state.list_state.selected(), Some(0));
        state.handle_key(key(KeyCode::Down, KeyModifiers::NONE, KeyEventKind::Press));
        assert_eq!(state.list_state.selected(), Some(1));
        state.handle_key(key(KeyCode::PageDown, KeyModifiers::NONE, KeyEventKind::Repeat));
        assert_eq!(state.list_state.selected(), Some(5));
        state.handle_key(key(KeyCode::PageUp, KeyModifiers::NONE, KeyEventKind::Press));
        assert_eq!(state.list_state.selected(), Some(1));
        state.handle_key(key(KeyCode::Char('G'), KeyModifiers::SHIFT, KeyEventKind::Press));
        assert_eq!(state.list_state.selected(), Some(11));
        state.handle_key(key(KeyCode::Char('g'), KeyModifiers::NONE, KeyEventKind::Press));
        assert_eq!(state.list_state.selected(), Some(0));

        // Repeat on Enter/y/q does not trigger action
        assert_eq!(
            state.handle_key(key(KeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Repeat)),
            None
        );
        assert_eq!(
            state.handle_key(key(KeyCode::Char('y'), KeyModifiers::NONE, KeyEventKind::Repeat)),
            None
        );
        assert_eq!(
            state.handle_key(key(KeyCode::Char('q'), KeyModifiers::NONE, KeyEventKind::Repeat)),
            None
        );

        // Press on Enter and y triggers CopyUrl without quitting
        assert_eq!(
            state.handle_key(key(KeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Press)),
            Some(SearchAction::CopyUrl(
                "https://www.youtube.com/watch?v=id1".to_string()
            ))
        );
        assert_eq!(
            state.handle_key(key(KeyCode::Char('y'), KeyModifiers::NONE, KeyEventKind::Press)),
            Some(SearchAction::CopyUrl(
                "https://www.youtube.com/watch?v=id1".to_string()
            ))
        );

        // Press on q quits
        assert_eq!(
            state.handle_key(key(KeyCode::Char('q'), KeyModifiers::NONE, KeyEventKind::Press)),
            Some(SearchAction::Quit)
        );
    }

    #[test]
    fn utf8_cursor_editing_and_escape_rules_work_safely() {
        let mut state = SearchAppState::with_ascii(None, 10, true);
        assert!(state.editing_query);
        assert_eq!(state.phase, SearchPhase::Idle);

        // Empty enter shows toast instead of starting search
        assert_eq!(
            state.handle_key(key(KeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Press)),
            None
        );
        assert!(state.status_toast.as_ref().is_some_and(|(_, err)| *err));

        // Insert multi-byte UTF-8 characters and edit in middle
        for ch in ['音', '楽', 'a'] {
            state.handle_key(key(KeyCode::Char(ch), KeyModifiers::NONE, KeyEventKind::Press));
        }
        assert_eq!(state.query_input, "音楽a");
        assert_eq!(state.cursor_pos, "音楽a".len());

        state.handle_key(key(KeyCode::Left, KeyModifiers::NONE, KeyEventKind::Press));
        state.handle_key(key(KeyCode::Left, KeyModifiers::NONE, KeyEventKind::Press));
        state.handle_key(key(KeyCode::Char('X'), KeyModifiers::NONE, KeyEventKind::Press));
        assert_eq!(state.query_input, "音X楽a");

        state.handle_key(key(KeyCode::Backspace, KeyModifiers::NONE, KeyEventKind::Press));
        assert_eq!(state.query_input, "音楽a");

        state.handle_key(key(KeyCode::Delete, KeyModifiers::NONE, KeyEventKind::Press));
        assert_eq!(state.query_input, "音a");

        state.handle_key(key(KeyCode::Home, KeyModifiers::NONE, KeyEventKind::Press));
        assert_eq!(state.cursor_pos, 0);
        state.handle_key(key(KeyCode::End, KeyModifiers::NONE, KeyEventKind::Press));
        assert_eq!(state.cursor_pos, state.query_input.len());

        // Submit search
        assert_eq!(
            state.handle_key(key(KeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Press)),
            Some(SearchAction::StartSearch("音a".to_string()))
        );
        assert!(!state.editing_query);

        // Re-enter edit mode and Esc returns to browse mode instead of quitting
        state.handle_key(key(KeyCode::Char('/'), KeyModifiers::NONE, KeyEventKind::Press));
        assert!(state.editing_query);
        assert_eq!(
            state.handle_key(key(KeyCode::Esc, KeyModifiers::NONE, KeyEventKind::Press)),
            None
        );
        assert!(!state.editing_query);

        // Ctrl+U clears input in edit mode
        state.handle_key(key(KeyCode::Char('/'), KeyModifiers::NONE, KeyEventKind::Press));
        state.handle_key(key(
            KeyCode::Char('u'),
            KeyModifiers::CONTROL,
            KeyEventKind::Press,
        ));
        assert_eq!(state.query_input, "");
        assert_eq!(state.cursor_pos, 0);

        // Ctrl+Alt+C is ignored (AltGr guard), Ctrl+C Press quits
        assert_eq!(
            state.handle_key(key(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
                KeyEventKind::Press,
            )),
            None
        );
        assert_eq!(
            state.handle_key(key(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL,
                KeyEventKind::Press,
            )),
            Some(SearchAction::Quit)
        );
    }

    #[test]
    fn download_mode_renders_provider_format_and_triggers_download_or_copy() {
        let mut state = SearchAppState::with_ascii_and_config(
            Some("chill beats".to_string()),
            10,
            SearchProvider::SoundCloud,
            true,
            DownloadFormat::Mp4,
            false,
        );

        let mut terminal = Terminal::new(TestBackend::new(110, 14)).unwrap();
        terminal.draw(|f| state.render(f)).unwrap();
        let searching = buffer_text(&terminal);
        assert!(searching.contains("Tunr Search (top 10)"));
        assert!(searching.contains("SoundCloud"));
        assert!(searching.contains("DL:mp4"));
        assert!(searching.contains("Searching SoundCloud"));

        state.apply_search_result(Ok(sample_results(2)));
        terminal.draw(|f| state.render(f)).unwrap();
        let results_view = buffer_text(&terminal);
        assert!(results_view.contains("Enter:Download (mp4)"));
        assert!(results_view.contains("y:Copy"));

        // Enter emits DownloadUrl while 'y' continues to emit CopyUrl
        assert_eq!(
            state.handle_key(key(KeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Press)),
            Some(SearchAction::DownloadUrl {
                url: "https://www.youtube.com/watch?v=id1".to_string(),
                title: "Sample Track 1".to_string(),
            })
        );
        assert_eq!(
            state.handle_key(key(KeyCode::Char('y'), KeyModifiers::NONE, KeyEventKind::Press)),
            Some(SearchAction::CopyUrl(
                "https://www.youtube.com/watch?v=id1".to_string()
            ))
        );

        // Repeat on Enter/y does not fire in download mode either
        assert_eq!(
            state.handle_key(key(KeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Repeat)),
            None
        );

        // Verify download options disable auto_add for non-audio mp4 and enable it for mp3
        let cfg_mp4 = SearchRunConfig {
            limit: 10,
            provider: SearchProvider::SoundCloud,
            download_mode: true,
            format: DownloadFormat::Mp4,
            paths: None,
            dest: Some(PathBuf::from("/tmp/music")),
            theme: Theme::default(),
            theme_warning: None,
        };
        let dl_mp4 = build_search_download_options("https://example.com/1".to_string(), &cfg_mp4, false);
        assert!(!dl_mp4.auto_add);
        assert_eq!(dl_mp4.format, DownloadFormat::Mp4);

        let mut cfg_mp3 = cfg_mp4.clone();
        cfg_mp3.format = DownloadFormat::Mp3;
        let dl_mp3 = build_search_download_options("https://example.com/1".to_string(), &cfg_mp3, true);
        assert!(dl_mp3.auto_add);
        assert!(dl_mp3.no_tui);

        // Toast formatting summarizes saved filename or first failure
        let ok_outcome = DownloadOutcome {
            downloaded_files: vec![PathBuf::from("/tmp/music/track.mp4")],
            ..Default::default()
        };
        assert_eq!(
            format_download_toast("Track", &ok_outcome),
            ("Downloaded track.mp4".to_string(), false)
        );
        let err_outcome = DownloadOutcome {
            failed: vec![crate::download::DownloadFailure {
                url: "https://example.com/1".to_string(),
                error: "network error".to_string(),
            }],
            ..Default::default()
        };
        assert_eq!(
            format_download_toast("Track", &err_outcome),
            ("Download failed: network error".to_string(), true)
        );
    }

    fn find_text_pos(terminal: &Terminal<TestBackend>, needle: &str) -> (u16, u16) {
        let buf = terminal.backend().buffer();
        for y in 0..buf.area.height {
            let mut row = String::new();
            for x in 0..buf.area.width {
                row.push_str(buf[(x, y)].symbol());
            }
            if let Some(byte_idx) = row.find(needle) {
                let col = row[..byte_idx].chars().count() as u16;
                return (col, y);
            }
        }
        panic!("text {needle:?} not found in buffer:\n{}", buffer_text(terminal));
    }

    #[test]
    fn tunr_dark_theme_preserves_search_styles() {
        let mut state = SearchAppState::with_ascii(Some("lofi".to_string()), 10, false);
        state.apply_search_result(Ok(sample_results(2)));
        state.apply_copy_result(
            "https://www.youtube.com/watch?v=id1",
            Ok(ClipboardMethod::WlCopy),
        );

        let mut terminal = Terminal::new(TestBackend::new(100, 14)).unwrap();
        terminal.draw(|f| state.render(f)).unwrap();

        let buf = terminal.backend().buffer();
        // Frame background remains terminal Reset for tunr-dark
        assert_eq!(buf[(95, 1)].bg, Color::Reset);
        // Active results border is Cyan, inactive header border is DarkGray
        assert_eq!(buf[(0, 0)].fg, Color::DarkGray);
        assert_eq!(buf[(0, 3)].fg, Color::Cyan);
        // Selected first row uses DarkGray bg and preserves Cyan index + White title
        let (idx_x, idx_y) = find_text_pos(&terminal, "1.");
        assert_eq!(buf[(idx_x, idx_y)].fg, Color::Cyan);
        assert_eq!(buf[(idx_x, idx_y)].bg, Color::DarkGray);
        let (title_x, title_y) = find_text_pos(&terminal, "Sample Track 1");
        assert_eq!(buf[(title_x, title_y)].fg, Color::White);
        assert_eq!(buf[(title_x, title_y)].bg, Color::DarkGray);
        // LIVE stream row detail uses LightRed
        let (live_x, live_y) = find_text_pos(&terminal, "LIVE");
        assert_eq!(buf[(live_x, live_y)].fg, Color::LightRed);
        // Success toast in footer uses Green
        let (toast_x, toast_y) = find_text_pos(&terminal, "Copied via wl-copy");
        assert_eq!(buf[(toast_x, toast_y)].fg, Color::Green);

        // Edit mode uses Yellow "Query: " and Black-on-Cyan cursor
        state.handle_key(key(KeyCode::Char('/'), KeyModifiers::NONE, KeyEventKind::Press));
        terminal.draw(|f| state.render(f)).unwrap();
        let buf = terminal.backend().buffer();
        assert_eq!(buf[(0, 0)].fg, Color::Cyan);
        let (q_x, q_y) = find_text_pos(&terminal, "Query: ");
        assert_eq!(buf[(q_x, q_y)].fg, Color::Yellow);
        let (lofi_x, lofi_y) = find_text_pos(&terminal, "lofi");
        let cursor_cell = &buf[(lofi_x + 4, lofi_y)];
        assert_eq!(cursor_cell.fg, Color::Black);
        assert_eq!(cursor_cell.bg, Color::Cyan);
    }

    #[test]
    fn tunr_light_theme_paints_explicit_bg_without_bleed_through() {
        let light = Theme::tunr_light();
        let light_bg = light.bg.to_color();
        assert_eq!(light_bg, Color::Rgb(246, 248, 250));

        let mut terminal = Terminal::new(TestBackend::new(90, 14)).unwrap();

        // Idle phase
        let mut state = SearchAppState::with_ascii(None, 10, false).with_theme(light.clone());
        terminal.draw(|f| state.render(f)).unwrap();
        for y in 0..14u16 {
            for x in 0..90u16 {
                let cell = &terminal.backend().buffer()[(x, y)];
                assert_ne!(
                    cell.bg,
                    Color::Reset,
                    "dark terminal bleed-through at ({x}, {y}) in Idle phase"
                );
            }
        }
        assert_eq!(terminal.backend().buffer()[(88, 1)].bg, light_bg);
        assert_eq!(terminal.backend().buffer()[(45, 5)].bg, light_bg);

        // Searching phase
        state.handle_key(key(KeyCode::Char('a'), KeyModifiers::NONE, KeyEventKind::Press));
        state.handle_key(key(KeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Press));
        terminal.draw(|f| state.render(f)).unwrap();
        assert_eq!(terminal.backend().buffer()[(45, 6)].bg, light_bg);

        // Error phase
        state.apply_search_result(Err(TunrError::Search("boom".to_string())));
        terminal.draw(|f| state.render(f)).unwrap();
        assert_eq!(terminal.backend().buffer()[(45, 6)].bg, light_bg);
        let (err_x, err_y) = find_text_pos(&terminal, "Search failed: search error: boom");
        assert_eq!(terminal.backend().buffer()[(err_x, err_y)].fg, light.error.to_color());
        assert_eq!(terminal.backend().buffer()[(err_x, err_y)].bg, light_bg);

        // Results phase (including selected row & LIVE row)
        state.apply_search_result(Ok(sample_results(2)));
        terminal.draw(|f| state.render(f)).unwrap();
        let buf = terminal.backend().buffer();
        for y in 0..14u16 {
            for x in 0..90u16 {
                assert_ne!(
                    buf[(x, y)].bg,
                    Color::Reset,
                    "dark terminal bleed-through at ({x}, {y}) in Results phase"
                );
            }
        }
        let (title_x, title_y) = find_text_pos(&terminal, "Sample Track 1");
        assert_eq!(buf[(title_x, title_y)].fg, light.fg_emphasis.to_color());
        assert_eq!(buf[(title_x, title_y)].bg, light.selection_bg.to_color());
        let (live_x, live_y) = find_text_pos(&terminal, "LIVE");
        assert_eq!(buf[(live_x, live_y)].fg, light.live.to_color());
        assert_eq!(buf[(live_x, live_y)].bg, light_bg);
    }
}
