use std::io::stdout;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::cursor::Show;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use futures_util::StreamExt;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Gauge, HighlightSpacing, List, ListItem, ListState, Paragraph,
};
use tokio::signal::unix::{SignalKind, signal as unix_signal};
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;

use crate::download::{
    self, DownloadFormat, DownloadOptions, DownloadOutcome, DownloadPhase, DownloadProgressEvent,
};
use crate::error::{Result, TermusicError};
use crate::paths::AppPaths;

const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const TUI_TICK_INTERVAL: Duration = Duration::from_millis(80);

struct DownloadTerminalGuard;

impl Drop for DownloadTerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(stdout(), Show);
        ratatui::restore();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DownloadItemState {
    url: String,
    title: Option<String>,
    phase: DownloadPhase,
}

impl DownloadItemState {
    fn new(url: String) -> Self {
        Self {
            url,
            title: None,
            phase: DownloadPhase::Queued,
        }
    }

    fn display_label(&self) -> &str {
        self.title.as_deref().unwrap_or(&self.url)
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
}

impl DownloadTuiState {
    /// Initialize state from validated download options before background workers begin.
    pub fn from_options(opts: &DownloadOptions) -> Self {
        let urls = download::normalize_urls(&opts.urls).unwrap_or_else(|_| opts.urls.clone());
        Self::new(urls, opts.format, opts.jobs.max(1))
    }

