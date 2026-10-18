//! Look and feel: color themes, banner fonts, the spinner. Truecolor when the
//! terminal advertises it, 256 colors otherwise, plain text for pipes and
//! NO_COLOR.

use std::io::IsTerminal;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

type Rgb = (u8, u8, u8);

pub struct Theme {
    pub name: &'static str,
    pub primary: Rgb,
    pub input: Rgb,
    pub success: Rgb,
    pub error: Rgb,
    pub accent: Rgb,
    pub info: Rgb,
    pub warn: Rgb,
    pub muted: Rgb,
    /// Banner gradient, left to right.
    pub from: Rgb,
    pub to: Rgb,
}

pub const THEMES: &[Theme] = &[
    Theme {
        name: "rust",
        input: (138, 190, 255),
        success: (145, 215, 162),
        error: (255, 157, 152),
        primary: (245, 166, 109),
        accent: (225, 182, 145),
        info: (211, 218, 226),
        warn: (244, 205, 124),
        muted: (173, 179, 186),
        from: (236, 122, 52),
        to: (160, 168, 178),
    },
    Theme {
        name: "neon",
        input: (138, 190, 255),
        success: (145, 215, 162),
        error: (255, 157, 152),
        primary: (57, 255, 136),
        accent: (255, 46, 99),
        info: (0, 229, 255),
        warn: (255, 190, 11),
        muted: (142, 152, 160),
        from: (57, 255, 136),
        to: (255, 46, 99),
    },
    Theme {
        name: "matrix",
        input: (138, 190, 255),
        success: (145, 215, 162),
        error: (255, 157, 152),
        primary: (0, 255, 65),
        accent: (255, 59, 48),
        info: (140, 255, 170),
        warn: (210, 255, 0),
        muted: (104, 164, 120),
        from: (0, 110, 40),
        to: (170, 255, 190),
    },
    Theme {
        name: "amber",
        input: (138, 190, 255),
        success: (145, 215, 162),
        error: (255, 157, 152),
        primary: (255, 176, 0),
        accent: (255, 69, 58),
        info: (255, 214, 140),
        warn: (255, 140, 0),
        muted: (178, 148, 104),
        from: (255, 110, 0),
        to: (255, 226, 140),
    },
    Theme {
        name: "ice",
        input: (138, 190, 255),
        success: (145, 215, 162),
        error: (255, 157, 152),
        primary: (110, 231, 255),
        accent: (255, 85, 170),
        info: (180, 160, 255),
        warn: (255, 204, 102),
        muted: (138, 158, 180),
        from: (60, 140, 255),
        to: (230, 120, 255),
    },
    Theme {
        name: "mono",
        input: (235, 235, 235),
        success: (235, 235, 235),
        error: (255, 255, 255),
        primary: (235, 235, 235),
        accent: (255, 255, 255),
        info: (200, 200, 200),
        warn: (220, 220, 220),
        muted: (164, 164, 164),
        from: (120, 120, 120),
        to: (255, 255, 255),
    },
    Theme {
        name: "paper",
        primary: (140, 65, 15),
        input: (31, 85, 153),
        success: (35, 99, 59),
        error: (160, 45, 50),
        accent: (110, 55, 135),
        info: (36, 40, 45),
        warn: (104, 85, 0),
        muted: (88, 94, 101),
        from: (140, 65, 15),
        to: (88, 94, 101),
    },
];

static THEME: AtomicU8 = AtomicU8::new(0);
static FONT: AtomicU8 = AtomicU8::new(0);

pub fn set_theme(name: &str) -> bool {
    match THEMES.iter().position(|t| t.name == name) {
        Some(i) => {
            THEME.store(i as u8, Ordering::Relaxed);
            true
        }
        None => false,
    }
}

pub fn theme() -> &'static Theme {
    &THEMES[THEME.load(Ordering::Relaxed) as usize % THEMES.len()]
}

/// 0 = no color, 1 = 256 colors, 2 = truecolor.
fn level() -> u8 {
    static L: OnceLock<u8> = OnceLock::new();
    *L.get_or_init(|| {
        if !std::io::stdout().is_terminal() || std::env::var_os("NO_COLOR").is_some() {
            0
        } else if std::env::var("COLORTERM").is_ok_and(|v| v.contains("truecolor") || v.contains("24bit")) {
            2
        } else {
            1
        }
    })
}

pub fn tty() -> bool {
    level() > 0
}

/// Animations are on for real terminals unless RUSTY_NO_ANIM is set.
pub fn animate() -> bool {
    tty() && std::env::var_os("RUSTY_NO_ANIM").is_none()
}

