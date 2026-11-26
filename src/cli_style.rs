//! Terminal output formatting for the `grovefs` command line.
//!
//! Every human-facing line the CLI prints goes through this module, so the
//! tool has ONE visual language: a small set of semantic markers, a restrained
//! palette, and tables that are laid out from the terminal's real width.
//!
//! The contract is deliberately conservative:
//!
//! * **Pipes stay plain.** Colour is emitted only on a colour-capable TTY
//!   (`NO_COLOR`, `CLICOLOR=0`, `TERM=dumb` and a non-TTY all turn it off).
//!   Symbols fall back to ASCII when the locale cannot be trusted to carry
//!   UTF-8. A redirected log therefore reads exactly like a 1980s tool, and its
//!   bytes never contain escape sequences.
//! * **Width is discovered, never guessed.** `COLUMNS` wins, then the kernel's
//!   `TIOCGWINSZ`, then an unlimited width for pipes. Tables shrink columns in
//!   priority order and wrap the trailing free-text column instead of
//!   overflowing or silently clipping it.
//! * **Display width, not byte count.** CJK text and emoji are measured in
//!   cells (two for a wide glyph), so a Chinese label cannot push a table past
//!   the right edge.
//! * **Decoration is limited to structure.** Headings, rules, labels and
//!   status markers. Prose keeps its own words and is only wrapped.

use std::io::IsTerminal;
use std::sync::OnceLock;

// ---------------------------------------------------------------------------
// Environment / terminal capabilities
// ---------------------------------------------------------------------------

/// Per-process terminal capabilities, resolved once.
#[derive(Debug, Clone, Copy)]
struct Caps {
    color: bool,
    unicode: bool,
}

fn caps() -> &'static Caps {
    static CAPS: OnceLock<Caps> = OnceLock::new();
    CAPS.get_or_init(|| {
        let env = |k: &str| std::env::var_os(k);
        let no_color = env("NO_COLOR").is_some_and(|v| !v.is_empty());
        let clicolor_off = env("CLICOLOR").is_some_and(|v| v == "0");
        let dumb = env("TERM").is_some_and(|v| v == "dumb");
        let is_tty = std::io::stdout().is_terminal();
        let forced = env("FORCE_COLOR").is_some_and(|v| !v.is_empty() && v != "0");
        // An explicit FORCE_COLOR is the operator overriding detection (CI
        // snapshots, `TERM=dumb` in a wrapper); NO_COLOR still wins.
        let color = !no_color && !clicolor_off && (forced || (!dumb && is_tty));

        // A UTF-8 locale is the reliable signal. Non-TTY writers are left on
        // ASCII so redirected logs stay portable; an explicit GROVEFS_ASCII=1
        // forces the same fallback on a capable terminal.
        let locale_utf8 = ["LC_ALL", "LC_CTYPE", "LANG"]
            .iter()
            .filter_map(|k| std::env::var(k).ok())
            .any(|v| {
                let v = v.to_ascii_lowercase();
                v.contains("utf-8") || v.contains("utf8")
            });
        let ascii = env("GROVEFS_ASCII").is_some_and(|v| !v.is_empty() && v != "0");
        let unicode = !ascii && locale_utf8 && (is_tty || forced);
        Caps { color, unicode }
    })
}

/// Whether ANSI styling is in effect for this process.
pub fn color_enabled() -> bool {
    caps().color
}

/// Whether the glyph set includes box drawing / check marks.
pub fn unicode_enabled() -> bool {
    caps().unicode
}

fn glyph(unicode: &'static str, ascii: &'static str) -> &'static str {
    if caps().unicode {
        unicode
    } else {
        ascii
    }
}

/// Terminal width in cells. `usize::MAX` means "unbounded" (a pipe or a
/// redirected file), where wrapping would corrupt rather than help.
pub fn width() -> usize {
    if let Some(n) = std::env::var("GROVEFS_COLUMNS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|n| *n >= 20)
    {
        return n;
    }
    if let Some(n) = std::env::var("COLUMNS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|n| *n >= 20)
    {
        return n;
    }
    tty_width().unwrap_or(usize::MAX)
}

#[cfg(all(unix, feature = "fuse-bridge"))]
fn tty_width() -> Option<usize> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws as *mut _) };
    if rc == 0 && ws.ws_col > 0 {
        Some(ws.ws_col as usize)
    } else {
        None
    }
}

#[cfg(not(all(unix, feature = "fuse-bridge")))]
fn tty_width() -> Option<usize> {
    None
}