    /// Construct a download UI state from explicit normalized URLs, format, and concurrency.
    pub fn new(urls: Vec<String>, format: DownloadFormat, jobs: usize) -> Self {
        let items: Vec<DownloadItemState> = urls.into_iter().map(DownloadItemState::new).collect();
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
        }
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
        let area = frame.area();
        let [header_area, overall_area, list_area, footer_area] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(3),
        ])
        .areas(area);

        self.render_header(frame, header_area);
        self.render_overall(frame, overall_area);
        self.render_items(frame, list_area);
        self.render_footer(frame, footer_area);
    }

    fn render_header(&self, frame: &mut Frame, area: Rect) {
        if area.is_empty() {
            return;
        }
        let spinner = SPINNER_FRAMES[self.spinner_idx % SPINNER_FRAMES.len()];
        let (done, active, _queued, failed) = self.summary_counts();
        let (state_label, state_style) = if self.finished {
            if failed > 0 {
                (
                    "Finished (with errors)".to_string(),
                    Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                )
            } else {
                (
                    "Complete".to_string(),
                    Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
                )
            }
        } else if self.indexing {
            (
                format!("{spinner} Indexing {} track(s)...", self.indexing_count),
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            )
        } else if active > 0 || done > 0 || failed > 0 {
            (
                format!("{spinner} Downloading..."),
                Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
            )
        } else {
            (
                format!("{spinner} Preparing tools..."),
                Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            )
        };

        let line = Line::from(vec![
            Span::styled(
                concat!(" Termusic Downloader v", env!("CARGO_PKG_VERSION")),
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  |  ", Style::new().fg(Color::DarkGray)),
            Span::styled(state_label, state_style),
            Span::styled("  |  ", Style::new().fg(Color::DarkGray)),
            Span::styled(
                format!("Format: {}  Jobs: {}", self.format, self.jobs),
                Style::new().fg(Color::Magenta),
            ),
            Span::styled("  |  ", Style::new().fg(Color::DarkGray)),
            Span::styled(
                format!("Elapsed {}", format_duration(self.elapsed().as_secs())),
                Style::new().fg(Color::Gray),
            ),
        ]);

        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(Color::DarkGray));
        frame.render_widget(Paragraph::new(line).block(block), area);
    }

    fn render_overall(&self, frame: &mut Frame, area: Rect) {
        if area.is_empty() {
            return;
        }
        let ratio = self.overall_ratio();
        let pct = (ratio * 100.0).round() as u16;
        let total = self.items.len();
        let (done, active, queued, failed) = self.summary_counts();

        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(Color::Cyan))
            .title(Span::styled(
                format!(
                    " Batch Progress ({done}/{total} done · {active} active · {queued} queued · {failed} failed) "
                ),
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.is_empty() {
            return;
        }

        let gauge = Gauge::default()
            .ratio(ratio)
            .label(format!("{pct}%"))
            .style(Style::new().fg(Color::White).bg(Color::DarkGray))
            .gauge_style(Style::new().fg(Color::Cyan).bg(Color::DarkGray));
        frame.render_widget(gauge, inner);
    }

    fn render_items(&mut self, frame: &mut Frame, area: Rect) {
        if area.is_empty() {
            return;
        }
        let total = self.items.len();
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(Color::DarkGray))
            .title(Span::styled(
                format!(" Downloads ({total}) "),
                Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
            ));

        let inner_width = area.width.saturating_sub(4) as usize;
        let spinner = SPINNER_FRAMES[self.spinner_idx % SPINNER_FRAMES.len()];

        let list_items: Vec<ListItem> = self
            .items
            .iter()
            .enumerate()
            .map(|(idx, item)| {
                let (icon, icon_color) = match &item.phase {
                    DownloadPhase::Queued => ("·", Color::DarkGray),
                    DownloadPhase::Starting
                    | DownloadPhase::Downloading { .. }
                    | DownloadPhase::PostProcessing { .. }
                    | DownloadPhase::Finalizing => (spinner, Color::Cyan),
                    DownloadPhase::Completed { .. } => ("✓", Color::Green),
                    DownloadPhase::Failed { .. } => ("✗", Color::Red),
                };

                let status_detail = format_item_detail(item);
                let bar = format_mini_bar(item.fraction(), 10);
                let prefix = format!("[{}/{}] {icon} ", idx + 1, total);
                let label = item.display_label();

                let header_line = Line::from(vec![
                    Span::styled(prefix, Style::new().fg(icon_color).add_modifier(Modifier::BOLD)),
                    Span::styled(
                        fit_text(label, inner_width.saturating_sub(10)),
                        Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
                    ),
                ]);

                let detail_line = Line::from(vec![
                    Span::styled("      ", Style::new()),
                    Span::styled(bar, Style::new().fg(icon_color)),
                    Span::raw(" "),
                    Span::styled(
                        fit_text(&status_detail, inner_width.saturating_sub(19)),
                        Style::new().fg(if item.is_failed() {
                            Color::Red
                        } else if item.is_done() {
                            Color::Green
                        } else {
                            Color::Gray
                        }),
                    ),
                ]);

                ListItem::new(vec![header_line, detail_line])
            })
            .collect();

        let list = List::new(list_items)
            .block(block)
            .highlight_symbol("> ")
            .highlight_spacing(HighlightSpacing::Always)
            .highlight_style(
                Style::new()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            );
        frame.render_stateful_widget(list, area, &mut self.list_state);
    }

    fn render_footer(&self, frame: &mut Frame, area: Rect) {
        if area.is_empty() {
            return;
        }
        let dest_text = self
            .dest_dir
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "resolving...".to_string());
        let mut block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(Color::DarkGray));

        if let Some(notice) = &self.notice {
            block = block.title_bottom(
                Line::from(Span::styled(
                    format!(" note: {notice} "),
                    Style::new().fg(Color::Yellow),
                ))
                .right_aligned(),
            );
        }

        let line = Line::from(vec![
            Span::styled(" Dest: ", Style::new().fg(Color::Cyan)),
            Span::styled(dest_text, Style::new().fg(Color::White)),
            Span::styled("   |   ", Style::new().fg(Color::DarkGray)),
            Span::styled(
                "↑/↓: Scroll   q / Esc / Ctrl+C: Cancel",
                Style::new().fg(Color::Gray),
            ),
        ]);
        frame.render_widget(Paragraph::new(line).block(block), area);
    }
}