/// The closest of the 256-colour palette's cube (steps 0, 95, 135, ..., 255,
/// not evenly spaced) and grey ramp (8, 18, ..., 238), the way tmux picks it.
fn ansi256((r, g, b): Rgb) -> u8 {
    let step = |v: u8| {
        if v < 48 {
            0
        } else if v < 115 {
            1
        } else {
            (v - 35) / 40
        }
    };
    let level = |i: u8| if i == 0 { 0 } else { 55 + 40 * i as i32 };
    let dist = |x: i32, y: i32, z: i32| (x - r as i32).pow(2) + (y - g as i32).pow(2) + (z - b as i32).pow(2);
    let (i, j, k) = (step(r), step(g), step(b));
    let grey = ((r as u16 + g as u16 + b as u16) / 3).saturating_sub(3).min(238) / 10;
    let v = 8 + 10 * grey as i32;
    if dist(v, v, v) < dist(level(i), level(j), level(k)) {
        232 + grey as u8
    } else {
        16 + 36 * i + 6 * j + k
    }
}

fn fg((r, g, b): Rgb, s: &str, bold: bool) -> String {
    let weight = if bold { "1;" } else { "" };
    match level() {
        0 => s.to_string(),
        1 => format!("\x1b[{weight}38;5;{}m{s}\x1b[0m", ansi256((r, g, b))),
        _ => format!("\x1b[{weight}38;2;{r};{g};{b}m{s}\x1b[0m"),
    }
}

/// User input has one semantic colour, independent of the rust/error accents.
/// Reset first so a redraw cannot inherit a footer's dim or warning style.
pub fn user(s: &str) -> String {
    if level() == 0 {
        s.to_string()
    } else {
        format!("\x1b[0m{}", fg(theme().input, s, true))
    }
}

pub fn primary(s: &str) -> String {
    fg(theme().primary, s, false)
}
pub fn ok(s: &str) -> String {
    fg(theme().success, s, false)
}
pub fn err(s: &str) -> String {
    fg(theme().error, s, false)
}
pub fn accent(s: &str) -> String {
    fg(theme().accent, s, true)
}
pub fn info(s: &str) -> String {
    fg(theme().info, s, false)
}
pub fn warn(s: &str) -> String {
    fg(theme().warn, s, false)
}
pub fn dim(s: &str) -> String {
    fg(theme().muted, s, false)
}
pub fn bold(s: &str) -> String {
    if level() == 0 {
        s.to_string()
    } else {
        format!("\x1b[1m{s}\x1b[0m")
    }
}

fn lerp(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t) as u8;
    (m(a.0, b.0), m(a.1, b.1), m(a.2, b.2))
}

/// Paints `s` with the theme gradient across its characters.
pub fn gradient(s: &str) -> String {
    let n = s.chars().count().max(2) - 1;
    let t = theme();
    s.chars().enumerate().map(|(i, c)| fg(lerp(t.from, t.to, i as f32 / n as f32), &c.to_string(), true)).collect()
}

/// Terminal width: the tty's own size, then $COLUMNS, then 100.
pub fn width() -> usize {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) } == 0 && ws.ws_col > 0 {
        return (ws.ws_col as usize).clamp(1, 300);
    }
    std::env::var("COLUMNS").ok().and_then(|c| c.parse().ok()).unwrap_or(100).clamp(1, 300)
}

pub fn height() -> usize {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) } == 0 && ws.ws_row > 0 {
        return ws.ws_row as usize;
    }
    24
}

// ------------------------------------------------------------------ banner

pub const FONTS: &[&str] = &["compact", "rust", "block", "thin", "classic"];

pub fn set_font(name: &str) -> bool {
    match FONTS.iter().position(|f| *f == name) {
        Some(i) => {
            FONT.store(i as u8, Ordering::Relaxed);
            true
        }
        None => false,
    }
}

pub fn font() -> &'static str {
    FONTS[FONT.load(Ordering::Relaxed) as usize % FONTS.len()]
}

fn art_for(font: &str) -> &'static [&'static str] {
    match font {
        "thin" => &["┬─┐┬ ┬┌─┐┌┬┐┬ ┬", "├┬┘│ │└─┐ │ └┬┘", "┴└─└─┘└─┘ ┴  ┴ "],
        "classic" => &[
            " ___ _   _ ___ _______   __",
            "| _ \\ | | / __|_   _\\ \\ / /",
            "|   / |_| \\__ \\ | |  \\ V / ",
            "|_|_\\\\___/|___/ |_|   |_|  ",
        ],
        _ => &[
            "██████╗ ██╗   ██╗███████╗████████╗██╗   ██╗",
            "██╔══██╗██║   ██║██╔════╝╚══██╔══╝╚██╗ ██╔╝",
            "██████╔╝██║   ██║███████╗   ██║    ╚████╔╝ ",
            "██╔══██╗██║   ██║╚════██║   ██║     ╚██╔╝  ",
            "██║  ██║╚██████╔╝███████║   ██║      ██║   ",
            "╚═╝  ╚═╝ ╚═════╝ ╚══════╝   ╚═╝      ╚═╝   ",
        ],
    }
}

