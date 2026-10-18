//! Just enough markdown for a terminal: headings, bullets, quotes, rules,
//! `code`, **bold** and fenced code blocks. Rendering is per line so it works
//! while text streams in. Pipes get the raw markdown untouched.

use crate::ui;

/// Renders one complete line. `in_code` tracks fenced blocks across lines.
/// Prose wraps at word boundaries one cell short of the terminal width, so
/// the terminal's own hard wrap never splits a word.
pub fn line(text: &str, in_code: &mut bool) -> String {
    if !ui::tty() {
        // Redirected output stays exactly as written; a NO_COLOR terminal
        // gets the raw markdown, still wrapped for the screen.
        return if ui::terminal() { plain(text, in_code, ui::width().saturating_sub(1)) } else { text.to_string() };
    }
    render(text, in_code, ui::width().saturating_sub(1))
}

/// Unstyled markdown wrapped to `cols`: fences, table rows and rules as
/// they are, list items and quotes continuing under their marker.
fn plain(text: &str, in_code: &mut bool, cols: usize) -> String {
    let trimmed = text.trim_start();
    let indent = &text[..text.len() - trimmed.len()];
    if trimmed.starts_with("```") {
        *in_code = !*in_code;
        return text.to_string();
    }
    if *in_code
        || trimmed.starts_with('|')
        || (trimmed.len() >= 3 && trimmed.chars().all(|c| matches!(c, '-' | '*' | '_')))
    {
        return text.to_string();
    }
    let marker = if trimmed.starts_with("- ") || trimmed.starts_with("* ") || trimmed.starts_with("> ") {
        2
    } else {
        match trimmed.split_once(". ") {
            Some((num, _)) if !num.is_empty() && num.len() <= 3 && num.chars().all(|c| c.is_ascii_digit()) => {
                num.len() + 2
            }
            _ => 0,
        }
    };
    let hang = if trimmed.starts_with("> ") { format!("{indent}> ") } else { " ".repeat(cells(indent) + marker) };
    wrap(text, cols, &hang)
}

fn render(text: &str, in_code: &mut bool, cols: usize) -> String {
    let trimmed = text.trim_start();
    let indent = &text[..text.len() - trimmed.len()];
    if let Some(lang) = trimmed.strip_prefix("```") {
        *in_code = !*in_code;
        return if *in_code {
            ui::dim(&format!("{indent}╭─ {}", if lang.trim().is_empty() { "code" } else { lang.trim() }))
        } else {
            ui::dim(&format!("{indent}╰─"))
        };
    }
    if *in_code {
        return format!("{}{}", ui::dim("│ "), ui::info(text));
    }
    if let Some(h) = trimmed.strip_prefix("### ") {
        return each_line(&wrap(&inline_plain(h), cols, ""), ui::bold);
    }
    if let Some(h) = trimmed.strip_prefix("## ").or_else(|| trimmed.strip_prefix("# ")) {
        return each_line(&wrap(&inline_plain(h), cols, ""), ui::accent);
    }
    if trimmed.len() >= 3 && trimmed.chars().all(|c| c == '-' || c == '*' || c == '_') {
        return ui::dim(&"─".repeat(40));
    }
    if let Some(q) = trimmed.strip_prefix("> ") {
        let body = wrap(&inline_plain(q), cols.saturating_sub(cells(indent) + 2), "");
        return body.lines().map(|l| format!("{indent}{} {}", ui::dim("▏"), ui::dim(l))).collect::<Vec<_>>().join("\n");
    }
    // Tables keep their columns; the terminal wraps an over-wide row.
    if trimmed.starts_with('|') {
        return format!("{indent}{}", inline(trimmed));
    }
    if let Some(rest) = trimmed.strip_prefix("- ").or_else(|| trimmed.strip_prefix("* ")) {
        let rest = match rest.strip_prefix("[x] ").or_else(|| rest.strip_prefix("[X] ")) {
            Some(done) => format!("{} {}", ui::ok("■"), inline(done)),
            None => match rest.strip_prefix("[ ] ") {
                Some(open) => format!("{} {}", ui::dim("□"), inline(open)),
                None => inline(rest),
            },
        };
        return wrap(&format!("{indent}{} {rest}", ui::primary("•")), cols, &" ".repeat(cells(indent) + 2));
    }
    if let Some((num, rest)) = trimmed.split_once(". ") {
        if !num.is_empty() && num.len() <= 3 && num.chars().all(|c| c.is_ascii_digit()) {
            let hang = " ".repeat(cells(indent) + num.len() + 2);
            return wrap(&format!("{indent}{} {}", ui::primary(&format!("{num}.")), inline(rest)), cols, &hang);
        }
    }
    wrap(&format!("{indent}{}", inline(trimmed)), cols, indent)
}