// ---------------------------------------------------------------------------
// ANSI primitives
// ---------------------------------------------------------------------------

fn paint(code: &str, text: &str) -> String {
    if !caps().color {
        return text.to_string();
    }
    format!("\x1b[{code}m{text}\x1b[0m")
}

/// Bold, for headings and labels that need to lead.
pub fn strong(text: &str) -> String {
    paint("1", text)
}

/// Dim, for labels, rules, and supporting facts.
pub fn muted(text: &str) -> String {
    paint("2", text)
}

/// Cyan accent, for identifiers and paths that should be scannable.
pub fn accent(text: &str) -> String {
    paint("36", text)
}

/// Green, for a completed operation.
pub fn success_text(text: &str) -> String {
    paint("32", text)
}

/// Yellow, for a degraded-but-continuing condition.
pub fn warn_text(text: &str) -> String {
    paint("33", text)
}

/// Red, for a failed operation.
pub fn error_text(text: &str) -> String {
    paint("31", text)
}

// ---------------------------------------------------------------------------
// Width-aware text helpers
// ---------------------------------------------------------------------------

/// Strip CSI escape sequences so measurements never count colour bytes.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                // CSI parameters end at a byte in 0x40..=0x7e.
                for n in chars.by_ref() {
                    if ('@'..='~').contains(&n) {
                        break;
                    }
                }
                continue;
            }
        }
        out.push(c);
    }
    out
}

/// Width of a character in terminal cells (East Asian wide = 2, combining = 0).
fn char_width(c: char) -> usize {
    let u = c as u32;
    // Combining marks, variation selectors and zero-width joiners.
    if matches!(u, 0x0300..=0x036F | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF
        | 0x20D0..=0x20FF | 0xFE00..=0xFE0F | 0xFE20..=0xFE2F
        | 0x200B..=0x200F | 0x2060..=0x206F)
    {
        return 0;
    }
    // East Asian Wide / Fullwidth blocks.
    let wide = matches!(u,
        0x1100..=0x115F
        | 0x2E80..=0x303E
        | 0x3041..=0x33FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF
        | 0xA960..=0xA97F
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE10..=0xFE19
        | 0xFE30..=0xFE6F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x1F300..=0x1F64F
        | 0x1F900..=0x1F9FF
        | 0x20000..=0x3FFFD
    );
    if wide {
        2
    } else {
        1
    }
}

/// Terminal cells occupied by `s` (escape sequences are ignored).
pub fn display_width(s: &str) -> usize {
    strip_ansi(s).chars().map(char_width).sum()
}

