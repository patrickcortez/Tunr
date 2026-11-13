use std::borrow::Cow;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Gauge, HighlightSpacing, List, ListItem, ListState, Paragraph,
};
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;

use crate::download::{
    self, DownloadFormat, DownloadOptions, DownloadOutcome, DownloadPhase, DownloadProgressEvent,
    StepTransition,
};
use crate::error::{Result, TunrError};
use crate::link::{DownloadTarget, LinkStatement};
use crate::paths::AppPaths;
use crate::platform::ShutdownSignals;
use crate::theme::Theme;
use crate::tui_common::{
    SPINNER_FRAMES, TerminalSession, active_spinner, border_type, format_duration,
    paint_background, passive_highlight_style, truncate_to_width, use_ascii_glyphs,
};

const TUI_TICK_INTERVAL: Duration = Duration::from_millis(80);

#[derive(Debug, Clone, PartialEq, Eq)]
struct DownloadItemState {
    url: String,
    title: Option<String>,
    phase: DownloadPhase,
    step: usize,
    total_steps: usize,
    transition: Option<StepTransition>,
}

impl DownloadItemState {
    fn new(url: String) -> Self {
        Self::with_steps(url, 1)
    }

    fn with_steps(url: String, total_steps: usize) -> Self {
        Self {
            url,
            title: None,
            phase: DownloadPhase::Queued,
            step: 0,
            total_steps: total_steps.max(1),
            transition: None,
        }
    }

    fn display_label(&self) -> Cow<'_, str> {
        let base = self.title.as_deref().unwrap_or(&self.url);
        if self.total_steps > 1 && self.step >= 1 {
            let tag = match self.transition {
                Some(StepTransition::Fallback) => " fallback",
                Some(StepTransition::Then) => " then",
                Some(StepTransition::Primary) | None => "",
            };
            Cow::Owned(format!(
                "[step {}/{}{}] {}",
                self.step, self.total_steps, tag, base
            ))
        } else {
            Cow::Borrowed(base)
        }
    }

    fn is_active(&self) -> bool {
        matches!(
            self.phase,
            DownloadPhase::Starting
                | DownloadPhase::Downloading { .. }
                | DownloadPhase::PostProcessing { .. }
                | DownloadPhase::Finalizing
        )
    }

    fn is_done(&self) -> bool {
        matches!(self.phase, DownloadPhase::Completed { .. })
    }

    fn is_failed(&self) -> bool {
        matches!(self.phase, DownloadPhase::Failed { .. })
    }

    fn fraction(&self) -> f64 {
        match &self.phase {
            DownloadPhase::Queued => 0.0,
            DownloadPhase::Starting => 0.03,
            DownloadPhase::Downloading {
                downloaded_bytes,
                total_bytes,
                ..
            } => match (downloaded_bytes, total_bytes) {
                (Some(dl), Some(total)) if *total > 0 => {
                    ((*dl as f64) / (*total as f64)).clamp(0.0, 0.92)
                }
                _ => 0.25,
            },
            DownloadPhase::PostProcessing { .. } => 0.94,
            DownloadPhase::Finalizing => 0.98,
            DownloadPhase::Completed { .. } | DownloadPhase::Failed { .. } => 1.0,
        }
    }
}

/// Pure renderable state machine for the interactive download loading TUI.
#[derive(Debug, Clone)]
pub struct DownloadTuiState {
    items: Vec<DownloadItemState>,
    list_state: ListState,
    dest_dir: Option<PathBuf>,
    format: DownloadFormat,
    jobs: usize,
    spinner_idx: usize,
    started_at: Instant,
    elapsed_override: Option<Duration>,
    indexing: bool,
    indexing_count: usize,
    finished: bool,
    notice: Option<String>,
    theme: Theme,
}

impl DownloadTuiState {
    /// Initialize state from validated download options before background workers begin.
    pub fn from_options(opts: &DownloadOptions) -> Self {
        if let Ok(plan) = download::resolve_download_plan(opts) {
            return Self::from_plan(&plan, opts.format, opts.jobs);
        }
        let urls = if !opts.targets.is_empty() {
            opts.targets
                .iter()
                .map(|target| match target {
                    DownloadTarget::Url(u) => u.clone(),
                    DownloadTarget::LinkFile(p) => p.display().to_string(),
                })
                .collect()
        } else {
            download::normalize_urls(&opts.urls).unwrap_or_else(|_| opts.urls.clone())
        };
        Self::new(urls, opts.format, opts.jobs.max(1))
    }

    /// Construct a download UI state from a resolved `.link` / CLI download plan.
    pub fn from_plan(plan: &[LinkStatement], format: DownloadFormat, jobs: usize) -> Self {
        let items = plan
            .iter()
            .map(|stmt| {
                DownloadItemState::with_steps(stmt.expr.to_string(), stmt.expr.leaf_count())
            })
            .collect();
        Self::from_items(items, format, jobs)
    }

    /// Construct a download UI state from explicit normalized URLs, format, and concurrency.
    pub fn new(urls: Vec<String>, format: DownloadFormat, jobs: usize) -> Self {
        let items: Vec<DownloadItemState> = urls.into_iter().map(DownloadItemState::new).collect();
        Self::from_items(items, format, jobs)
    }

    fn from_items(items: Vec<DownloadItemState>, format: DownloadFormat, jobs: usize) -> Self {
        let mut list_state = ListState::default();
        if !items.is_empty() {
            list_state.select(Some(0));
        }
        Self {
            items,
            list_state,
            dest_dir: None,
            format,
            jobs: jobs.max(1),
            spinner_idx: 0,
            started_at: Instant::now(),
            elapsed_override: None,
            indexing: false,
            indexing_count: 0,
            finished: false,
            notice: None,
            theme: Theme::default(),
        }
    }