/// Display cells of plain text.
fn cells(s: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(s)
}

fn each_line(s: &str, style: fn(&str) -> String) -> String {
    s.lines().map(style).collect::<Vec<_>>().join("\n")
}

/// Breaks `s` at spaces so no line is wider than `cols` cells, starting each
/// extra line with `hang`. ANSI escapes take no cells and are never split;
/// styles carry across the break, since the terminal keeps them over a
/// newline. Each run of text between spaces and escapes is measured once as
/// a string, so emoji presentation and joined sequences get their real
/// width. A word wider than a whole line stays whole for the terminal to
/// wrap, so long paths and URLs remain intact.
fn wrap(s: &str, cols: usize, hang: &str) -> String {
    let hang_cells = cells(hang);
    // Too narrow to wrap usefully: leave it to the terminal.
    if cols < hang_cells + 8 {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 16);
    let mut col = 0;
    let mut seen_text = false;
    // The last space after some text: its byte offset in `out`, and the column after it.
    let mut space: Option<(usize, usize)> = None;
    // A space that reached the right edge. The break is made only when another
    // word follows, so trailing spaces never leave a blank row.
    let mut edge = false;
    let mut rest = s;
    while !rest.is_empty() {
        if let Some(n) = escape_len(rest) {
            out.push_str(&rest[..n]);
            rest = &rest[n..];
            continue;
        }
        if let Some(after) = rest.strip_prefix(' ') {
            rest = after;
            if !seen_text {
                out.push(' ');
                col += 1;
            } else if edge || col + 1 > cols {
                edge = true;
                space = None;
            } else {
                out.push(' ');
                col += 1;
                space = Some((out.len() - 1, col));
            }
            continue;
        }
        let end = match rest.find([' ', '\x1b']) {
            // A lone escape byte that starts no sequence: one character.
            Some(0) => rest.chars().next().map_or(1, char::len_utf8),
            Some(i) => i,
            None => rest.len(),
        };
        let word = &rest[..end];
        let w = cells(word);
        if edge {
            out.push('\n');
            out.push_str(hang);
            col = hang_cells;
            edge = false;
        } else if col + w > cols {
            if let Some((at, after)) = space.take() {
                out.replace_range(at..at + 1, &format!("\n{hang}"));
                col = hang_cells + (col - after);
            }
        }
        seen_text = true;
        out.push_str(word);
        col += w;
        rest = &rest[end..];
    }
    out
}

/// The length of the CSI escape sequence `s` starts with, if any.
fn escape_len(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    if b.len() < 2 || b[0] != 0x1b || b[1] != b'[' {
        return None;
    }
    Some(b[2..].iter().position(|c| (0x40..=0x7e).contains(c)).map_or(b.len(), |i| i + 3))
}

