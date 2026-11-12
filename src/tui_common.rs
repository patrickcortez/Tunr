use std::io::{self, IsTerminal, stdout};
use std::sync::Once;

use crossterm::cursor::Show;
use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, enable_raw_mode};
use ratatui::backend::CrosstermBackend;
use ratatui::text::Span;
use ratatui::{DefaultTerminal, Terminal};

use crate::error::{Result, TermusicError};

static PANIC_HOOK_ONCE: Once = Once::new();

/// Return whether UI chrome should fall back to pure ASCII glyphs.
pub fn use_ascii_glyphs() -> bool {
    std::env::var_os("TERMUSIC_ASCII")
        .is_some_and(|val| val == "1" || val.eq_ignore_ascii_case("true"))
}

/// Restore cursor visibility, exit the alternate screen, and disable raw mode.
pub fn restore_terminal_state() {
    let _ = execute!(stdout(), Show);
    ratatui::restore();
}

/// Install a one-time panic hook that restores terminal state before chaining.
pub fn install_panic_hook() {
    PANIC_HOOK_ONCE.call_once(|| {
        let prev_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_terminal_state();
            prev_hook(info);
        }));
    });
}

/// RAII wrapper owning an initialized interactive `ratatui` terminal session.
///
/// Any `EventStream` must be constructed after [`TerminalSession::enter`] so it
/// drops before raw mode and the alternate screen are torn down.
pub struct TerminalSession {
    terminal: DefaultTerminal,
}

impl TerminalSession {
    /// Verify interactive TTYs, install the panic hook, enter raw mode and the
    /// alternate screen, and initialize `ratatui`.
    pub fn enter() -> Result<Self> {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            return Err(TermusicError::Io(io::Error::other(
                "interactive TUI requires both stdin and stdout to be terminals",
            )));
        }

        install_panic_hook();
        enable_raw_mode()?;

        if let Err(err) = execute!(stdout(), EnterAlternateScreen) {
            restore_terminal_state();
            return Err(err.into());
        }

        let mut terminal = match Terminal::new(CrosstermBackend::new(stdout())) {
            Ok(term) => term,
            Err(err) => {
                restore_terminal_state();
                return Err(err.into());
            }
        };
        let _ = terminal.clear();

        Ok(Self { terminal })
    }

    /// Borrow the underlying `ratatui` terminal mutably.
    pub fn terminal_mut(&mut self) -> &mut DefaultTerminal {
        &mut self.terminal
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = self.terminal.show_cursor();
        restore_terminal_state();
    }
}

/// Fit `text` to `max_width` display columns, right-padding with spaces or
/// truncating with a single-column ellipsis (`…`).
pub fn fit_to_width(text: &str, max_width: usize) -> String {
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

/// Truncate `text` to at most `max_width` display columns without right-padding,
/// appending `...` when truncated if space permits.
pub fn truncate_to_width(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if Span::raw(text).width() <= max_width {
        return text.to_string();
    }

    let (target, suffix) = if max_width <= 3 {
        (max_width, "")
    } else {
        (max_width - 3, "...")
    };

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
    out.push_str(suffix);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_to_width_pads_and_truncates_unicode_display_columns() {
        assert_eq!(fit_to_width("abc", 5), "abc  ");
        assert_eq!(fit_to_width("abc", 3), "abc");
        assert_eq!(fit_to_width("abcdef", 4), "abc…");
        assert_eq!(fit_to_width("abcdef", 1), "…");
        assert_eq!(fit_to_width("abcdef", 0), "");

        // Wide CJK characters take 2 display cells each.
        let fitted = fit_to_width("日本語の歌", 5);
        assert_eq!(Span::raw(&fitted).width(), 5);
        assert_eq!(fitted, "日本…");

        let exact_even_target_with_wide_leftover = fit_to_width("日本語", 4);
        assert_eq!(Span::raw(&exact_even_target_with_wide_leftover).width(), 4);
        assert_eq!(exact_even_target_with_wide_leftover, "日… ");
    }

    #[test]
    fn truncate_to_width_handles_cjk_without_padding() {
        assert_eq!(truncate_to_width("abc", 10), "abc");
        assert_eq!(truncate_to_width("abcdefg", 6), "abc...");
        let truncated = truncate_to_width("日本語タイトル", 7);
        assert!(Span::raw(&truncated).width() <= 7);
        assert_eq!(truncated, "日本...");
        assert_eq!(truncate_to_width("日本語", 2), "日");
        assert_eq!(truncate_to_width("abc", 0), "");
    }
}