/// Slatted pixel letters for the default banner, 9 rows tall.
const SLAT_GLYPHS: [[&str; 9]; 5] = [
    [
        "###########.",
        "############",
        "###......###",
        "###......###",
        "###########.",
        "##########..",
        "###....###..",
        "###.....###.",
        "###......###",
    ],
    [
        "###......###",
        "###......###",
        "###......###",
        "###......###",
        "###......###",
        "###......###",
        "###......###",
        "############",
        ".##########.",
    ],
    [
        ".###########",
        "############",
        "###.........",
        "###########.",
        ".###########",
        ".........###",
        ".........###",
        "############",
        "###########.",
    ],
    [
        "############",
        "############",
        "....####....",
        "....####....",
        "....####....",
        "....####....",
        "....####....",
        "....####....",
        "....####....",
    ],
    [
        "###......###",
        ".###....###.",
        "..###..###..",
        "...######...",
        "....####....",
        "....####....",
        "....####....",
        "....####....",
        "....####....",
    ],
];

/// Cheap deterministic noise in 0..1 for the rust texture.
fn noise(x: i64, y: i64) -> f32 {
    let mut h = (x.wrapping_mul(374_761_393) ^ y.wrapping_mul(668_265_263)) as u64;
    h = (h ^ (h >> 13)).wrapping_mul(1_274_126_177);
    ((h ^ (h >> 16)) & 0xffff) as f32 / 65535.0
}

/// Weathered metal: mostly rust tones with patches of bare steel.
fn rust_color(x: i64, y: i64) -> Rgb {
    let v = 0.5 * noise(x / 5, y / 2) + 0.3 * noise(x / 2, y) + 0.2 * noise(x, y);
    match v {
        v if v < 0.22 => (118, 120, 124),
        v if v < 0.32 => (168, 170, 174),
        v if v < 0.55 => (214, 104, 44),
        v if v < 0.72 => (238, 132, 58),
        v if v < 0.86 => (156, 64, 30),
        v if v < 0.94 => (112, 44, 22),
        _ => (232, 196, 160),
    }
}

/// The RUSTY wordmark as slats of upper-half blocks with an italic lean.
fn slat_banner() -> Vec<String> {
    let rows = SLAT_GLYPHS[0].len();
    (0..rows)
        .map(|r| {
            let mut line = " ".repeat(rows - 1 - r);
            let mut x = 0i64;
            for (i, glyph) in SLAT_GLYPHS.iter().enumerate() {
                if i > 0 {
                    line.push_str("  ");
                    x += 2;
                }
                for c in glyph[r].chars() {
                    if c == '#' {
                        line.push_str(&match level() {
                            0 => "▀".to_string(),
                            _ => fg(rust_color(x, r as i64), "▀", false),
                        });
                    } else {
                        line.push(' ');
                    }
                    x += 1;
                }
            }
            // Pad the right edge so the leaning block stays rectangular.
            line.push_str(&" ".repeat(r));
            line
        })
        .collect()
}

// ------------------------------------------------------------ mission log

/// (year, month, day) from days since 1970-01-01 (Howard Hinnant's algorithm).
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn unix_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Lines for the mission log under the banner, in the voice of the
/// people building with agents right now.
const DISPATCHES: &[&str] = &[
    "ship it, then prove it.",
    "the eval is the spec.",
    "context is the new compute.",
    "tokens are cheap. regressions aren't.",
    "default to action. verify after.",
    "small diffs compound.",
    "make something agents want.",
    "do things that don't scale. then automate them.",
    "the bitter lesson applies to your codebase too.",
    "your moat is your test suite.",
    "one engineer, ten agents, zero flaky tests.",
    "if it isn't in the eval, it didn't happen.",
    "feel the agi. then run the tests.",
    "cracked engineers read the error message twice.",
    "we're so back. (once ci is green.)",
    "the scaling laws hold. so does the linter.",
    "lock in. small commits. no force pushes.",
    "agents don't get tired. tests don't lie.",
];