/// A count with its noun in the right number: `plural(1, "entry", "entries")`
/// is `"1 entry"`, every other count is `"0 entries"` / `"3 entries"`.
pub fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// Truncate to `max` cells, appending an ellipsis when text was removed.
pub fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let plain = strip_ansi(s);
    if display_width(&plain) <= max {
        return plain;
    }
    let dots = if caps().unicode { "…" } else { "..." };
    let dots_w = display_width(dots);
    if max <= dots_w {
        return dots.chars().take(max).collect();
    }
    let budget = max - dots_w;
    let mut out = String::new();
    let mut used = 0;
    for c in plain.chars() {
        let w = char_width(c);
        if used + w > budget {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push_str(dots);
    out
}

/// Pad `s` to exactly `width` cells (ANSI codes do not count).
pub fn pad(s: &str, width: usize, align: Align) -> String {
    let w = display_width(s);
    if w >= width {
        return s.to_string();
    }
    let fill = " ".repeat(width - w);
    match align {
        Align::Left => format!("{s}{fill}"),
        Align::Right => format!("{fill}{s}"),
    }
}

/// Greedy word wrap at `width` cells; never emits a line wider than `width`.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    if width == usize::MAX || display_width(text) <= width {
        return vec![text.to_string()];
    }
    let mut lines = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        let word_w = display_width(word);
        let cur_w = display_width(&cur);
        if cur.is_empty() {
            if word_w <= width {
                cur.push_str(word);
            } else {
                // A single token longer than the line (a path, a key): hard
                // split so it still fits.
                let mut rest = word;
                while display_width(rest) > width {
                    let mut cut = 0;
                    let mut used = 0;
                    for c in rest.chars() {
                        let w = char_width(c);
                        if used + w > width {
                            break;
                        }
                        used += w;
                        cut += c.len_utf8();
                    }
                    lines.push(rest[..cut].to_string());
                    rest = &rest[cut..];
                }
                cur.push_str(rest);
            }
        } else if cur_w + 1 + word_w <= width {
            cur.push(' ');
            cur.push_str(word);
        } else {
            lines.push(std::mem::take(&mut cur));
            cur = word.to_string();
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

// ---------------------------------------------------------------------------
// Structural output
// ---------------------------------------------------------------------------

fn emit_stdout(s: &str) {
    println!("{s}");
}

fn emit_stderr(s: &str) {
    eprintln!("{s}");
}

/// Opening line of a command's report: bold title, dim rule underneath.
pub fn heading(text: &str) {
    let w = width();
    let lines = if w == usize::MAX {
        vec![text.to_string()]
    } else {
        wrap(text, w)
    };
    for line in &lines {
        emit_stdout(&strong(line));
    }
    let len = lines
        .iter()
        .map(|l| display_width(l))
        .max()
        .unwrap_or(8)
        .max(8);
    let rule: String = std::iter::repeat(glyph("─", "-")).take(len).collect();
    emit_stdout(&muted(&rule));
}

/// A titled group inside a longer report.
pub fn section(text: &str) {
    let w = width();
    if w == usize::MAX {
        emit_stdout(&strong(text));
        return;
    }
    for line in wrap(text, w) {
        emit_stdout(&strong(&line));
    }
}

/// Blank separator line.
pub fn blank() {
    emit_stdout("");
}

/// Neutral informational line with a bullet marker.
pub fn info(msg: &str) {
    emit_stdout(&wrapped_with_prefix(&muted(glyph("•", "-")), msg, 2));
}

/// Completed operation.
pub fn success(msg: &str) {
    emit_stdout(&wrapped_with_prefix(&success_text(glyph("✓", "+")), msg, 2));
}

/// Warning on stderr; the process keeps going. The literal `warning:` prefix
/// stays in the text (colour is not enough for a redirected log, and existing
/// operators grep for it).
pub fn warn(msg: &str) {
    let msg = msg.strip_prefix("warning: ").unwrap_or(msg);
    emit_stderr(&wrapped_with_prefix(&warn_text("warning:"), msg, 9));
}

/// Fatal/refused operation on stderr, with the same greppable `error:` prefix.
pub fn error(msg: &str) {
    let msg = msg.strip_prefix("error: ").unwrap_or(msg);
    emit_stderr(&wrapped_with_prefix(&error_text("error:"), msg, 7));
}

/// Supporting note on stderr. Keeps the greppable `note:` prefix that operators
/// and tests already use; only the marker is toned down.
pub fn note(msg: &str) {
    let msg = msg.strip_prefix("note: ").unwrap_or(msg);
    let prefix = muted("note:");
    emit_stderr(&wrapped_with_prefix(&prefix, msg, 6));
}

/// Actionable follow-up on stderr.
pub fn hint(msg: &str) {
    let prefix = muted(glyph("→", ">"));
    emit_stderr(&wrapped_with_prefix(&prefix, msg, 2));
}

fn wrapped_with_prefix(prefix: &str, msg: &str, hang: usize) -> String {
    let w = width();
    if w == usize::MAX {
        return format!("{prefix} {msg}");
    }
    let prefix_w = display_width(prefix);
    let hang_indent = " ".repeat(hang.min(w.saturating_sub(1)));
    let lines = wrap(msg, w.saturating_sub(prefix_w + 1).max(8));
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate() {
        if i == 0 {
            out.push_str(prefix);
            out.push(' ');
            out.push_str(line);
        } else {
            out.push('\n');
            out.push_str(&hang_indent);
            out.push_str(line);
        }
    }
    out
}

/// One indented bullet under a heading.
pub fn item(msg: &str) {
    let w = width();
    let pad = "  ";
    let prefix = format!("{pad}{} ", muted(glyph("•", "-")));
    if w == usize::MAX {
        emit_stdout(&format!("{prefix}{msg}"));
        return;
    }
    let lines = wrap(msg, w.saturating_sub(display_width(&prefix)).max(8));
    for (i, line) in lines.iter().enumerate() {
        if i == 0 {
            emit_stdout(&format!("{prefix}{line}"));
        } else {
            emit_stdout(&format!("{}{line}", " ".repeat(display_width(&prefix))));
        }
    }
}

/// A key/value row with a shared label column.
pub fn kv_block(indent: usize, pairs: &[(&str, String)]) {
    let pad = " ".repeat(indent);
    // The shared column must fit the colon too, or a long label ("expires:")
    // would be pushed onto its own line while its neighbours stay inline.
    let key_w = pairs
        .iter()
        .map(|(k, _)| display_width(k) + 1)
        .max()
        .unwrap_or(0)
        .min(22);
    let w = width();
    for (key, value) in pairs {
        let label = muted(&format!("{key}:"));
        let label_w = display_width(key) + 1;
        let value_w = if w == usize::MAX {
            usize::MAX
        } else {
            w.saturating_sub(indent + key_w + 2).max(8)
        };
        // A secret, key id or path is a single unbreakable token: splitting it
        // across lines makes it wrong to copy. Put it on its own lightly
        // indented line instead of hard-wrapping through the middle.
        let single_token = !value.chars().any(char::is_whitespace);
        if single_token && display_width(value) > value_w && !value.is_empty() {
            emit_stdout(&format!("{pad}{label}"));
            for line in value.lines() {
                emit_stdout(&format!("{pad}  {line}"));
            }
            continue;
        }
        let lines = wrap(value, value_w);
        if label_w > key_w {
            // A label wider than the shared column: give the value its own
            // indented block instead of letting it collide with the label.
            emit_stdout(&format!("{pad}{label}"));
            for line in &lines {
                emit_stdout(&format!("{pad}{}{line}", " ".repeat(key_w + 2)));
            }
            continue;
        }
        for (i, line) in lines.iter().enumerate() {
            if i == 0 {
                emit_stdout(&format!(
                    "{pad}{label}{} {line}",
                    " ".repeat(key_w.saturating_sub(label_w))
                ));
            } else {
                emit_stdout(&format!("{pad}{}{line}", " ".repeat(key_w + 1)));
            }
        }
    }
}

/// Render a stored textual view/definition listing.
///
/// Lines already shaped as prose (they contain a parenthetical explanation)
/// are wrapped with a hanging indent; a line that begins with `note:` goes to
/// the note channel. No wording is rewritten here.
pub fn outline(text: &str) {
    let text = text.trim_end();
    if text.is_empty() {
        blank();
        return;
    }
    if let Some(rest) = text.strip_prefix("note: ") {
        note(rest);
        return;
    }
    let w = width();
    if w == usize::MAX {
        emit_stdout(text);
        return;
    }
    let lines = wrap(text, w.saturating_sub(2).max(8));
    for (i, line) in lines.iter().enumerate() {
        if i == 0 {
            emit_stdout(&format!("{} {line}", muted(glyph("›", "-"))));
        } else {
            emit_stdout(&format!("  {line}"));
        }
    }
}

// ---------------------------------------------------------------------------
// Tables
// ---------------------------------------------------------------------------

/// Column alignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
}