    /// Apply a [`Theme`] to the download TUI state.
    pub fn with_theme(mut self, theme: Theme) -> Self {
        self.theme = theme;
        self
    }

    /// Return the active [`Theme`].
    pub fn theme(&self) -> &Theme {
        &self.theme
    }

    /// Advance the spinner frame.
    pub fn tick(&mut self) {
        self.spinner_idx = (self.spinner_idx + 1) % SPINNER_FRAMES.len();
    }

    /// Move selection up in the downloads list.
    pub fn move_up(&mut self) {
        if self.items.is_empty() {
            self.list_state.select(None);
            return;
        }
        let next = self.list_state.selected().unwrap_or(0).saturating_sub(1);
        self.list_state.select(Some(next));
    }

    /// Move selection down in the downloads list.
    pub fn move_down(&mut self) {
        if self.items.is_empty() {
            self.list_state.select(None);
            return;
        }
        let max_idx = self.items.len() - 1;
        let next = self
            .list_state
            .selected()
            .unwrap_or(0)
            .saturating_add(1)
            .min(max_idx);
        self.list_state.select(Some(next));
    }

    /// Apply a streaming [`DownloadProgressEvent`] to update the UI model.
    pub fn apply_event(&mut self, event: DownloadProgressEvent) {
        match event {
            DownloadProgressEvent::BatchStarted {
                urls,
                dest_dir,
                format,
                jobs,
            } => {
                if self.items.len() != urls.len() {
                    self.items = urls.into_iter().map(DownloadItemState::new).collect();
                    if self.items.is_empty() {
                        self.list_state.select(None);
                    } else {
                        let clamped = self
                            .list_state
                            .selected()
                            .unwrap_or(0)
                            .min(self.items.len() - 1);
                        self.list_state.select(Some(clamped));
                    }
                }
                self.dest_dir = Some(dest_dir);
                self.format = format;
                self.jobs = jobs.max(1);
            }
            DownloadProgressEvent::Notice(msg) => {
                self.notice = Some(msg);
            }
            DownloadProgressEvent::ItemStepStarted {
                index,
                url,
                step,
                total_steps,
                transition,
            } => {
                if let Some(item) = self.items.get_mut(index) {
                    item.url = url;
                    item.title = None;
                    item.step = step;
                    item.total_steps = total_steps.max(1);
                    item.transition = Some(transition);
                    item.phase = DownloadPhase::Starting;
                }
            }
            DownloadProgressEvent::ItemUpdate {
                index,
                title,
                phase,
            } => {
                if let Some(item) = self.items.get_mut(index) {
                    if let Some(t) = title.filter(|t| !t.trim().is_empty()) {
                        item.title = Some(t);
                    }
                    item.phase = phase;
                }
            }
            DownloadProgressEvent::IndexingStarted { track_count } => {
                self.indexing = true;
                self.indexing_count = track_count;
            }
            DownloadProgressEvent::BatchFinished => {
                self.indexing = false;
                self.finished = true;
            }
        }
    }

    fn elapsed(&self) -> Duration {
        self.elapsed_override
            .unwrap_or_else(|| self.started_at.elapsed())
    }

    fn summary_counts(&self) -> (usize, usize, usize, usize) {
        let mut done = 0usize;
        let mut failed = 0usize;
        let mut active = 0usize;
        let mut queued = 0usize;
        for item in &self.items {
            if item.is_done() {
                done += 1;
            } else if item.is_failed() {
                failed += 1;
            } else if item.is_active() {
                active += 1;
            } else {
                queued += 1;
            }
        }
        (done, active, queued, failed)
    }

    fn overall_ratio(&self) -> f64 {
        if self.items.is_empty() {
            return if self.finished { 1.0 } else { 0.0 };
        }
        if self.finished {
            return 1.0;
        }
        let sum: f64 = self.items.iter().map(DownloadItemState::fraction).sum();
        (sum / self.items.len() as f64).clamp(0.0, 0.99)
    }

    /// Render the loading interface into `frame`.
    pub fn render(&mut self, frame: &mut Frame) {
        self.render_with_ascii(frame, use_ascii_glyphs());
    }