/// Today's mission line and dispatch; the same all day.
fn mission() -> (String, String) {
    let t = unix_now();
    let days = t.div_euclid(86_400);
    let (y, m, d) = civil(days);
    let day_of_year = days - days_from_civil(y, 1, 1) + 1;
    let to_next = days_from_civil(y + 1, 1, 1) - days;
    let line = format!("◉ {y}.{m:02}.{d:02} · day {day_of_year} · T−{to_next}d to {}", y + 1);
    let dispatch = DISPATCHES[(days as usize).wrapping_mul(7) % DISPATCHES.len()];
    (line, dispatch.to_string())
}

/// Days since 1970-01-01 for a date (the inverse of `civil`).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (i64::from(m) + 9) % 12;
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// A sparse, dim starfield that changes once a day.
fn starfield(width: usize) -> String {
    let day = unix_now().div_euclid(86_400);
    (0..width)
        .map(|x| {
            let n = noise(x as i64 * 7 + 3, day);
            match n {
                n if n > 0.985 => "✦",
                n if n > 0.965 => "⋆",
                n if n > 0.93 => "·",
                _ => " ",
            }
        })
        .collect()
}

pub struct BannerInfo<'a> {
    pub model: &'a str,
    /// Who serves the model: compatible, anthropic or openai.
    pub provider: &'a str,
    pub exec: &'a str,
    pub cwd: &'a str,
    pub perms: &'a str,
    pub agents: &'a str,
    pub view: &'a str,
    pub keys: usize,
    /// Live infrastructure target, empty when none was detected.
    pub target: &'a str,
}

/// Clip UI chrome by display cells; never fill the terminal wrap column.
pub fn fit(s: &str, cells: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    let mut used = 0;
    s.chars()
        .filter(|c| !c.is_control())
        .take_while(|c| {
            used += c.width().unwrap_or(0);
            used <= cells
        })
        .collect()
}

/// A short separator survives reflow down to 24 columns without becoming a
/// stack of rules. It stays outside the native multiline line editor.
pub fn input_rule() {
    if tty() {
        println!("{}", dim(&"─".repeat(width().saturating_sub(1).min(20))));
    }
}

pub fn banner(b: &BannerInfo) {
    if font() == "compact" {
        let cells = width().saturating_sub(1);
        println!(
            "{}",
            primary(&fit(
                &format!(
                    "rusty / {}",
                    std::path::Path::new(b.cwd).file_name().and_then(|s| s.to_str()).unwrap_or(b.cwd)
                ),
                cells
            ))
        );
        println!("{}", bold(&fit(&format!("provider {}", b.provider), cells)));
        println!("{}", info(&fit(&format!("model {}", b.model), cells)));
        // Permissions remain visible even in narrow splits.
        let mode = fit(&format!("permissions {} · {}", b.perms, b.exec), cells);
        println!("{}", if b.perms == "yolo" { warn(&mode) } else { info(&mode) });
        if width() >= 60 {
            println!("{}", dim(&fit(&format!("agents {} · view {} · /help", b.agents, b.view), cells)));
        }
        if !b.target.is_empty() {
            // Infrastructure targets must not disappear behind truncation.
            println!("{}", warn(&format!("target {}", b.target)));
        }
        println!();
        return;
    }
    let pause = |ms| {
        if animate() {
            std::thread::sleep(Duration::from_millis(ms));
        }
    };
    let slats = font() == "rust" && width() >= 80;
    let art: Vec<String> = if slats {
        slat_banner()
    } else {
        let f = if font() == "rust" { "thin" } else { font() };
        art_for(f).iter().map(|l| gradient(l)).collect()
    };
    let art_w = if slats { 76 } else { art_for(font()).iter().map(|l| l.chars().count()).max().unwrap_or(40) };
    println!();
    println!("  {}", dim(&starfield(art_w)));
    for line in &art {
        println!("  {line}");
        pause(24);
    }
    println!("  {}", dim(&starfield(art_w).chars().rev().collect::<String>()));
    let (mission_line, dispatch) = mission();
    println!("  {}  {}", primary(&mission_line), dim(&format!("// {dispatch}")));
    let rule = "─".repeat(art_w.min(width().saturating_sub(4)));
    let sep = dim(" · ");
    println!("  {}", dim(&rule));
    let row = |k: &str, v: String| println!("  {} {}", dim(&format!("{k:<6}")), v);
    // The provider tag only when it fits on the row.
    let fits = 2 + 7 + b.model.chars().count() + 3 + b.provider.chars().count() <= width();
    row("model", if fits { format!("{}{sep}{}", info(b.model), dim(b.provider)) } else { info(b.model) });
    row("dir", b.cwd.to_string());
    let perms = if b.perms == "yolo" { warn(b.perms) } else { b.perms.to_string() };
    let mut mode = format!("{}{sep}permissions {perms}{sep}agents {}{sep}view {}", bold(b.exec), b.agents, b.view);
    if b.keys > 1 {
        mode.push_str(&format!("{sep}{}", dim(&format!("{} keys", b.keys))));
    }
    row("mode", mode);
    if !b.target.is_empty() {
        row("target", b.target.to_string());
    }
    println!("  {}", dim(&rule));
    println!("  {}", dim("/help · ctrl-c stops a turn · ctrl-d quits"));
    println!();
}