/// One table column.
///
/// `priority` drives the narrow-terminal fallback: columns labelled `0` are
/// never dropped, `1` shrinks first, and `3` is dropped before required
/// columns lose their last character. The final column may be wrapped instead
/// of truncated when it carries free text.
#[derive(Debug, Clone)]
pub struct Column {
    pub header: &'static str,
    pub align: Align,
    pub min: usize,
    pub priority: u8,
    pub max: Option<usize>,
}

impl Column {
    pub fn left(header: &'static str) -> Self {
        Self {
            header,
            align: Align::Left,
            min: 3,
            priority: 0,
            max: None,
        }
    }

    pub fn right(header: &'static str) -> Self {
        Self {
            header,
            align: Align::Right,
            min: 3,
            priority: 0,
            max: None,
        }
    }

    pub fn min(mut self, n: usize) -> Self {
        self.min = n;
        self
    }

    pub fn max(mut self, n: usize) -> Self {
        self.max = Some(n);
        self
    }

    /// Shrink order for narrow terminals: higher values give way first.
    pub fn priority(mut self, p: u8) -> Self {
        self.priority = p;
        self
    }
}

/// One cell; `tone` colours the text without affecting layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Normal,
    Muted,
    Strong,
    Accent,
    Success,
    Warn,
    Error,
}

#[derive(Debug, Clone)]
pub struct Cell {
    text: String,
    tone: Tone,
}

impl Cell {
    pub fn new(text: impl AsRef<str>) -> Self {
        Self {
            text: text.as_ref().to_string(),
            tone: Tone::Normal,
        }
    }

    pub fn muted(text: impl AsRef<str>) -> Self {
        Self {
            text: text.as_ref().to_string(),
            tone: Tone::Muted,
        }
    }

    pub fn strong(text: impl AsRef<str>) -> Self {
        Self {
            text: text.as_ref().to_string(),
            tone: Tone::Strong,
        }
    }