    fn render_with_ascii(&mut self, frame: &mut Frame, ascii: bool) {
        let area = frame.area();
        paint_background(frame, area, &self.theme);
        let [header_area, overall_area, list_area, footer_area] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(3),
        ])
        .areas(area);

        self.render_header(frame, header_area, ascii);
        self.render_overall(frame, overall_area, ascii);
        self.render_items(frame, list_area, ascii);
        self.render_footer(frame, footer_area, ascii);
    }

    fn render_header(&self, frame: &mut Frame, area: Rect, ascii: bool) {
        if area.is_empty() {
            return;
        }
        let spinner = active_spinner(self.spinner_idx, ascii);
        let (done, active, _queued, failed) = self.summary_counts();
        let (state_label, state_style) = if self.finished {
            if failed > 0 {
                (
                    "Finished (with errors)".to_string(),
                    self.theme.warning_bold_style(),
                )
            } else {
                (
                    "Complete".to_string(),
                    self.theme.playing_bold_style(),
                )
            }
        } else if self.indexing {
            (
                format!("{spinner} Indexing {} track(s)...", self.indexing_count),
                self.theme.accent_bold_style(),
            )
        } else if active > 0 || done > 0 || failed > 0 {
            (
                format!("{spinner} Downloading..."),
                self.theme.playing_bold_style(),
            )
        } else {
            (
                format!("{spinner} Preparing tools..."),
                self.theme.warning_bold_style(),
            )
        };

        let elapsed = format!("Elapsed {}", format_duration(self.elapsed().as_secs()));
        let line = if area.width < 80 {
            Line::from(vec![
                Span::styled(
                    " Tunr Downloader",
                    self.theme.accent_bold_style(),
                ),
                Span::styled(" | ", self.theme.muted_style()),
                Span::styled(state_label, state_style),
                Span::styled(" | ", self.theme.muted_style()),
                Span::styled(elapsed, self.theme.secondary_style()),
            ])
        } else {
            Line::from(vec![
                Span::styled(
                    concat!(" Tunr Downloader v", env!("CARGO_PKG_VERSION")),
                    self.theme.accent_bold_style(),
                ),
                Span::styled("  |  ", self.theme.muted_style()),
                Span::styled(state_label, state_style),
                Span::styled("  |  ", self.theme.muted_style()),
                Span::styled(
                    format!("Format: {}  Jobs: {}", self.format, self.jobs),
                    self.theme.accent_secondary_style(),
                ),
                Span::styled("  |  ", self.theme.muted_style()),
                Span::styled(elapsed, self.theme.secondary_style()),
            ])
        };

        let block = Block::bordered()
            .border_type(border_type(ascii))
            .style(self.theme.base_style())
            .border_style(self.theme.border_style(false));
        frame.render_widget(
            Paragraph::new(line)
                .style(self.theme.base_style())
                .block(block),
            area,
        );
    }

    fn render_overall(&self, frame: &mut Frame, area: Rect, ascii: bool) {
        if area.is_empty() {
            return;
        }
        let ratio = self.overall_ratio();
        let pct = (ratio * 100.0).round() as u16;
        let total = self.items.len();
        let (done, active, queued, failed) = self.summary_counts();
        let sep = if ascii { " | " } else { " · " };
        let title = if area.width < 68 {
            format!(" Progress ({done}/{total} done, {active} active, {failed} failed) ")
        } else {
            format!(
                " Batch Progress ({done}/{total} done{sep}{active} active{sep}{queued} queued{sep}{failed} failed) "
            )
        };

        let block = Block::bordered()
            .border_type(border_type(ascii))
            .style(self.theme.base_style())
            .border_style(self.theme.border_style(true))
            .title(Span::styled(title, self.theme.accent_bold_style()));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.is_empty() {
            return;
        }

        let gauge = Gauge::default()
            .ratio(ratio)
            .label(format!("{pct}%"))
            .style(self.theme.gauge_style())
            .gauge_style(self.theme.gauge_fill_style());
        frame.render_widget(gauge, inner);
    }

    fn render_items(&mut self, frame: &mut Frame, area: Rect, ascii: bool) {
        if area.is_empty() {
            return;
        }
        let total = self.items.len();
        let block = Block::bordered()
            .border_type(border_type(ascii))
            .style(self.theme.base_style())
            .border_style(self.theme.border_style(false))
            .title(Span::styled(
                format!(" Downloads ({total}) "),
                self.theme.panel_title_style(),
            ));

        let inner_width = area.width.saturating_sub(4) as usize;
        let spinner = active_spinner(self.spinner_idx, ascii);

        let list_items: Vec<ListItem> = self
            .items
            .iter()
            .enumerate()
            .map(|(idx, item)| {
                let (icon, phase_style) = match &item.phase {
                    DownloadPhase::Queued => (
                        if ascii { "-" } else { "·" },
                        self.theme.muted_style(),
                    ),
                    DownloadPhase::Starting
                    | DownloadPhase::Downloading { .. }
                    | DownloadPhase::PostProcessing { .. }
                    | DownloadPhase::Finalizing => (spinner, self.theme.accent_style()),
                    DownloadPhase::Completed { .. } => (
                        if ascii { "+" } else { "✓" },
                        self.theme.playing_style(),
                    ),
                    DownloadPhase::Failed { .. } => (
                        if ascii { "x" } else { "✗" },
                        self.theme.error_style(),
                    ),
                };

                let status_detail = format_item_detail(item, ascii);
                let bar = format_mini_bar(item.fraction(), 10, ascii);
                let prefix = format!("[{}/{}] {icon} ", idx + 1, total);
                let prefix_width = Span::raw(&prefix).width();
                let label = item.display_label();

                let header_line = Line::from(vec![
                    Span::styled(prefix, phase_style.add_modifier(Modifier::BOLD)),
                    Span::styled(
                        truncate_to_width(label.as_ref(), inner_width.saturating_sub(prefix_width)),
                        self.theme.emphasis_bold_style(),
                    ),
                ]);

                let detail_style = if item.is_failed() {
                    self.theme.error_style()
                } else if item.is_done() {
                    self.theme.playing_style()
                } else {
                    self.theme.secondary_style()
                };
                let detail_line = Line::from(vec![
                    Span::styled("      ", Style::new()),
                    Span::styled(bar, phase_style),
                    Span::raw(" "),
                    Span::styled(
                        truncate_to_width(&status_detail, inner_width.saturating_sub(19)),
                        detail_style,
                    ),
                ]);

                ListItem::new(vec![header_line, detail_line])
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
        let dest_raw = self
            .dest_dir
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "resolving...".to_string());
        let mut block = Block::bordered()
            .border_type(border_type(ascii))
            .style(self.theme.base_style())
            .border_style(self.theme.border_style(false));

        if let Some(notice) = &self.notice {
            block = block.title_bottom(
                Line::from(Span::styled(
                    format!(" note: {notice} "),
                    self.theme.warning_style(),
                ))
                .right_aligned(),
            );
        }

        let help_text = if ascii {
            "Up/Down: Scroll   q / Esc / Ctrl+C: Cancel"
        } else {
            "↑/↓: Scroll   q / Esc / Ctrl+C: Cancel"
        };
        let inner_width = area.width.saturating_sub(2) as usize;
        let help_width = Span::raw(help_text).width();
        let dest_budget = inner_width.saturating_sub(help_width + 14);
        let dest_text = if dest_budget >= 8 {
            truncate_to_width(&dest_raw, dest_budget)
        } else {
            dest_raw
        };

        let line = Line::from(vec![
            Span::styled(" Dest: ", self.theme.accent_style()),
            Span::styled(dest_text, self.theme.emphasis_style()),
            Span::styled("   |   ", self.theme.muted_style()),
            Span::styled(help_text, self.theme.secondary_style()),
        ]);
        frame.render_widget(
            Paragraph::new(line)
                .style(self.theme.base_style())
                .block(block),
            area,
        );
    }
}