fn format_item_detail(item: &DownloadItemState) -> String {
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
            parts.join(" · ")
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

fn format_mini_bar(fraction: f64, width: usize) -> String {
    let clamped = fraction.clamp(0.0, 1.0);
    let filled = ((clamped * width as f64).round() as usize).min(width);
    let empty = width - filled;
    format!("[{}{}]", "█".repeat(filled), "░".repeat(empty))
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

fn format_duration(total_secs: u64) -> String {
    let mins = total_secs / 60;
    let secs = total_secs % 60;
    if mins >= 60 {
        let hrs = mins / 60;
        let rem_mins = mins % 60;
        format!("{hrs:02}:{rem_mins:02}:{secs:02}")
    } else {
        format!("{mins:02}:{secs:02}")
    }
}

fn fit_text(text: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    if max_chars <= 3 {
        return text.chars().take(max_chars).collect();
    }
    let mut out: String = text.chars().take(max_chars - 3).collect();
    out.push_str("...");
    out
}

fn is_cancel_key(key: &KeyEvent) -> bool {
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
    {
        return true;
    }
    !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        && matches!(key.code, KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc)
}

/// Run [`download::run_download_with_progress`] with an interactive ratatui loading TUI.
pub async fn run_download_tui(
    paths: &AppPaths,
    opts: &DownloadOptions,
) -> Result<DownloadOutcome> {
    let mut state = DownloadTuiState::from_options(opts);

    let mut terminal = match ratatui::try_init() {
        Ok(term) => term,
        Err(err) => {
            ratatui::restore();
            return Err(err.into());
        }
    };
    let _guard = DownloadTerminalGuard;

    let mut sigint = unix_signal(SignalKind::interrupt())?;
    let mut sigterm = unix_signal(SignalKind::terminate())?;

    // Instantiate EventStream after DownloadTerminalGuard so EventStream drops before raw mode restores.
    let mut events = EventStream::new();

    let (tx, mut rx) = mpsc::unbounded_channel();
    let paths_cloned = paths.clone();
    let opts_cloned = opts.clone();
    let mut download_task = tokio::spawn(async move {
        download::run_download_with_progress(&paths_cloned, &opts_cloned, Some(tx)).await
    });

    let mut tick = tokio::time::interval(TUI_TICK_INTERVAL);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        terminal.draw(|frame| state.render(frame))?;

        tokio::select! {
            res = &mut download_task => {
                while let Ok(ev) = rx.try_recv() {
                    state.apply_event(ev);
                }
                return res.map_err(|err| TermusicError::Async(format!("download task panicked: {err}")))?;
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
                    Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                        if is_cancel_key(&key) {
                            download_task.abort();
                            let _ = download_task.await;
                            return Err(TermusicError::Download(
                                "download cancelled by user".to_string(),
                            ));
                        }
                        match key.code {
                            KeyCode::Up | KeyCode::Char('k') => state.move_up(),
                            KeyCode::Down | KeyCode::Char('j') => state.move_down(),
                            _ => {}
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
            _ = sigint.recv() => {
                download_task.abort();
                let _ = download_task.await;
                return Err(TermusicError::Download(
                    "download interrupted (SIGINT)".to_string(),
                ));
            }
            _ = sigterm.recv() => {
                download_task.abort();
                let _ = download_task.await;
                return Err(TermusicError::Download(
                    "download terminated (SIGTERM)".to_string(),
                ));
            }
        }
    }
}

/// Run [`download::run_download_with_progress`] emitting non-TTY milestone lines to `stderr`.
pub async fn run_download_with_line_progress(
    paths: &AppPaths,
    opts: &DownloadOptions,
) -> Result<DownloadOutcome> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let paths_cloned = paths.clone();
    let opts_cloned = opts.clone();
    let download_task = tokio::spawn(async move {
        download::run_download_with_progress(&paths_cloned, &opts_cloned, Some(tx)).await
    });

    let mut total = 0usize;
    let mut last_percent_bucket: Vec<Option<u8>> = Vec::new();
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
                eprintln!(
                    "Downloading {total} track(s) as {format} into {} (jobs={jobs})...",
                    dest_dir.display()
                );
            }
            DownloadProgressEvent::Notice(msg) => {
                eprintln!("note: {msg}");
            }
            DownloadProgressEvent::ItemUpdate {
                index,
                title,
                phase,
            } => {
                let n = index + 1;
                let label = title.as_deref().unwrap_or("track");
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
        .map_err(|err| TermusicError::Async(format!("download task panicked: {err}")))?
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

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

        assert!(rendered.contains("Termusic Downloader"));
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
}