    pub fn accent(text: impl AsRef<str>) -> Self {
        Self {
            text: text.as_ref().to_string(),
            tone: Tone::Accent,
        }
    }

    pub fn tone(mut self, tone: Tone) -> Self {
        self.tone = tone;
        self
    }

    fn painted(&self, width: usize, align: Align) -> String {
        let text = truncate(&self.text, width);
        let padded = pad(&text, width, align);
        // Colour only the content, then re-add the padding outside the escape
        // sequence so widths stay correct in every terminal.
        let colored = match self.tone {
            Tone::Normal => return padded,
            Tone::Muted => muted(&text),
            Tone::Strong => strong(&text),
            Tone::Accent => accent(&text),
            Tone::Success => success_text(&text),
            Tone::Warn => warn_text(&text),
            Tone::Error => error_text(&text),
        };
        let extra = width.saturating_sub(display_width(&text));
        match align {
            Align::Left => format!("{colored}{}", " ".repeat(extra)),
            Align::Right => format!("{}{colored}", " ".repeat(extra)),
        }
    }
}

/// A width-aware table with a header rule.
pub struct Table {
    columns: Vec<Column>,
    rows: Vec<Vec<Cell>>,
    indent: usize,
    wrap_last: bool,
}

impl Table {
    pub fn new(columns: Vec<Column>) -> Self {
        Self {
            columns,
            rows: Vec::new(),
            indent: 0,
            wrap_last: true,
        }
    }

    pub fn indent(mut self, n: usize) -> Self {
        self.indent = n;
        self
    }

    pub fn wrap_last(mut self, yes: bool) -> Self {
        self.wrap_last = yes;
        self
    }

    pub fn row(&mut self, cells: Vec<Cell>) {
        self.rows.push(cells);
    }

    /// Paint the table and write it to stdout.
    pub fn print(&self) {
        print!("{}", self.render());
    }

    /// Render the table as text. The returned lines never exceed [`width`].
    pub fn render(&self) -> String {
        self.render_at(width())
    }