/// Inline styling: `code` and **bold**.
pub fn inline(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix('`') {
            if let Some(end) = after.find('`') {
                out.push_str(&ui::info(&after[..end]));
                rest = &after[end + 1..];
                continue;
            }
        }
        if let Some(after) = rest.strip_prefix("**") {
            if let Some(end) = after.find("**") {
                out.push_str(&ui::bold(&after[..end]));
                rest = &after[end + 2..];
                continue;
            }
        }
        // Step past the whole first character: a byte offset of 1 splits `✅`.
        let first = rest.chars().next().map_or(1, char::len_utf8);
        let next = rest[first..].find(['`', '*']).map_or(rest.len(), |i| i + first);
        out.push_str(&rest[..next]);
        rest = &rest[next..];
    }
    out
}

/// Strips inline markers for text that is styled as a whole (headings, quotes).
fn inline_plain(s: &str) -> String {
    s.replace("**", "").replace('`', "")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_keeps_text_when_unstyled() {
        // Tests run without a TTY, so `line` passes text through untouched;
        // `inline` must never drop characters either way.
        assert_eq!(line("- a `b` **c**", &mut false), "- a `b` **c**");
        let s = inline("x * y `z` and **w** end*");
        for part in ["x * y ", "z", " and ", "w", " end*"] {
            assert!(s.contains(part), "{part:?} missing from {s:?}");
        }
    }

    #[test]
    fn inline_handles_lines_that_start_with_wide_characters() {
        for text in ["✅ tests pass", "→ next", "文 `code` 字", "`a`é **b**", "é"] {
            let s = inline(text);
            let plain: String = s.chars().filter(|c| !c.is_control()).collect();
            for word in text.split(['`', '*', ' ']).filter(|w| !w.is_empty()) {
                assert!(plain.contains(word), "{word:?} missing from {s:?}");
            }
        }
    }

    fn visible(s: &str) -> String {
        let mut out = String::new();
        let mut esc = false;
        for c in s.chars() {
            match (esc, c) {
                (_, '\x1b') => esc = true,
                (true, c) if ('\x40'..='\x7e').contains(&c) && c != '[' => esc = false,
                (true, _) => {}
                (false, c) => out.push(c),
            }
        }
        out
    }

    fn widest(s: &str) -> usize {
        visible(s).lines().map(cells).max().unwrap_or(0)
    }

    #[test]
    fn prose_wraps_between_words_within_the_width() {
        let text = "Permissions: symlink-aware path boundaries; clearer workspace scope when launched from your home directory.";
        for cols in [20, 39, 79] {
            let out = render(text, &mut false, cols);
            assert!(widest(&out) <= cols, "{cols}: {out:?}");
            let words: Vec<_> = visible(&out).split_whitespace().map(String::from).collect();
            assert_eq!(words, text.split_whitespace().map(String::from).collect::<Vec<_>>(), "no word split or lost");
        }
        assert_eq!(render("short line", &mut false, 79), "short line", "a fitting line is untouched");
    }

    #[test]
    fn trailing_spaces_do_not_add_a_blank_terminal_row() {
        assert_eq!(wrap("12345678 ", 8, ""), "12345678");
        assert_eq!(wrap("12345678  \x1b[0m", 8, ""), "12345678\x1b[0m", "a style reset after the spaces");
        assert_eq!(plain("one two three ", &mut false, 13), "one two three");
        assert_eq!(wrap("1234567890   next", 10, "  "), "1234567890\n  next", "no spaces carried to the next row");
    }

    #[test]
    fn list_items_and_quotes_hang_under_their_marker() {
        let out =
            visible(&render("- **Goals:** distinguish agent says done from acceptance check passed", &mut false, 30));
        let lines: Vec<_> = out.lines().collect();
        assert!(lines.len() > 1 && lines[0].starts_with("• "));
        assert!(lines[1..].iter().all(|l| l.starts_with("  ") && !l.starts_with("   ")), "{out:?}");
        let out = visible(&render("12. resume behaviour after the turn limit is reached again", &mut false, 30));
        assert!(out.lines().skip(1).all(|l| l.starts_with("    ") && !l.starts_with("     ")), "{out:?}");
        let out = visible(&render("> a quoted remark that is long enough to need a second line", &mut false, 30));
        assert!(out.lines().count() > 1 && out.lines().all(|l| l.starts_with("▏ ")), "{out:?}");
        assert!(widest(&render("> a quoted remark that is long enough to need a second line", &mut false, 30)) <= 30);
    }

    #[test]
    fn styles_survive_the_break_and_escapes_are_never_split() {
        let out = render(
            "use `cargo xtask tty-capture` then **inspect both narrow and wide terminals** carefully",
            &mut false,
            24,
        );
        assert!(widest(&out) <= 24, "{out:?}");
        for line in out.lines() {
            // Every escape that starts on a line also ends on it.
            let opened = line.matches('\x1b').count();
            let closed = line.matches("\x1b[").count();
            assert_eq!(opened, closed, "split escape in {line:?}");
        }
        assert!(
            visible(&out).contains("cargo xtask tty-capture") || visible(&out).contains("cargo xtask\ntty-capture")
        );
    }

    #[test]
    fn code_tables_rules_and_long_words_are_left_alone() {
        let mut in_code = false;
        render("```rust", &mut in_code, 20);
        let code = render("let a_long_identifier = some_function(with, many, arguments);", &mut in_code, 20);
        assert!(!code.contains('\n'), "code lines never wrap");
        render("```", &mut in_code, 20);
        let table = render("| column one | column two | column three |", &mut false, 20);
        assert!(!table.contains('\n'), "table rows keep their columns");
        let url = "see https://github.com/jadenfix/rusty/pull/142/files#diff-0123456789abcdef";
        let out = visible(&render(url, &mut false, 30));
        assert!(
            out.lines().any(|l| l == "https://github.com/jadenfix/rusty/pull/142/files#diff-0123456789abcdef"),
            "{out:?}"
        );
    }

    #[test]
    fn wide_characters_count_as_two_cells() {
        let out = render("文字 文字 文字 文字 文字 文字 文字 文字 文字 文字", &mut false, 12);
        assert!(widest(&out) <= 12, "{out:?}");
        assert!(out.lines().count() >= 4);
    }

    #[test]
    fn emoji_sequences_are_measured_as_displayed() {
        // ❤️ is a heart plus a presentation selector: two cells on screen,
        // one if the characters are counted separately.
        let out = render("❤️ ❤️ ❤️ ❤️ ab", &mut false, 12);
        assert!(widest(&out) <= 12, "{out:?}");
        assert!(visible(&out).lines().any(|l| l.trim_end().ends_with("ab")) && !visible(&out).contains("a\nb"));
        let family = "👨\u{200d}👩\u{200d}👧";
        let out = render(&format!("{family} {family} {family} {family} {family} ok"), &mut false, 12);
        assert!(widest(&out) <= 12, "{out:?}");
    }

    #[test]
    fn no_color_terminals_still_wrap_raw_markdown() {
        let mut in_code = false;
        let out = plain("- a list item long enough that it has to wrap onto another line", &mut in_code, 30);
        assert!(
            out.lines().count() > 1 && out.lines().skip(1).all(|l| l.starts_with("  ") && !l.starts_with("   ")),
            "{out:?}"
        );
        assert!(out.lines().all(|l| cells(l) <= 30));
        let out = plain("> quoted words that keep going past the edge of the pane", &mut in_code, 30);
        assert!(out.lines().all(|l| l.starts_with("> ")), "{out:?}");
        plain("```", &mut in_code, 30);
        let code = "let a_long_identifier = some_function(with, many, arguments);";
        assert_eq!(plain(code, &mut in_code, 30), code);
        plain("```", &mut in_code, 30);
        assert_eq!(
            plain("| a | b | c | d | e | f | g | h | i | j |", &mut in_code, 20),
            "| a | b | c | d | e | f | g | h | i | j |"
        );
    }
}