fn format_item_detail(item: &DownloadItemState, ascii: bool) -> String {
    match &item.phase {
        DownloadPhase::Queued => "Queued".to_string(),
        DownloadPhase::Starting => "Fetching media metadata...".to_string(),
        DownloadPhase::Downloading {
            downloaded_bytes,
            total_bytes,
            speed_bytes_per_sec,
            eta_secs,
        } => {
            let mut parts = Vec::new();
            match (downloaded_bytes, total_bytes) {
                (Some(dl), Some(total)) if *total > 0 => {
                    let pct = ((*dl as f64 / *total as f64) * 100.0).clamp(0.0, 100.0);
                    parts.push(format!(
                        "{pct:>3.0}% ({} / {})",
                        format_bytes(*dl),
                        format_bytes(*total)
                    ));
                }
                (Some(dl), _) => parts.push(format_bytes(*dl)),
                _ => parts.push("Downloading...".to_string()),
            }
            if let Some(speed) = speed_bytes_per_sec {
                parts.push(format!("{}/s", format_bytes(*speed)));
            }
            if let Some(eta) = eta_secs {
                parts.push(format!("ETA {}", format_duration(*eta)));
            }
            parts.join(if ascii { " | " } else { " · " })
        }
        DownloadPhase::PostProcessing { stage } => format!("{stage}..."),
        DownloadPhase::Finalizing => "Moving to destination...".to_string(),
        DownloadPhase::Completed { files } => match files.first().and_then(|p| p.file_name()) {
            Some(name) if files.len() == 1 => format!("Saved: {}", name.to_string_lossy()),
            _ => format!("Saved {} track(s)", files.len()),
        },
        DownloadPhase::Failed { error } => error.clone(),
    }
}

fn format_mini_bar(fraction: f64, width: usize, ascii: bool) -> String {
    let clamped = fraction.clamp(0.0, 1.0);
    let filled = ((clamped * width as f64).round() as usize).min(width);
    let empty = width - filled;
    let (fill_ch, empty_ch) = if ascii { ("#", "-") } else { ("█", "░") };
    format!("[{}{}]", fill_ch.repeat(filled), empty_ch.repeat(empty))
}

fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    let b = bytes as f64;
    if b >= GIB {
        format!("{:.2} GiB", b / GIB)
    } else if b >= MIB {
        format!("{:.1} MiB", b / MIB)
    } else if b >= KIB {
        format!("{:.0} KiB", b / KIB)
    } else {
        format!("{bytes} B")
    }
}

fn is_cancel_key(key: &KeyEvent) -> bool {
    if key.kind != KeyEventKind::Press {
        return false;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && !key.modifiers.contains(KeyModifiers::ALT)
        && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
    {
        return true;
    }
    !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        && matches!(key.code, KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc)
}

/// Outcome of running the interactive download loading TUI on an active terminal session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadTuiOutcome {
    Completed(DownloadOutcome),
    Cancelled,
}

async fn resolve_plan_blocking(opts: &DownloadOptions) -> Result<Vec<LinkStatement>> {
    let opts_cloned = opts.clone();
    tokio::task::spawn_blocking(move || download::resolve_download_plan(&opts_cloned))
        .await
        .map_err(|err| TunrError::Async(format!("blocking task failed: {err}")))?
}

/// Run [`download::run_download_with_progress`] with an interactive ratatui loading TUI.
pub async fn run_download_tui(
    paths: &AppPaths,
    opts: &DownloadOptions,
    theme: &Theme,
) -> Result<DownloadOutcome> {
    let plan = resolve_plan_blocking(opts).await?;
    run_download_tui_plan(paths, opts, &plan, theme, None).await
}

/// Run [`download::run_download_plan_with_progress`] with an interactive ratatui loading TUI.
pub async fn run_download_tui_plan(
    paths: &AppPaths,
    opts: &DownloadOptions,
    plan: &[LinkStatement],
    theme: &Theme,
    theme_warning: Option<String>,
) -> Result<DownloadOutcome> {
    let mut session = TerminalSession::enter()?;
    let mut signals = ShutdownSignals::new()?;

    // Instantiate EventStream after TerminalSession so EventStream drops before raw mode restores.
    let mut events = EventStream::new();

    let res = run_download_tui_plan_on_session(
        &mut session,
        &mut events,
        &mut signals,
        paths,
        opts,
        plan,
        theme,
        theme_warning,
    )
    .await;
    drop(events);
    drop(session);

    match res? {
        DownloadTuiOutcome::Completed(outcome) => Ok(outcome),
        DownloadTuiOutcome::Cancelled => Err(TunrError::Download(
            "download cancelled by user".to_string(),
        )),
    }
}

/// Run [`download::run_download_with_progress`] inside an existing [`TerminalSession`].
pub async fn run_download_tui_on_session(
    session: &mut TerminalSession,
    events: &mut EventStream,
    signals: &mut ShutdownSignals,
    paths: &AppPaths,
    opts: &DownloadOptions,
    theme: &Theme,
) -> Result<DownloadTuiOutcome> {
    let plan = resolve_plan_blocking(opts).await?;
    run_download_tui_plan_on_session(session, events, signals, paths, opts, &plan, theme, None).await
}