    /// Render at an explicit cell budget (tests, previews, embedded reports).
    pub fn render_at(&self, limit: usize) -> String {
        let n = self.columns.len();
        if n == 0 {
            return String::new();
        }
        let mut visible: Vec<usize> = (0..n).collect();

        // Natural widths, capped by each column's declared maximum.
        let mut widths: Vec<usize> = self
            .columns
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let mut w = display_width(c.header);
                for row in &self.rows {
                    if let Some(cell) = row.get(i) {
                        w = w.max(display_width(&cell.text));
                    }
                }
                let w = w.max(c.min);
                c.max.map(|m| w.min(m)).unwrap_or(w)
            })
            .collect();

        let total = |ws: &[usize], vis: &[usize]| -> usize {
            self.indent
                + vis.iter().map(|i| ws[*i]).sum::<usize>()
                + 2 * vis.len().saturating_sub(1)
        };

        if limit != usize::MAX && total(&widths, &visible) > limit {
            // 1. The trailing free-text column is the wrapping one: its content
            //    survives reflow, so it absorbs the overflow before any factual
            //    column is squeezed into an ellipsis.
            if self.wrap_last {
                if let Some(&last) = visible.last() {
                    let min = self.columns[last].min.max(1);
                    let can = widths[last].saturating_sub(min);
                    let excess = total(&widths, &visible) - limit;
                    widths[last] -= can.min(excess);
                }
            }

            // 2. Shrink flexible columns, highest priority first.
            let mut order: Vec<usize> = visible.clone();
            order.sort_by_key(|i| std::cmp::Reverse(self.columns[*i].priority));
            for i in order {
                if total(&widths, &visible) <= limit {
                    break;
                }
                let excess = total(&widths, &visible) - limit;
                let min = self.columns[i].min.max(1);
                let can = widths[i].saturating_sub(min);
                widths[i] -= can.min(excess);
            }

            // 2. Drop the most optional columns when even minimums do not fit.
            while total(&widths, &visible) > limit && visible.len() > 2 {
                let drop = visible
                    .iter()
                    .enumerate()
                    .filter(|(_, i)| self.columns[**i].priority > 0)
                    .max_by_key(|(pos, i)| (self.columns[**i].priority, *pos))
                    .map(|(pos, _)| pos);
                match drop {
                    Some(pos) => {
                        visible.remove(pos);
                    }
                    None => break,
                }
            }

            // 3. Last resort: squeeze every remaining column to one cell. The
            //    table may still be unreadable here, but it will not wrap.
            if total(&widths, &visible) > limit {
                let avail = limit.saturating_sub(self.indent + 2 * visible.len().saturating_sub(1));
                let each = (avail / visible.len()).max(1);
                for i in &visible {
                    widths[*i] = widths[*i].min(each.max(1));
                }
            }
        }

        let mut out = String::new();
        let lead = " ".repeat(self.indent);
        let sep = "  ";

        // Header.
        let header: Vec<String> = visible
            .iter()
            .map(|i| {
                let c = &self.columns[*i];
                let h = truncate(c.header, widths[*i]);
                pad(&strong(&h), widths[*i], c.align)
            })
            .collect();
        out.push_str(&lead);
        out.push_str(&header.join(sep));
        out.push('\n');

        // Rule.
        let rule: Vec<String> = visible
            .iter()
            .map(|i| {
                std::iter::repeat(glyph("─", "-"))
                    .take(widths[*i])
                    .collect()
            })
            .collect();
        out.push_str(&lead);
        out.push_str(&muted(&rule.join(sep)));
        out.push('\n');

        for row in &self.rows {
            self.render_row(row, &visible, &widths, &lead, sep, &mut out);
        }
        out
    }

    fn render_row(
        &self,
        row: &[Cell],
        visible: &[usize],
        widths: &[usize],
        pad: &str,
        sep: &str,
        out: &mut String,
    ) {
        let last_visible = visible.last().copied();
        // Wrap the trailing free-text column: it is the one whose content is
        // worth reading in full, and its continuations are cheap.
        let mut wrapped: Vec<Vec<String>> = Vec::new();
        for i in visible {
            let text = row.get(*i).map(|c| c.text.clone()).unwrap_or_default();
            let lines = if self.wrap_last && Some(*i) == last_visible {
                wrap(&text, widths[*i].max(1))
            } else {
                vec![text]
            };
            wrapped.push(lines);
        }
        let height = wrapped.iter().map(|l| l.len()).max().unwrap_or(1);
        for line in 0..height {
            let mut cells_out: Vec<String> = Vec::new();
            for (pos, i) in visible.iter().enumerate() {
                let cell = row.get(*i);
                let text = wrapped[pos].get(line).cloned().unwrap_or_default();
                let tone = cell.map(|c| c.tone).unwrap_or(Tone::Normal);
                cells_out.push(Cell { text, tone }.painted(widths[*i], self.columns[*i].align));
            }
            out.push_str(pad);
            out.push_str(&cells_out.join(sep));
            out.push('\n');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_and_cjk_widths_are_counted_in_cells() {
        assert_eq!(display_width("grovefs"), 7);
        assert_eq!(display_width("视图"), 4);
        assert_eq!(display_width("a视b"), 4);
        assert_eq!(display_width("\x1b[32mok\x1b[0m"), 2);
    }

    #[test]
    fn truncation_never_exceeds_the_budget() {
        for max in 1..20 {
            let t = truncate("abcdefghijklmnopqrstuvwxyz", max);
            assert!(
                display_width(&t) <= max,
                "max {max} produced width {} ({t:?})",
                display_width(&t)
            );
        }
        let cjk = truncate("这是一个很长的中文标签", 7);
        assert!(display_width(&cjk) <= 7, "{cjk:?}");
    }

    #[test]
    fn wrapping_keeps_words_and_bounds_lines() {
        let lines = wrap("one two three four five six seven", 12);
        assert!(lines.iter().all(|l| display_width(l) <= 12), "{lines:?}");
        assert_eq!(
            lines.join(" "),
            "one two three four five six seven",
            "wrapping must not drop words"
        );
    }

    #[test]
    fn a_table_fits_a_narrow_terminal() {
        let mut t = Table::new(vec![
            Column::left("id").min(4),
            Column::left("owner").priority(2).min(4),
            Column::left("note").min(6),
        ]);
        t.row(vec![
            Cell::new("0123456789abcdef"),
            Cell::new("someone"),
            Cell::new("a long trailing note that should wrap instead of overflowing"),
        ]);
        let text = t.render_at(60);
        for line in text.lines() {
            assert!(
                display_width(line) <= 60,
                "line wider than 60 (got {}): {:?}",
                display_width(line),
                line
            );
        }
        assert!(text.contains("a long trailing note"), "{text}");
        assert!(
            text.contains("overflowing"),
            "the tail must wrap, not vanish:\n{text}"
        );
    }
}