// ----------------------------------------------------------------- spinner

/// Small xorshift seeded from the clock; good enough for picking a verb.
pub fn rand(n: usize) -> usize {
    use std::sync::atomic::AtomicU64;
    static STATE: AtomicU64 = AtomicU64::new(0);
    let mut x = STATE.load(Ordering::Relaxed);
    if x == 0 {
        x = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(7)
            | 1;
    }
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    STATE.store(x, Ordering::Relaxed);
    (x % n.max(1) as u64) as usize
}

pub fn human(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1e6)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1e3)
    } else {
        n.to_string()
    }
}

/// Cuts `s` to at most `max` bytes on a char boundary.
pub fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours_map_to_the_nearest_palette_entry() {
        // Greys go to the grey ramp, not the coarser cube.
        assert_eq!(ansi256((158, 154, 150)), 247);
        assert_eq!(ansi256((120, 120, 120)), 243);
        // Exact cube colours and the corners stay put.
        assert_eq!(ansi256((255, 175, 0)), 214);
        assert_eq!(ansi256((0, 0, 0)), 16);
        assert_eq!(ansi256((255, 255, 255)), 231);
        // Every theme keeps its colours apart once mapped.
        for t in THEMES {
            let roles = [t.primary, t.accent, t.info, t.warn, t.muted].map(ansi256);
            for (i, a) in roles.iter().enumerate() {
                assert!(roles[i + 1..].iter().all(|b| b != a), "{}: {roles:?}", t.name);
            }
        }
    }

    #[test]
    fn baseline_skins_have_readable_text_in_truecolor_and_indexed_terminals() {
        fn luminance(rgb: Rgb) -> f64 {
            let [r, g, b] = [rgb.0, rgb.1, rgb.2].map(|v| {
                let v = f64::from(v) / 255.0;
                if v <= 0.04045 {
                    v / 12.92
                } else {
                    ((v + 0.055) / 1.055).powf(2.4)
                }
            });
            0.2126 * r + 0.7152 * g + 0.0722 * b
        }
        fn indexed(rgb: Rgb) -> Rgb {
            let i = ansi256(rgb);
            if i >= 232 {
                let v = 8 + (i - 232) * 10;
                (v, v, v)
            } else {
                let i = i - 16;
                let step = |v| if v == 0 { 0 } else { 55 + 40 * v };
                (step(i / 36), step(i / 6 % 6), step(i % 6))
            }
        }
        for t in THEMES.iter().filter(|t| matches!(t.name, "rust" | "paper" | "mono")) {
            let bg = luminance(if t.name == "paper" { (250, 247, 240) } else { (22, 25, 29) });
            for role in [t.primary, t.input, t.success, t.error, t.info, t.warn, t.muted] {
                for rgb in [role, indexed(role)] {
                    let fg = luminance(rgb);
                    let contrast = (fg.max(bg) + 0.05) / (fg.min(bg) + 0.05);
                    assert!(contrast >= 4.5, "{} {rgb:?}: {contrast}", t.name);
                }
            }
        }
    }

    #[test]
    fn chrome_clips_wide_unicode_and_removes_controls() {
        use unicode_width::UnicodeWidthStr;
        for width in 0..80 {
            let clipped = fit("repo/文件/🦀/e\u{301}\n\r\x1b", width);
            assert!(clipped.width() <= width);
            assert!(!clipped.chars().any(char::is_control));
        }
    }

    #[test]
    fn calendar_round_trips() {
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(20_731), (2026, 10, 5));
        for days in [0, 20_731, 20_819, 11_016, -1] {
            let (y, m, d) = civil(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
        assert_eq!(days_from_civil(2027, 1, 1) - 20_731, 88);
    }

    #[test]
    fn slat_banner_rows_line_up() {
        let rows = slat_banner();
        assert_eq!(rows.len(), 9);
        let widths: Vec<usize> = rows.iter().map(|r| r.chars().count()).collect();
        assert!(widths.iter().all(|w| *w == widths[0]), "{widths:?}");
    }
}
