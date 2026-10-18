//! A stationary primary-screen status row. Completed lines enter ordinary
//! scrollback. No alternate screen or animation thread.
use std::io::Write;
use unicode_width::UnicodeWidthStr;

use crate::ui;

#[derive(Default)]
pub struct Footer {
    rows: usize,
}

impl Footer {
    /// The cursor lives on the final panel row, never one row beyond it.
    pub fn clear(&mut self, out: &mut impl Write) {
        if self.rows == 0 {
            return;
        }
        let _ = write!(out, "\r");
        if self.rows > 1 {
            let _ = write!(out, "\x1b[{}A", self.rows - 1);
        }
        for row in 0..self.rows {
            let _ = write!(out, "\x1b[2K");
            if row + 1 < self.rows {
                let _ = write!(out, "\x1b[1B\r");
            }
        }
        if self.rows > 1 {
            let _ = write!(out, "\x1b[{}A", self.rows - 1);
        }
        self.rows = 0;
    }

    pub fn draw(&mut self, out: &mut impl Write, frame: usize, verb: &str, status: &str, secs: u64) {
        self.clear(out);
        let lines = panel(ui::width(), ui::height(), frame, verb, status, secs);
        for (i, line) in lines.iter().enumerate() {
            if i > 0 {
                let _ = write!(out, "\r\n");
            }
            let _ = write!(out, "{}", ui::info(line));
        }
        self.rows = lines.len();
    }
}

/// Clips by terminal cells, removes control characters, and leaves the final
/// terminal column unused so a frame cannot trigger automatic wrapping.
fn fit(s: &str, cells: usize) -> String {
    let mut text = String::new();
    for c in s.chars().filter(|c| !c.is_control()) {
        text.push(c);
        if text.width() > cells {
            text.pop();
            break;
        }
    }
    text
}

fn panel(width: usize, _height: usize, _frame: usize, verb: &str, status: &str, secs: u64) -> Vec<String> {
    let cells = width.saturating_sub(1);
    let hint = if width >= 40 { " · ctrl-c to stop" } else { "" };
    let tail = format!(" · {secs}s{hint}");
    let state = if status.is_empty() { verb.to_string() } else { format!("{verb} · {status}") };
    let prefix = "◇ ";
    let available = cells.saturating_sub(prefix.width() + tail.width());
    vec![fit(&format!("{prefix}{}{tail}", fit(&state, available)), cells)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_do_not_wrap_at_any_terminal_size() {
        for width in [1, 12, 24, 31, 40, 80, 120, 300] {
            for height in [3, 24] {
                for frame in 0..12 {
                    let lines = panel(width, height, frame, "Thinking", "文件 🦀 very long status\r\n\x1b", 12345);
                    assert!(lines.iter().all(|line| line.width() < width));
                    assert!(lines.iter().all(|line| !line.chars().any(char::is_control)));
                    assert_eq!(lines.len(), 1);
                }
            }
        }
    }

    #[test]
    fn status_is_still_and_keeps_stop_hint_and_elapsed_time() {
        assert_eq!(panel(80, 24, 0, "Thinking", "reading", 4), panel(80, 24, 11, "Thinking", "reading", 4));
        let lines = panel(40, 24, 0, "Thinking", &"x".repeat(200), 14);
        assert!(lines[0].ends_with(" · 14s · ctrl-c to stop"));
    }
}