/// Run [`download::run_download_plan_with_progress`] inside an existing [`TerminalSession`].
#[allow(clippy::too_many_arguments)]
pub async fn run_download_tui_plan_on_session(
    session: &mut TerminalSession,
    events: &mut EventStream,
    signals: &mut ShutdownSignals,
    paths: &AppPaths,
    opts: &DownloadOptions,
    plan: &[LinkStatement],
    theme: &Theme,
    theme_warning: Option<String>,
) -> Result<DownloadTuiOutcome> {
    let mut state = DownloadTuiState::from_plan(plan, opts.format, opts.jobs).with_theme(theme.clone());
    if state.notice.is_none()
        && let Some(warn) = theme_warning
    {
        state.notice = Some(warn);
    }

    let (tx, mut rx) = mpsc::unbounded_channel();
    let paths_cloned = paths.clone();
    let opts_cloned = opts.clone();
    let plan_cloned = plan.to_vec();
    let mut download_task = tokio::spawn(async move {
        download::run_download_plan_with_progress(&paths_cloned, &opts_cloned, &plan_cloned, Some(tx))
            .await
    });

    let mut tick = tokio::time::interval(TUI_TICK_INTERVAL);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        session
            .terminal_mut()
            .draw(|frame| state.render(frame))?;

        tokio::select! {
            res = &mut download_task => {
                while let Ok(ev) = rx.try_recv() {
                    state.apply_event(ev);
                }
                let outcome = res
                    .map_err(|err| TunrError::Async(format!("download task panicked: {err}")))??;
                return Ok(DownloadTuiOutcome::Completed(outcome));
            }
            Some(ev) = rx.recv() => {
                state.apply_event(ev);
                while let Ok(more) = rx.try_recv() {
                    state.apply_event(more);
                }
            }
            _ = tick.tick() => {
                state.tick();
            }
            maybe_ev = events.next() => {
                match maybe_ev {
                    Some(Ok(Event::Key(key)))
                        if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                    {
                        if is_cancel_key(&key) {
                            download_task.abort();
                            let _ = download_task.await;
                            return Ok(DownloadTuiOutcome::Cancelled);
                        }
                        if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
                            match key.code {
                                KeyCode::Up | KeyCode::Char('k') => state.move_up(),
                                KeyCode::Down | KeyCode::Char('j') => state.move_down(),
                                _ => {}
                            }
                        }
                    }
                    Some(Ok(Event::Resize(_, _))) | Some(Ok(_)) => {}
                    Some(Err(err)) => {
                        download_task.abort();
                        let _ = download_task.await;
                        return Err(err.into());
                    }
                    None => {}
                }
            }
            sig = signals.recv() => {
                download_task.abort();
                let _ = download_task.await;
                return Err(TunrError::Download(format!(
                    "download interrupted ({sig})"
                )));
            }
        }
    }
}

/// Run [`download::run_download_with_progress`] emitting non-TTY milestone lines to `stderr`.
pub async fn run_download_with_line_progress(
    paths: &AppPaths,
    opts: &DownloadOptions,
) -> Result<DownloadOutcome> {
    let plan = resolve_plan_blocking(opts).await?;
    run_download_plan_with_line_progress(paths, opts, &plan).await
}

