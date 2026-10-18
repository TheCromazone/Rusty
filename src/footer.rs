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
    /// Keep the cursor at the start of this transient row. Terminal reflow then
    /// preserves its anchor when the pane contracts; erase its wrapped tail too.
    pub fn clear(&mut self, out: &mut impl Write) {
        if self.rows > 0 {
            let _ = write!(out, "\r\x1b[J");
            self.rows = 0;
        }
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
        let _ = write!(out, "\r");
        self.rows = lines.len();
    }
}

/// Clips by terminal cells, removes control characters, and leaves the final
/// terminal column unused so a frame cannot trigger automatic wrapping.
fn fit(s: &str, cells: usize) -> String {
    let text: String = s.chars().filter(|c| !c.is_control()).collect();
    // Most rows fit as they are; measuring once is enough for those, and
    // only a row that is too long is cut a character at a time.
    if text.width() <= cells {
        return text;
    }
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
    fn fitting_matches_cutting_a_character_at_a_time() {
        let slow = |s: &str, cells: usize| {
            let mut text = String::new();
            for c in s.chars().filter(|c| !c.is_control()) {
                text.push(c);
                if text.width() > cells {
                    text.pop();
                    break;
                }
            }
            text
        };
        for s in ["", "reading src", "文件 🦀 very long status\r\n\x1b", "◇ rusty · 4s", "👨\u{200d}👩\u{200d}👧 ok"]
        {
            for cells in 0..40 {
                assert_eq!(fit(s, cells), slow(s, cells), "{s:?} in {cells}");
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