/// Run [`download::run_download_plan_with_progress`] emitting non-TTY milestone lines to `stderr`.
pub async fn run_download_plan_with_line_progress(
    paths: &AppPaths,
    opts: &DownloadOptions,
    plan: &[LinkStatement],
) -> Result<DownloadOutcome> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let paths_cloned = paths.clone();
    let opts_cloned = opts.clone();
    let plan_cloned = plan.to_vec();
    let download_task = tokio::spawn(async move {
        download::run_download_plan_with_progress(&paths_cloned, &opts_cloned, &plan_cloned, Some(tx))
            .await
    });

    let mut total = 0usize;
    let mut last_percent_bucket: Vec<Option<u8>> = Vec::new();
    let mut active_urls: Vec<String> = Vec::new();
    while let Some(ev) = rx.recv().await {
        match ev {
            DownloadProgressEvent::BatchStarted {
                urls,
                dest_dir,
                format,
                jobs,
            } => {
                total = urls.len();
                last_percent_bucket = vec![None; total];
                active_urls = urls;
                eprintln!(
                    "Downloading {total} track(s) as {format} into {} (jobs={jobs})...",
                    dest_dir.display()
                );
            }
            DownloadProgressEvent::Notice(msg) => {
                eprintln!("note: {msg}");
            }
            DownloadProgressEvent::ItemStepStarted {
                index,
                url,
                step,
                total_steps,
                transition,
            } => {
                if let Some(slot) = last_percent_bucket.get_mut(index) {
                    *slot = None;
                }
                if let Some(slot) = active_urls.get_mut(index) {
                    *slot = url.clone();
                }
                let n = index + 1;
                match transition {
                    StepTransition::Fallback => {
                        eprintln!("[{n}/{total}] Fallback ({step}/{total_steps}): {url}");
                    }
                    StepTransition::Then => {
                        eprintln!("[{n}/{total}] Next ({step}/{total_steps}): {url}");
                    }
                    StepTransition::Primary => {
                        eprintln!("[{n}/{total}] Step {step}/{total_steps}: {url}");
                    }
                }
            }
            DownloadProgressEvent::ItemUpdate {
                index,
                title,
                phase,
            } => {
                let n = index + 1;
                let label = title
                    .as_deref()
                    .filter(|t| !t.trim().is_empty())
                    .unwrap_or_else(|| active_urls.get(index).map(String::as_str).unwrap_or("track"));
                match phase {
                    DownloadPhase::Starting => {
                        eprintln!("[{n}/{total}] Starting...");
                    }
                    DownloadPhase::Downloading {
                        downloaded_bytes: Some(dl),
                        total_bytes: Some(tot),
                        ..
                    } if tot > 0 => {
                        let pct = ((dl as f64 / tot as f64) * 100.0).clamp(0.0, 100.0) as u8;
                        let bucket = pct / 25;
                        if last_percent_bucket
                            .get(index)
                            .copied()
                            .flatten()
                            != Some(bucket)
                        {
                            if let Some(slot) = last_percent_bucket.get_mut(index) {
                                *slot = Some(bucket);
                            }
                            eprintln!("[{n}/{total}] Downloading {label}: {pct}%");
                        }
                    }
                    DownloadPhase::PostProcessing { stage } => {
                        eprintln!("[{n}/{total}] {stage} ({label})...");
                    }
                    DownloadPhase::Completed { files } => {
                        let saved = files
                            .first()
                            .and_then(|p| p.file_name())
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| label.to_string());
                        eprintln!("[{n}/{total}] Ready: {saved}");
                    }
                    DownloadPhase::Failed { error } => {
                        eprintln!("[{n}/{total}] Error: {error}");
                    }
                    _ => {}
                }
            }
            DownloadProgressEvent::IndexingStarted { track_count } => {
                eprintln!("Indexing {track_count} track(s) into library...");
            }
            DownloadProgressEvent::BatchFinished => {}
        }
    }

    download_task
        .await
        .map_err(|err| TunrError::Async(format!("download task panicked: {err}")))?
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::Color;

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

    #[test]
    fn renders_loading_progress_across_download_phases() {
        let mut state = DownloadTuiState::new(
            vec![
                "https://example.com/1".to_string(),
                "https://example.com/2".to_string(),
                "https://example.com/3".to_string(),
            ],
            DownloadFormat::Mp3,
            4,
        );
        state.elapsed_override = Some(Duration::from_secs(7));
        state.apply_event(DownloadProgressEvent::BatchStarted {
            urls: vec![
                "https://example.com/1".to_string(),
                "https://example.com/2".to_string(),
                "https://example.com/3".to_string(),
            ],
            dest_dir: PathBuf::from("/tmp/music"),
            format: DownloadFormat::Mp3,
            jobs: 4,
        });
        state.apply_event(DownloadProgressEvent::ItemUpdate {
            index: 0,
            title: Some("Alpha Song".to_string()),
            phase: DownloadPhase::Downloading {
                downloaded_bytes: Some(524_288),
                total_bytes: Some(1_048_576),
                speed_bytes_per_sec: Some(262_144),
                eta_secs: Some(2),
            },
        });
        state.apply_event(DownloadProgressEvent::ItemUpdate {
            index: 1,
            title: Some("Beta Song".to_string()),
            phase: DownloadPhase::PostProcessing {
                stage: "Extracting audio".to_string(),
            },
        });
        state.tick();

        let backend = TestBackend::new(100, 18);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| state.render(frame)).unwrap();
        let rendered = buffer_text(&terminal);

        assert!(rendered.contains("Tunr Downloader"));
        assert!(rendered.contains("Downloading..."));
        assert!(rendered.contains("Elapsed 00:07"));
        assert!(rendered.contains("Alpha Song"));
        assert!(rendered.contains("50%"));
        assert!(rendered.contains("Beta Song"));
        assert!(rendered.contains("Extracting audio..."));
        assert!(rendered.contains("https://example.com/3"));
        assert!(rendered.contains("Queued"));
        assert!(rendered.contains("/tmp/music"));

        state.apply_event(DownloadProgressEvent::IndexingStarted { track_count: 2 });
        terminal.draw(|frame| state.render(frame)).unwrap();
        let indexing_text = buffer_text(&terminal);
        assert!(indexing_text.contains("Indexing 2 track(s)..."));
    }

    #[test]
    fn navigation_and_formatting_helpers_behave_cleanly() {
        let mut state = DownloadTuiState::new(
            vec![
                "https://example.com/a".to_string(),
                "https://example.com/b".to_string(),
            ],
            DownloadFormat::Flac,
            2,
        );
        assert_eq!(state.list_state.selected(), Some(0));
        state.move_down();
        assert_eq!(state.list_state.selected(), Some(1));
        state.move_down();
        assert_eq!(state.list_state.selected(), Some(1));
        state.move_up();
        assert_eq!(state.list_state.selected(), Some(0));

        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2 KiB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MiB");
        assert_eq!(format_duration(65), "01:05");
        assert_eq!(format_duration(3665), "01:01:05");
    }

    #[test]
    fn cancel_key_requires_press_and_ignores_altgr_ctrl_alt_c() {
        assert!(is_cancel_key(&KeyEvent::new_with_kind(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
            KeyEventKind::Press,
        )));
        assert!(is_cancel_key(&KeyEvent::new_with_kind(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
            KeyEventKind::Press,
        )));
        assert!(!is_cancel_key(&KeyEvent::new_with_kind(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
            KeyEventKind::Repeat,
        )));
        assert!(!is_cancel_key(&KeyEvent::new_with_kind(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
            KeyEventKind::Press,
        )));
    }

    #[test]
    fn narrow_terminal_and_ascii_mode_render_cleanly() {
        let mut state = DownloadTuiState::new(
            vec!["https://example.com/1".to_string()],
            DownloadFormat::Mp3,
            2,
        );
        state.elapsed_override = Some(Duration::from_secs(12));
        state.apply_event(DownloadProgressEvent::ItemUpdate {
            index: 0,
            title: Some("日本語タイトルのとても長い楽曲名".to_string()),
            phase: DownloadPhase::Completed {
                files: vec![PathBuf::from("/music/song.mp3")],
            },
        });
        state.finished = true;

        let backend = TestBackend::new(46, 14);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| state.render_with_ascii(frame, true))
            .unwrap();
        let rendered = buffer_text(&terminal);

        assert!(rendered.contains("Tunr Downloader"));
        assert!(rendered.contains("Progress (1/1 done"));
        assert!(rendered.contains("[##########]"));
        assert!(!rendered.contains('╭'), "unexpected rounded border:\n{rendered}");
        assert!(!rendered.contains('✓'), "unexpected unicode check:\n{rendered}");
    }

    #[test]
    fn from_plan_and_item_step_started_update_url_title_labels_and_counts() {
        use crate::link::LinkExpr;

        let plan = vec![
            LinkStatement {
                line_number: Some(1),
                source_file: Some(PathBuf::from("test.link")),
                expr: LinkExpr::Or(
                    Box::new(LinkExpr::Url("https://primary.example/a".to_string())),
                    Box::new(LinkExpr::Url("https://fallback.example/a".to_string())),
                ),
            },
            LinkStatement {
                line_number: Some(2),
                source_file: Some(PathBuf::from("test.link")),
                expr: LinkExpr::And(
                    Box::new(LinkExpr::Url("https://first.example/b".to_string())),
                    Box::new(LinkExpr::Url("https://second.example/b".to_string())),
                ),
            },
            LinkStatement {
                line_number: None,
                source_file: None,
                expr: LinkExpr::Url("https://single.example/c".to_string()),
            },
        ];

        let mut state = DownloadTuiState::from_plan(&plan, DownloadFormat::Mp3, 2);
        assert_eq!(state.items.len(), 3);
        assert_eq!(state.items[0].step, 0);
        assert_eq!(state.items[0].total_steps, 2);
        assert_eq!(state.items[0].transition, None);
        assert_eq!(
            state.items[0].display_label().as_ref(),
            "https://primary.example/a || https://fallback.example/a"
        );
        assert_eq!(
            state.items[2].display_label().as_ref(),
            "https://single.example/c"
        );
        assert_eq!(state.summary_counts(), (0, 0, 3, 0));
        assert_eq!(state.overall_ratio(), 0.0);

        state.apply_event(DownloadProgressEvent::BatchStarted {
            urls: plan.iter().map(|s| s.expr.to_string()).collect(),
            dest_dir: PathBuf::from("/tmp/music"),
            format: DownloadFormat::Mp3,
            jobs: 2,
        });
        assert_eq!(state.items[0].total_steps, 2);

        state.apply_event(DownloadProgressEvent::ItemStepStarted {
            index: 0,
            url: "https://primary.example/a".to_string(),
            step: 1,
            total_steps: 2,
            transition: StepTransition::Primary,
        });
        assert_eq!(state.items[0].url, "https://primary.example/a");
        assert_eq!(state.items[0].phase, DownloadPhase::Starting);
        assert_eq!(
            state.items[0].display_label().as_ref(),
            "[step 1/2] https://primary.example/a"
        );
        assert_eq!(state.summary_counts(), (0, 1, 2, 0));
        assert!(state.overall_ratio() > 0.0);

        state.apply_event(DownloadProgressEvent::ItemUpdate {
            index: 0,
            title: Some("Primary Track".to_string()),
            phase: DownloadPhase::Downloading {
                downloaded_bytes: Some(50),
                total_bytes: Some(100),
                speed_bytes_per_sec: None,
                eta_secs: None,
            },
        });
        assert_eq!(
            state.items[0].display_label().as_ref(),
            "[step 1/2] Primary Track"
        );

        state.apply_event(DownloadProgressEvent::ItemStepStarted {
            index: 0,
            url: "https://fallback.example/a".to_string(),
            step: 2,
            total_steps: 2,
            transition: StepTransition::Fallback,
        });
        assert_eq!(state.items[0].url, "https://fallback.example/a");
        assert_eq!(state.items[0].title, None);
        assert_eq!(state.items[0].phase, DownloadPhase::Starting);
        assert_eq!(
            state.items[0].display_label().as_ref(),
            "[step 2/2 fallback] https://fallback.example/a"
        );

        state.apply_event(DownloadProgressEvent::ItemStepStarted {
            index: 1,
            url: "https://second.example/b".to_string(),
            step: 2,
            total_steps: 2,
            transition: StepTransition::Then,
        });
        assert_eq!(
            state.items[1].display_label().as_ref(),
            "[step 2/2 then] https://second.example/b"
        );

        state.apply_event(DownloadProgressEvent::ItemStepStarted {
            index: 2,
            url: "https://single.example/c".to_string(),
            step: 1,
            total_steps: 1,
            transition: StepTransition::Primary,
        });
        assert_eq!(
            state.items[2].display_label().as_ref(),
            "https://single.example/c"
        );

        let backend = TestBackend::new(110, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| state.render(frame)).unwrap();
        let rendered = buffer_text(&terminal);
        assert!(rendered.contains("[step 2/2 fallback] https://fallback.example/a"));
        assert!(rendered.contains("[step 2/2 then] https://second.example/b"));

        state.apply_event(DownloadProgressEvent::ItemUpdate {
            index: 0,
            title: Some("Fallback Track".to_string()),
            phase: DownloadPhase::Completed {
                files: vec![PathBuf::from("/tmp/music/fallback.mp3")],
            },
        });
        state.apply_event(DownloadProgressEvent::ItemUpdate {
            index: 1,
            title: None,
            phase: DownloadPhase::Failed {
                error: "step 2 failed".to_string(),
            },
        });
        assert_eq!(state.summary_counts(), (1, 1, 0, 1));
        assert!(state.overall_ratio() > 0.6);

        state.apply_event(DownloadProgressEvent::BatchFinished);
        assert_eq!(state.overall_ratio(), 1.0);
    }

    #[test]
    fn from_options_supports_inline_targets() {
        let opts = DownloadOptions {
            urls: Vec::new(),
            targets: vec![DownloadTarget::Url("https://example.com/inline".to_string())],
            dest: None,
            set_dest: None,
            format: DownloadFormat::Mp3,
            playlist: false,
            embed_thumbnail: true,
            auto_add: true,
            playlist_name: None,
            jobs: 2,
            no_tui: false,
        };
        let state = DownloadTuiState::from_options(&opts);
        assert_eq!(state.items.len(), 1);
        assert_eq!(state.items[0].url, "https://example.com/inline");
        assert_eq!(state.items[0].total_steps, 1);
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
    fn tunr_dark_theme_preserves_download_styles() {
        let mut state = DownloadTuiState::new(
            vec![
                "https://example.com/1".to_string(),
                "https://example.com/2".to_string(),
                "https://example.com/3".to_string(),
                "https://example.com/4".to_string(),
            ],
            DownloadFormat::Mp3,
            4,
        );
        state.elapsed_override = Some(Duration::from_secs(5));
        state.apply_event(DownloadProgressEvent::Notice("retrying".to_string()));
        state.apply_event(DownloadProgressEvent::ItemUpdate {
            index: 0,
            title: Some("Active Song".to_string()),
            phase: DownloadPhase::Downloading {
                downloaded_bytes: Some(50),
                total_bytes: Some(100),
                speed_bytes_per_sec: None,
                eta_secs: None,
            },
        });
        state.apply_event(DownloadProgressEvent::ItemUpdate {
            index: 1,
            title: Some("Done Song".to_string()),
            phase: DownloadPhase::Completed {
                files: vec![PathBuf::from("/tmp/done.mp3")],
            },
        });
        state.apply_event(DownloadProgressEvent::ItemUpdate {
            index: 2,
            title: Some("Broken Song".to_string()),
            phase: DownloadPhase::Failed {
                error: "network timeout".to_string(),
            },
        });

        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal.draw(|f| state.render(f)).unwrap();
        let buf = terminal.backend().buffer();

        assert_eq!(buf[(95, 1)].bg, Color::Reset);
        let (fmt_x, fmt_y) = find_text_pos(&terminal, "Format: mp3");
        assert_eq!(buf[(fmt_x, fmt_y)].fg, Color::Magenta);
        let (done_x, done_y) = find_text_pos(&terminal, "Saved: done.mp3");
        assert_eq!(buf[(done_x, done_y)].fg, Color::Green);
        let (err_x, err_y) = find_text_pos(&terminal, "network timeout");
        assert_eq!(buf[(err_x, err_y)].fg, Color::Red);
        let (q_prefix_x, q_prefix_y) = find_text_pos(&terminal, "[4/4]");
        assert_eq!(buf[(q_prefix_x, q_prefix_y)].fg, Color::DarkGray);
        let (q_x, q_y) = find_text_pos(&terminal, "Queued");
        assert_eq!(buf[(q_x, q_y)].fg, Color::Gray);
        let (note_x, note_y) = find_text_pos(&terminal, "note: retrying");
        assert_eq!(buf[(note_x, note_y)].fg, Color::Yellow);
        // Selected item (index 0) uses DarkGray highlight bg
        let (active_x, active_y) = find_text_pos(&terminal, "Active Song");
        assert_eq!(buf[(active_x, active_y)].bg, Color::DarkGray);
        // Gauge uses DarkGray track bg and Cyan filled bar
        assert_eq!(buf[(2, 4)].bg, Color::DarkGray);
    }

    #[test]
    fn tunr_light_theme_paints_explicit_bg_and_gauge_surface() {
        let light = Theme::tunr_light();
        let light_bg = light.bg.to_color();
        assert_eq!(light_bg, Color::Rgb(246, 248, 250));

        let mut state = DownloadTuiState::new(
            vec![
                "https://example.com/1".to_string(),
                "https://example.com/2".to_string(),
            ],
            DownloadFormat::Flac,
            2,
        )
        .with_theme(light.clone());
        state.elapsed_override = Some(Duration::from_secs(12));
        state.apply_event(DownloadProgressEvent::ItemUpdate {
            index: 0,
            title: Some("Light Active".to_string()),
            phase: DownloadPhase::Downloading {
                downloaded_bytes: Some(50),
                total_bytes: Some(100),
                speed_bytes_per_sec: None,
                eta_secs: None,
            },
        });
        state.apply_event(DownloadProgressEvent::ItemUpdate {
            index: 1,
            title: Some("Light Failed".to_string()),
            phase: DownloadPhase::Failed {
                error: "http 404".to_string(),
            },
        });

        let mut terminal = Terminal::new(TestBackend::new(100, 16)).unwrap();
        terminal.draw(|f| state.render(f)).unwrap();
        let buf = terminal.backend().buffer();

        for y in 0..16u16 {
            for x in 0..100u16 {
                assert_ne!(
                    buf[(x, y)].bg,
                    Color::Reset,
                    "dark terminal bleed-through at ({x}, {y})"
                );
            }
        }

        // Empty header/footer padding uses explicit light bg
        assert_eq!(buf[(96, 1)].bg, light_bg);
        assert_eq!(buf[(96, 14)].bg, light_bg);
        // Gauge track uses light surface (#e6ebf1) and filled section uses accent (#0969da)
        assert_eq!(buf[(96, 4)].bg, light.surface.to_color());
        assert_eq!(buf[(2, 4)].fg, light.accent.to_color());
        // Selected item (index 0) uses selection_bg while preserving accent fg on [1/2]
        let (prefix_x, prefix_y) = find_text_pos(&terminal, "[1/2]");
        assert_eq!(buf[(prefix_x, prefix_y)].fg, light.accent.to_color());
        assert_eq!(buf[(prefix_x, prefix_y)].bg, light.selection_bg.to_color());
        // Unselected failed item keeps light bg and error fg
        let (fail_x, fail_y) = find_text_pos(&terminal, "http 404");
        assert_eq!(buf[(fail_x, fail_y)].fg, light.error.to_color());
        assert_eq!(buf[(fail_x, fail_y)].bg, light_bg);
    }
}
