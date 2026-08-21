//! fzf option compatibility: everything `arb --fzf` needs to paint the picker the
//! `fzf` binary would paint for the same environment.
//!
//! Two halves:
//!
//! 1. **Config ingestion** — `$FZF_DEFAULT_OPTS_FILE` and `$FZF_DEFAULT_OPTS`.
//!    A drop-in (`ZPWR_FZF='arb --fzf'`) inherits the user's fzf configuration
//!    exactly like the real binary does, so the prompt string, layout, border,
//!    colors and key bindings come out identical without touching the call site.
//! 2. **Look parsing** — the presentation flags (`--layout`, `--border`,
//!    `--info`, `--pointer`, `--marker`, `--color`, `--bind`, `--cycle`) that
//!    used to be dropped as "cosmetic". They are the look, so they are honored.
//!
//! The default palettes are not invented: they were read off fzf 0.74.2's own
//! output by running it under a pty and decoding the SGR sequences it emitted
//! (`dark`, `light` and `base16` each captured separately). Each constant below
//! is the color index fzf actually wrote for that slot.

use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{BorderType, Borders};

/// Split a shell-style option string into words, honoring `'…'`, `"…"` and `\`.
/// `$FZF_DEFAULT_OPTS` is written as a shell fragment — e.g.
/// `--prompt='<<)ZPWR(>> ' --reverse` — so splitting on whitespace alone would
/// tear the prompt in half and lose its trailing space.
pub fn shell_split(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut started = false; // `--prompt=''` is an empty word, not no word
    let mut quote: Option<char> = None;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match quote {
            // Inside single quotes everything is literal (shell rules).
            Some('\'') if c == '\'' => quote = None,
            Some('\'') => cur.push(c),
            Some('"') if c == '"' => quote = None,
            Some('"') if c == '\\' => match chars.next() {
                // In double quotes a backslash only escapes these four.
                Some(n @ ('"' | '\\' | '$' | '`')) => cur.push(n),
                Some(n) => {
                    cur.push('\\');
                    cur.push(n);
                }
                None => cur.push('\\'),
            },
            Some('"') => cur.push(c),
            Some(_) => unreachable!("quote is only ever ' or \""),
            None => match c {
                '\'' | '"' => {
                    started = true;
                    quote = Some(c);
                }
                '\\' => {
                    started = true;
                    if let Some(n) = chars.next() {
                        cur.push(n);
                    }
                }
                c if c.is_whitespace() => {
                    if started {
                        out.push(std::mem::take(&mut cur));
                        started = false;
                    }
                }
                _ => {
                    started = true;
                    cur.push(c);
                }
            },
        }
    }
    if started {
        out.push(cur);
    }
    out
}

/// The user's fzf configuration as argv words: `$FZF_DEFAULT_OPTS_FILE` first,
/// then `$FZF_DEFAULT_OPTS` — fzf's own precedence, with the command line
/// (spliced after these by the caller) winning over both.
pub fn env_args() -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(path) = std::env::var("FZF_DEFAULT_OPTS_FILE") {
        let path = match (path.strip_prefix("~/"), std::env::var("HOME")) {
            (Some(rest), Ok(home)) => format!("{home}/{rest}"),
            _ => path,
        };
        if let Ok(body) = std::fs::read_to_string(path) {
            out.extend(shell_split(&body));
        }
    }
    if let Ok(opts) = std::env::var("FZF_DEFAULT_OPTS") {
        out.extend(shell_split(&opts));
    }
    out
}

/// One `--color` slot: an optional color (None = the terminal's default) plus
/// text attributes, e.g. fzf's `hl+:151:bold`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ent {
    pub color: Option<Color>,
    pub attrs: Modifier,
}

impl Ent {
    /// A plain colored slot.
    fn c(idx: u8) -> Self {
        Ent {
            color: Some(Color::Indexed(idx)),
            attrs: Modifier::empty(),
        }
    }
    /// A bold colored slot (fzf bolds the pointer, the current line and hl+).
    fn b(idx: u8) -> Self {
        Ent {
            color: Some(Color::Indexed(idx)),
            attrs: Modifier::BOLD,
        }
    }
    /// Attributes only — the terminal's default color (fzf's `-1`).
    fn attr(attrs: Modifier) -> Self {
        Ent { color: None, attrs }
    }
    /// As a foreground style.
    pub fn fg(self) -> Style {
        let s = Style::default().add_modifier(self.attrs);
        match self.color {
            Some(c) => s.fg(c),
            None => s,
        }
    }
    /// As a background style (used for `bg`/`bg+`, where attrs don't apply).
    pub fn bg(self) -> Style {
        match self.color {
            Some(c) => Style::default().bg(c),
            None => Style::default(),
        }
    }
}

/// fzf's color slots. Field names track fzf's option names (`fg+` → `fg_plus`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Colors {
    pub fg: Ent,
    pub bg: Ent,
    pub hl: Ent,
    pub fg_plus: Ent,
    pub bg_plus: Ent,
    pub hl_plus: Ent,
    pub gutter: Ent,
    pub pointer: Ent,
    pub marker: Ent,
    pub prompt: Ent,
    pub query: Ent,
    pub info: Ent,
    pub spinner: Ent,
    pub header: Ent,
    pub border: Ent,
    pub separator: Ent,
    pub scrollbar: Ent,
}

impl Default for Colors {
    fn default() -> Self {
        Colors::dark()
    }
}

impl Colors {
    /// fzf's `dark` scheme (its default on a 256-color terminal), as measured
    /// from fzf 0.74.2's SGR output.
    pub fn dark() -> Self {
        Colors {
            fg: Ent::default(),
            bg: Ent::default(),
            hl: Ent::c(108),
            fg_plus: Ent::b(254),
            bg_plus: Ent::c(236),
            hl_plus: Ent::b(151),
            gutter: Ent::c(236),
            pointer: Ent::b(161),
            marker: Ent::c(168),
            prompt: Ent::b(110),
            query: Ent::attr(Modifier::BOLD),
            info: Ent::c(144),
            spinner: Ent::b(148),
            header: Ent::c(109),
            border: Ent::c(59),
            separator: Ent::c(59),
            scrollbar: Ent::c(59),
        }
    }

    /// fzf's `light` scheme.
    pub fn light() -> Self {
        Colors {
            hl: Ent::c(66),
            fg_plus: Ent::b(237),
            bg_plus: Ent::c(251),
            hl_plus: Ent::b(23),
            gutter: Ent::c(251),
            pointer: Ent::b(161),
            marker: Ent::c(168),
            prompt: Ent::b(25),
            info: Ent::c(101),
            border: Ent::c(145),
            separator: Ent::c(145),
            scrollbar: Ent::c(145),
            ..Colors::dark()
        }
    }

    /// fzf's `base16`/`16` scheme — the basic ANSI palette instead of 256 colors.
    pub fn base16() -> Self {
        Colors {
            hl: Ent {
                color: Some(Color::Green),
                attrs: Modifier::empty(),
            },
            fg_plus: Ent {
                color: Some(Color::White),
                attrs: Modifier::BOLD,
            },
            bg_plus: Ent {
                color: Some(Color::DarkGray),
                attrs: Modifier::empty(),
            },
            hl_plus: Ent {
                color: Some(Color::LightGreen),
                attrs: Modifier::BOLD,
            },
            gutter: Ent {
                color: Some(Color::DarkGray),
                attrs: Modifier::empty(),
            },
            pointer: Ent {
                color: Some(Color::Red),
                attrs: Modifier::BOLD,
            },
            marker: Ent {
                color: Some(Color::Magenta),
                attrs: Modifier::empty(),
            },
            prompt: Ent {
                color: Some(Color::Blue),
                attrs: Modifier::BOLD,
            },
            info: Ent {
                color: Some(Color::Yellow),
                attrs: Modifier::empty(),
            },
            border: Ent::attr(Modifier::DIM),
            separator: Ent::attr(Modifier::DIM),
            scrollbar: Ent::attr(Modifier::DIM),
            ..Colors::dark()
        }
    }

    /// fzf's `bw` scheme (`--no-color`, or `$NO_COLOR` set): attributes only.
    pub fn bw() -> Self {
        Colors {
            fg: Ent::default(),
            bg: Ent::default(),
            hl: Ent::attr(Modifier::UNDERLINED),
            fg_plus: Ent::attr(Modifier::BOLD),
            bg_plus: Ent::default(),
            hl_plus: Ent::attr(Modifier::BOLD | Modifier::UNDERLINED),
            gutter: Ent::default(),
            pointer: Ent::attr(Modifier::BOLD),
            marker: Ent::attr(Modifier::BOLD),
            prompt: Ent::default(),
            query: Ent::attr(Modifier::BOLD),
            info: Ent::default(),
            spinner: Ent::default(),
            header: Ent::default(),
            border: Ent::default(),
            separator: Ent::default(),
            scrollbar: Ent::default(),
        }
    }

    /// Look up a base scheme by fzf's name.
    fn scheme(name: &str) -> Option<Self> {
        match name {
            "dark" => Some(Colors::dark()),
            "light" => Some(Colors::light()),
            "16" | "base16" => Some(Colors::base16()),
            "bw" | "no" => Some(Colors::bw()),
            _ => None,
        }
    }

    /// Assign one slot by its fzf name (aliases included). Unknown names — the
    /// preview/label/scrollbar family arb has no analog for — are ignored, so a
    /// rich `--color` line from a themed setup still applies the slots that do
    /// map instead of failing the whole spec.
    fn set(&mut self, name: &str, ent: Ent) {
        match name {
            "fg" | "list-fg" => self.fg = ent,
            "bg" | "list-bg" => self.bg = ent,
            "hl" => self.hl = ent,
            "fg+" | "current-fg" | "selected-fg" => self.fg_plus = ent,
            "bg+" | "current-bg" | "selected-bg" => self.bg_plus = ent,
            "hl+" | "current-hl" | "selected-hl" => self.hl_plus = ent,
            "gutter" => self.gutter = ent,
            "pointer" => self.pointer = ent,
            "marker" => self.marker = ent,
            "prompt" => self.prompt = ent,
            "query" | "input-fg" => self.query = ent,
            "info" => self.info = ent,
            "spinner" => self.spinner = ent,
            "header" => self.header = ent,
            "border" => {
                self.border = ent;
                self.separator = ent;
            }
            "separator" => self.separator = ent,
            _ => {}
        }
    }
}

/// Parse one fzf color token: an index (`161`), `-1` for the terminal default,
/// `#rrggbb`, or an ANSI color name.
fn parse_color(tok: &str) -> Option<Option<Color>> {
    if tok == "-1" || tok == "default" {
        return Some(None);
    }
    if let Some(hex) = tok.strip_prefix('#') {
        if hex.len() == 6 {
            let n = u32::from_str_radix(hex, 16).ok()?;
            return Some(Some(Color::Rgb((n >> 16) as u8, (n >> 8) as u8, n as u8)));
        }
        return None;
    }
    if let Ok(idx) = tok.parse::<u8>() {
        return Some(Some(Color::Indexed(idx)));
    }
    let named = match tok {
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" => Color::Magenta,
        "cyan" => Color::Cyan,
        "white" => Color::Gray,
        "bright-black" | "gray" | "grey" => Color::DarkGray,
        "bright-red" => Color::LightRed,
        "bright-green" => Color::LightGreen,
        "bright-yellow" => Color::LightYellow,
        "bright-blue" => Color::LightBlue,
        "bright-magenta" => Color::LightMagenta,
        "bright-cyan" => Color::LightCyan,
        "bright-white" => Color::White,
        _ => return None,
    };
    Some(Some(named))
}

/// Parse one fzf attribute token.
fn parse_attr(tok: &str) -> Option<Modifier> {
    match tok {
        "bold" => Some(Modifier::BOLD),
        "dim" => Some(Modifier::DIM),
        "italic" => Some(Modifier::ITALIC),
        "underline" => Some(Modifier::UNDERLINED),
        "blink" => Some(Modifier::SLOW_BLINK),
        "reverse" => Some(Modifier::REVERSED),
        "strikethrough" => Some(Modifier::CROSSED_OUT),
        "regular" => Some(Modifier::empty()),
        _ => None,
    }
}

/// Apply one `--color=…` value: an optional leading base scheme followed by
/// `name:color[:attr…]` entries, separated by commas and/or whitespace.
fn apply_color_spec(colors: &mut Colors, spec: &str) {
    for entry in spec.split([',', ' ', '\t']).filter(|e| !e.is_empty()) {
        let mut parts = entry.split(':');
        let Some(name) = parts.next() else { continue };
        if let Some(scheme) = Colors::scheme(name) {
            // A base scheme resets every slot; later entries customize it.
            *colors = scheme;
            continue;
        }
        let mut ent = Ent::default();
        let mut saw = false;
        for tok in parts {
            if let Some(c) = parse_color(tok) {
                ent.color = c;
                saw = true;
            } else if let Some(a) = parse_attr(tok) {
                ent.attrs |= a;
                saw = true;
            }
        }
        if saw {
            colors.set(name, ent);
        }
    }
}

/// One `--nth`/`--with-nth` field range. `1` is the first field, `-1` the last,
/// and [`Nth::ELLIPSIS`] is the open end of `2..` / `..3` / `..`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Nth {
    pub begin: i32,
    pub end: i32,
}

impl Nth {
    pub const ELLIPSIS: i32 = i32::MIN;

    /// Parse one range in fzf's syntax (`ParseRange` in tokenizer.go).
    pub fn parse(s: &str) -> Option<Nth> {
        let n = |v: &str| v.parse::<i32>().ok().filter(|n| *n != 0);
        if s == ".." {
            return Some(Nth {
                begin: Nth::ELLIPSIS,
                end: Nth::ELLIPSIS,
            });
        }
        if let Some(rest) = s.strip_prefix("..") {
            return n(rest).map(|end| Nth {
                begin: Nth::ELLIPSIS,
                end,
            });
        }
        if let Some(rest) = s.strip_suffix("..") {
            return n(rest).map(|begin| Nth {
                begin,
                end: Nth::ELLIPSIS,
            });
        }
        if let Some((a, b)) = s.split_once("..") {
            let (begin, end) = (n(a)?, n(b)?);
            // fzf rejects a negative start paired with a positive end.
            return (!(begin < 0 && end > 0)).then_some(Nth { begin, end });
        }
        n(s).map(|v| Nth { begin: v, end: v })
    }

    /// Parse a comma-separated list (`--nth 1,3..`). Any bad entry drops the
    /// whole list, as fzf treats it as an option error.
    pub fn parse_list(s: &str) -> Vec<Nth> {
        let mut out = Vec::new();
        for part in s.split(',').filter(|p| !p.is_empty()) {
            match Nth::parse(part) {
                Some(r) => out.push(r),
                None => return Vec::new(),
            }
        }
        out
    }
}

/// Split a line into fields the way fzf's `Tokenize` does: with no
/// `--delimiter`, AWK-style — each token is a run of non-space plus the
/// whitespace that follows it, and leading whitespace belongs to no token. With
/// a delimiter, each token keeps its trailing delimiter so ranges rejoin
/// exactly as they appeared.
pub fn tokenize<'a>(text: &'a str, delimiter: Option<&str>) -> Vec<&'a str> {
    let Some(d) = delimiter else {
        let mut out = Vec::new();
        let bytes = text.as_bytes();
        let (mut start, mut i) = (0usize, 0usize);
        // Skip the leading whitespace — fzf counts it as a prefix, not a token.
        while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
            i += 1;
        }
        start = start.max(i);
        while i < bytes.len() {
            // A token runs to the end of the whitespace that follows it.
            while i < bytes.len() && bytes[i] != b' ' && bytes[i] != b'\t' {
                i += 1;
            }
            while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
                i += 1;
            }
            out.push(&text[start..i]);
            start = i;
        }
        return out;
    };
    if d.is_empty() {
        return vec![text];
    }
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(pos) = rest.find(d) {
        let (head, tail) = rest.split_at(pos + d.len());
        out.push(head);
        rest = tail;
    }
    if !rest.is_empty() {
        out.push(rest);
    }
    out
}

/// The text a `--nth`/`--with-nth` range selects: the chosen tokens rejoined.
/// With an explicit delimiter the trailing one is dropped (`--nth 1` on
/// `a:b:c` matches `a`, not `a:`, while `--nth 1..2` matches `a:b`); AWK-style
/// tokens keep their trailing whitespace.
pub fn transform(tokens: &[&str], ranges: &[Nth], delimiter: Option<&str>) -> String {
    if ranges.is_empty() {
        return tokens.concat();
    }
    let n = tokens.len() as i32;
    let resolve = |v: i32| if v < 0 { v + n + 1 } else { v };
    let mut out = String::new();
    for r in ranges {
        let (begin, end) = match (r.begin, r.end) {
            (Nth::ELLIPSIS, Nth::ELLIPSIS) => (1, n),
            (Nth::ELLIPSIS, e) => (1, resolve(e)),
            (b, Nth::ELLIPSIS) => (resolve(b), n),
            (b, e) => (resolve(b), resolve(e)),
        };
        let mut part = String::new();
        for idx in begin.max(1)..=end.min(n) {
            part.push_str(tokens[(idx - 1) as usize]);
        }
        match delimiter {
            Some(d) => out.push_str(part.strip_suffix(d).unwrap_or(&part)),
            None => out.push_str(&part),
        }
    }
    out
}

/// Strip SGR escape sequences, leaving the text `--ansi` should match against.
pub fn strip_ansi(s: &str) -> String {
    if !s.contains('\u{1b}') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        // `ESC [ … <final>` — drop the whole sequence.
        if chars.next() != Some('[') {
            continue;
        }
        for t in chars.by_ref() {
            if t.is_ascii_alphabetic() {
                break;
            }
        }
    }
    out
}

/// Where the prompt sits relative to the list (fzf `--layout`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    /// fzf's default: prompt at the bottom, list growing upward from it.
    Default,
    /// `--reverse` / `--layout=reverse`: prompt at the top, list below.
    Reverse,
    /// `--layout=reverse-list`: prompt at the bottom, list top-down.
    ReverseList,
}

/// Where the match counter goes (fzf `--info`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Info {
    /// Own line, on the left of a horizontal separator (fzf's default).
    Default,
    /// Own line, on the right of the separator.
    Right,
    /// No counter at all (`--info=hidden` / `--no-info`).
    Hidden,
    /// After the query, with the given prefix (default ` < `).
    Inline(String),
    /// At the right end of the prompt line.
    InlineRight(String),
}

/// Where `--preview-window` puts the preview box relative to the list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreviewPos {
    Up,
    Down,
    Left,
    /// fzf's default.
    Right,
}

/// How much room the preview box takes: a share of the body, or a fixed number
/// of columns (`left`/`right`) or rows (`up`/`down`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreviewSize {
    Percent(u16),
    Cells(u16),
}

/// `--preview-window=[POSITION][,SIZE[%]][,[no]wrap][,[no]hidden][,…]`.
///
/// The flags arb can act on are modeled; the rest of fzf's grammar
/// (`border-STYLE`, `follow`, `cycle`, `info`, `+SCROLL`, `~HEADER_LINES`,
/// `default`, `<THRESHOLD(ALT)`) parses without error and is ignored, so a
/// spec written for fzf never fails here — it just may not be honored in full.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PreviewWindow {
    pub pos: PreviewPos,
    pub size: PreviewSize,
    /// `hidden`: the box is not drawn (the command still runs, as in fzf).
    pub hidden: bool,
    /// `wrap`: long preview lines wrap instead of being truncated.
    pub wrap: bool,
}

impl Default for PreviewWindow {
    fn default() -> Self {
        // fzf(1): "POSITION: (default: right)", and its default size is half the
        // window.
        Self {
            pos: PreviewPos::Right,
            size: PreviewSize::Percent(50),
            hidden: false,
            wrap: false,
        }
    }
}

impl PreviewWindow {
    /// Parse one `--preview-window` spec. fzf accepts the parts in any order,
    /// separated by `,`; the older `:` form (`right:55%`) is still widely
    /// written, so both separators are taken.
    pub fn parse(spec: &str) -> PreviewWindow {
        let mut w = PreviewWindow::default();
        for part in spec.split([',', ':']).map(str::trim) {
            match part {
                "" => {}
                "up" | "top" => w.pos = PreviewPos::Up,
                "down" | "bottom" => w.pos = PreviewPos::Down,
                "left" => w.pos = PreviewPos::Left,
                // `next` places the preview on the list side of the input. arb
                // draws the input at the top in every fzf layout it models, so
                // that is the same side as `down`.
                "right" | "next" => w.pos = PreviewPos::Right,
                "hidden" => w.hidden = true,
                "nohidden" => w.hidden = false,
                "wrap" | "wrap-word" => w.wrap = true,
                "nowrap" => w.wrap = false,
                _ => {
                    if let Some(n) = part.strip_suffix('%') {
                        if let Ok(pct) = n.parse::<u16>() {
                            w.size = PreviewSize::Percent(pct.min(100));
                        }
                    } else if let Ok(cells) = part.parse::<u16>() {
                        w.size = PreviewSize::Cells(cells);
                    }
                    // Anything else is a part of fzf's grammar arb does not act
                    // on (border-*, follow, cycle, info, +SCROLL, ~N, default,
                    // <THRESHOLD(ALT)). Ignored rather than rejected.
                }
            }
        }
        w
    }

    /// Split `body` into (list, preview) rectangles. `None` means the preview is
    /// not drawn — `hidden`, or a size of zero, which fzf documents as "preview
    /// window will not be visible, but fzf will still execute the command".
    pub fn split(&self, body: ratatui::layout::Rect) -> (ratatui::layout::Rect, Option<ratatui::layout::Rect>) {
        use ratatui::layout::{Constraint, Layout};
        if self.hidden {
            return (body, None);
        }
        let vertical = matches!(self.pos, PreviewPos::Up | PreviewPos::Down);
        let total = match vertical {
            true => body.height,
            false => body.width,
        };
        let want = match self.size {
            PreviewSize::Percent(p) => (u32::from(total) * u32::from(p) / 100) as u16,
            PreviewSize::Cells(c) => c,
        };
        // Leave at least one cell for the list itself, or there is nothing to
        // pick from.
        let want = want.min(total.saturating_sub(1));
        if want == 0 {
            return (body, None);
        }
        let rest = total - want;
        let constraints = match self.pos {
            PreviewPos::Up | PreviewPos::Left => [Constraint::Length(want), Constraint::Length(rest)],
            PreviewPos::Down | PreviewPos::Right => {
                [Constraint::Length(rest), Constraint::Length(want)]
            }
        };
        let chunks = match vertical {
            true => Layout::vertical(constraints).split(body),
            false => Layout::horizontal(constraints).split(body),
        };
        match self.pos {
            PreviewPos::Up | PreviewPos::Left => (chunks[1], Some(chunks[0])),
            PreviewPos::Down | PreviewPos::Right => (chunks[0], Some(chunks[1])),
        }
    }
}

/// How the query's case is read. fzf's default is smart-case — a query typed
/// in lowercase matches either case, and one uppercase character anywhere makes
/// the whole query case-sensitive. `-i`/`--ignore-case` and `+i`/
/// `--no-ignore-case` pin it either way; `--smart-case` restores the default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Case {
    #[default]
    Smart,
    Ignore,
    Respect,
}

/// The resolved presentation of the picker: everything the renderer and the key
/// handler need in order to match fzf.
#[derive(Clone, Debug)]
pub struct Look {
    pub layout: Layout,
    pub info: Info,
    /// `None` = borderless (fzf's default). `Some((type, sides))` maps straight
    /// onto ratatui's `Block`.
    pub border: Option<(BorderType, Borders)>,
    pub pointer: String,
    pub marker: String,
    /// `--ellipsis`: what a truncated row ends with (fzf's default is `··`).
    pub ellipsis: String,
    pub colors: Colors,
    /// `--cycle`: moving past either end wraps around.
    pub cycle: bool,
    /// `--tac`: present the list in reverse input order.
    pub tac: bool,
    /// `--ansi`: the input carries SGR colour codes — render them, and match
    /// against the text with the codes removed.
    pub ansi: bool,
    /// `--delimiter`: what splits a line into fields. `None` = fzf's AWK-style
    /// splitting (a run of non-space plus the whitespace after it).
    pub delimiter: Option<String>,
    /// `--nth`: which fields the query matches against (the row still shows,
    /// and Enter still emits, the whole line).
    pub nth: Vec<Nth>,
    /// `--with-nth`: which fields the row SHOWS (and, absent `--nth`, matches).
    pub with_nth: Vec<Nth>,
    /// `--header-lines`: the first N input lines are a header, not candidates.
    pub header_lines: usize,
    /// `--with-shell`: how child processes (a `--preview` command, an `execute`
    /// binding) are started. fzf defaults to `$SHELL -c`, NOT `sh -c` — a
    /// preview written against the user's shell (zsh's `print`, a function from
    /// their rc) fails outright under `sh`.
    pub with_shell: Option<String>,
    /// `--print-query`: the query is written before the selection.
    pub print_query: bool,
    /// `--expect=KEYS`: these keys also accept, and the one that did is written
    /// before the selection (an empty line when plain Enter accepted). This is
    /// how fzf-tab drives continuous completion.
    pub expect: Vec<(Key, String)>,
    /// `--min-height=N[+]`: the floor for a percentage `--height`. fzf's default
    /// is `10+` — ten LIST rows, with the chrome added on top (the `+`).
    pub min_height: u16,
    pub min_height_plus: bool,
    /// `--no-scrollbar`: give the list its last column back.
    pub scrollbar: bool,
    /// `--scroll-off`: screen lines kept above/below the cursor while scrolling
    /// (fzf's default is 3, which is why its list starts sliding well before the
    /// cursor reaches the edge).
    pub scroll_off: usize,
    /// fzf's default `--tiebreak=length`: equally-scored matches rank shortest
    /// first (`--tiebreak=index` turns this off and keeps input order). The
    /// other criteria fzf offers (`begin`, `end`, `chunk`, `pathname`) parse but
    /// fall back to this pair.
    pub tiebreak_length: bool,
    /// `--preview-window`: where the preview box goes and how big it is.
    pub preview_window: PreviewWindow,
    /// `--bind` bindings, in declaration order (later wins for the same key).
    pub binds: Vec<(Key, Vec<Action>)>,
    /// `-i`/`--ignore-case`, `+i`/`--no-ignore-case`, `--smart-case`.
    pub case: Case,
    /// `-x`/`--extended` (fzf's DEFAULT) vs `+x`/`--no-extended`. Extended makes
    /// the query a term list — space-separated terms are AND'd, `|` ORs them,
    /// and `'exact` / `^prefix` / `suffix$` / `!inverse` change how a term
    /// matches. `+x` makes the whole query one literal fuzzy term.
    pub extended: bool,
}

impl Default for Look {
    fn default() -> Self {
        Look {
            layout: Layout::Default,
            info: Info::Default,
            border: None,
            // fzf 0.74's default pointer/marker glyphs.
            pointer: "\u{258c}".to_string(),
            marker: "\u{2503}".to_string(),
            ellipsis: "\u{b7}\u{b7}".to_string(),
            colors: Colors::dark(),
            cycle: false,
            tac: false,
            ansi: false,
            delimiter: None,
            nth: Vec::new(),
            with_nth: Vec::new(),
            header_lines: 0,
            with_shell: None,
            print_query: false,
            expect: Vec::new(),
            min_height: 10,
            min_height_plus: true,
            scrollbar: true,
            scroll_off: 3,
            tiebreak_length: true,
            preview_window: PreviewWindow::default(),
            binds: Vec::new(),
            case: Case::Smart,
            extended: true,
        }
    }
}

/// Map an fzf `--border` style name onto ratatui's border type + sides.
fn parse_border(style: &str) -> Option<(BorderType, Borders)> {
    let all = Borders::ALL;
    match style {
        "rounded" | "" => Some((BorderType::Rounded, all)),
        "sharp" | "block" | "thinblock" => Some((BorderType::Plain, all)),
        "bold" | "double" => Some((BorderType::Double, all)),
        "horizontal" => Some((BorderType::Plain, Borders::TOP | Borders::BOTTOM)),
        "vertical" => Some((BorderType::Plain, Borders::LEFT | Borders::RIGHT)),
        "top" | "up" => Some((BorderType::Plain, Borders::TOP)),
        "bottom" | "down" => Some((BorderType::Plain, Borders::BOTTOM)),
        "left" => Some((BorderType::Plain, Borders::LEFT)),
        "right" => Some((BorderType::Plain, Borders::RIGHT)),
        "none" => None,
        _ => Some((BorderType::Rounded, all)),
    }
}

/// A key an fzf `--bind` can name. Only keys arb's `/dev/tty` reader can
/// actually deliver are modeled; anything else parses to `None` and its binding
/// is dropped rather than silently mis-firing on a different key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    /// `ctrl-x` — the letter, lowercased.
    Ctrl(char),
    /// `alt-x` — the character after Esc.
    Alt(char),
    Tab,
    BTab,
    Enter,
    Esc,
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    Home,
    End,
    Backspace,
    Delete,
    Space,
    /// A plain printable key (`--bind=?:toggle-preview`).
    Char(char),
}

/// The fzf actions arb can carry out. fzf has many more (`execute`,
/// `reload`, preview control …); a binding naming one of those is dropped whole
/// so the key keeps its built-in behavior instead of half-working.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Up,
    Down,
    PageUp,
    PageDown,
    HalfPageUp,
    HalfPageDown,
    First,
    Last,
    Toggle,
    ToggleAll,
    Accept,
    Abort,
    ClearQuery,
    BackwardDeleteChar,
    Ignore,
}

/// Parse an fzf key name.
fn parse_key(name: &str) -> Option<Key> {
    let n = name.trim().to_ascii_lowercase();
    if let Some(rest) = n.strip_prefix("ctrl-") {
        let mut ch = rest.chars();
        let c = ch.next()?;
        return ch.next().is_none().then_some(Key::Ctrl(c));
    }
    if let Some(rest) = n.strip_prefix("alt-") {
        let mut ch = rest.chars();
        let c = ch.next()?;
        return ch.next().is_none().then_some(Key::Alt(c));
    }
    Some(match n.as_str() {
        "tab" => Key::Tab,
        "btab" | "shift-tab" => Key::BTab,
        "enter" | "return" => Key::Enter,
        "esc" => Key::Esc,
        "up" => Key::Up,
        "down" => Key::Down,
        "left" => Key::Left,
        "right" => Key::Right,
        "pgup" | "page-up" => Key::PageUp,
        "pgdn" | "page-down" => Key::PageDown,
        "home" => Key::Home,
        "end" => Key::End,
        "bspace" | "bs" | "backspace" => Key::Backspace,
        "del" | "delete" => Key::Delete,
        "space" => Key::Space,
        s if s.chars().count() == 1 => Key::Char(s.chars().next()?),
        _ => return None,
    })
}

/// Parse an fzf action name. `None` = arb has no equivalent.
fn parse_action(name: &str) -> Option<Action> {
    Some(match name.trim() {
        "up" | "prev-history" => Action::Up,
        "down" | "next-history" => Action::Down,
        "page-up" => Action::PageUp,
        "page-down" => Action::PageDown,
        "half-page-up" => Action::HalfPageUp,
        "half-page-down" => Action::HalfPageDown,
        "first" | "top" | "beginning-of-line" => Action::First,
        "last" => Action::Last,
        "toggle" | "toggle+up" => Action::Toggle,
        "toggle-all" | "select-all" | "deselect-all" => Action::ToggleAll,
        "accept" | "accept-non-empty" => Action::Accept,
        "abort" | "cancel" => Action::Abort,
        "clear-query" | "unix-line-discard" | "kill-line" => Action::ClearQuery,
        "backward-delete-char" => Action::BackwardDeleteChar,
        "ignore" => Action::Ignore,
        _ => return None,
    })
}

/// Split on `sep`, ignoring separators inside `(…)` — fzf action arguments
/// (`execute(grep -r {} .)`) routinely contain commas and plus signs.
fn split_top(s: &str, sep: char) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth = 0i32;
    for c in s.chars() {
        match c {
            '(' => {
                depth += 1;
                cur.push(c);
            }
            ')' => {
                depth -= 1;
                cur.push(c);
            }
            c if c == sep && depth <= 0 => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out
}

/// Parse one `--bind` value (`tab:toggle+down,ctrl-n:page-down`) into bindings.
/// A binding whose key or any action arb doesn't implement is skipped entirely.
fn parse_bind_spec(spec: &str) -> Vec<(Key, Vec<Action>)> {
    let mut out = Vec::new();
    for one in split_top(spec, ',') {
        let one = one.trim();
        if one.is_empty() {
            continue;
        }
        let Some((keyname, actions)) = one.split_once(':') else {
            continue;
        };
        let Some(key) = parse_key(keyname) else {
            continue;
        };
        let acts: Option<Vec<Action>> = split_top(actions, '+')
            .iter()
            .map(|a| parse_action(a))
            .collect();
        if let Some(acts) = acts {
            out.push((key, acts));
        }
    }
    out
}

/// The value of `--flag VALUE` / `--flag=VALUE`, consuming the next argv word
/// when the value is separate.
fn flag_value(arg: &str, next: Option<&String>) -> (Option<String>, bool) {
    match arg.split_once('=') {
        Some((_, v)) => (Some(v.to_string()), false),
        None => (next.cloned(), true),
    }
}

impl Look {
    /// Resolve the picker's look from a full argv (env options first, command
    /// line after — later wins, exactly like fzf).
    pub fn parse(args: &[String]) -> Look {
        let mut look = Look::default();
        // fzf honors $NO_COLOR by starting from the `bw` scheme.
        if std::env::var_os("NO_COLOR").is_some() {
            look.colors = Colors::bw();
        }
        let mut i = 0;
        while i < args.len() {
            let a = args[i].as_str();
            // fzf's short flags take an attached value (`-d:`, `-n3..`), so the
            // key is just the flag itself and the rest is the value.
            let key = match a.as_bytes() {
                [b'-', b'd', rest @ ..] if !rest.is_empty() => "-d",
                [b'-', b'n', rest @ ..] if !rest.is_empty() && rest[0] != b'o' => "-n",
                _ => a.split('=').next().unwrap_or(a),
            };
            let attached = match key {
                "-d" | "-n" if a.len() > 2 => Some(a[2..].to_string()),
                _ => None,
            };
            let next = args.get(i + 1);
            match key {
                "--reverse" => look.layout = Layout::Reverse,
                "--no-reverse" => look.layout = Layout::Default,
                "--layout" => {
                    let (v, sep) = flag_value(a, next);
                    look.layout = match v.as_deref() {
                        Some("reverse") => Layout::Reverse,
                        Some("reverse-list") => Layout::ReverseList,
                        _ => Layout::Default,
                    };
                    i += usize::from(sep);
                }
                "--preview-window" => {
                    let (v, sep) = flag_value(a, next);
                    if let Some(spec) = v {
                        look.preview_window = PreviewWindow::parse(&spec);
                    }
                    i += usize::from(sep);
                }
                "--border" => {
                    // `--border` alone is fzf's rounded box; `--border=STYLE` picks.
                    let style = a.split_once('=').map(|(_, v)| v.to_string());
                    look.border = parse_border(style.as_deref().unwrap_or("rounded"));
                }
                "--no-border" => look.border = None,
                "--info" => {
                    let (v, sep) = flag_value(a, next);
                    look.info = match v.as_deref() {
                        Some("hidden") => Info::Hidden,
                        Some("right") => Info::Right,
                        Some("inline") => Info::Inline(" < ".to_string()),
                        Some("inline-right") => Info::InlineRight(String::new()),
                        Some(s) if s.starts_with("inline-right:") => {
                            Info::InlineRight(s["inline-right:".len()..].to_string())
                        }
                        Some(s) if s.starts_with("inline:") => {
                            Info::Inline(s["inline:".len()..].to_string())
                        }
                        _ => Info::Default,
                    };
                    i += usize::from(sep);
                }
                "--no-info" => look.info = Info::Hidden,
                "--inline-info" => look.info = Info::Inline(" < ".to_string()),
                "--no-inline-info" => look.info = Info::Default,
                "--pointer" => {
                    let (v, sep) = flag_value(a, next);
                    if let Some(v) = v {
                        look.pointer = v;
                    }
                    i += usize::from(sep);
                }
                "--marker" => {
                    let (v, sep) = flag_value(a, next);
                    if let Some(v) = v {
                        look.marker = v;
                    }
                    i += usize::from(sep);
                }
                "--ellipsis" => {
                    let (v, sep) = flag_value(a, next);
                    if let Some(v) = v {
                        look.ellipsis = v;
                    }
                    i += usize::from(sep);
                }
                "--color" => {
                    let (v, sep) = flag_value(a, next);
                    if let Some(v) = v {
                        apply_color_spec(&mut look.colors, &v);
                    }
                    i += usize::from(sep);
                }
                "--no-color" => look.colors = Colors::bw(),
                "--tiebreak" => {
                    let (v, sep) = flag_value(a, next);
                    // Only the FIRST criterion decides the order arb can model.
                    look.tiebreak_length = !matches!(
                        v.as_deref()
                            .and_then(|v| v.split(',').next().map(str::to_string))
                            .as_deref(),
                        Some("index")
                    );
                    i += usize::from(sep);
                }
                "--scroll-off" => {
                    let (v, sep) = flag_value(a, next);
                    if let Some(n) = v.and_then(|v| v.parse::<usize>().ok()) {
                        look.scroll_off = n;
                    }
                    i += usize::from(sep);
                }
                "--cycle" => look.cycle = true,
                "--no-cycle" => look.cycle = false,
                "--tac" => look.tac = true,
                "--no-tac" => look.tac = false,
                "--ansi" => look.ansi = true,
                "--print-query" => look.print_query = true,
                "--no-print-query" => look.print_query = false,
                // fzf's case flags. The default is smart-case, so `--smart-case`
                // after an `-i` from `$FZF_DEFAULT_OPTS` puts it back.
                "-i" | "--ignore-case" => look.case = Case::Ignore,
                "+i" | "--no-ignore-case" => look.case = Case::Respect,
                "--smart-case" => look.case = Case::Smart,
                // fzf's query language. Extended is the DEFAULT — `+x` is what
                // turns the query back into a single literal fuzzy term.
                "-x" | "--extended" => look.extended = true,
                "+x" | "--no-extended" => look.extended = false,
                "--with-shell" => {
                    let (v, sep) = flag_value(a, next);
                    look.with_shell = v.filter(|v| !v.is_empty());
                    i += usize::from(sep);
                }
                "--expect" => {
                    let (v, sep) = flag_value(a, next);
                    if let Some(v) = v {
                        look.expect = v
                            .split(',')
                            .filter(|k| !k.is_empty())
                            .filter_map(|k| parse_key(k).map(|p| (p, k.to_string())))
                            .collect();
                    }
                    i += usize::from(sep);
                }
                "--no-ansi" => look.ansi = false,
                "--no-expect" => look.expect.clear(),
                "--no-header-lines" => look.header_lines = 0,
                // `--scrollbar[=CHARS]` puts the bar back after a `--no-scrollbar`.
                // arb draws fzf's default glyph, so only the on/off state is read.
                "--scrollbar" => look.scrollbar = true,
                "--delimiter" | "-d" => {
                    let (v, sep) = match attached {
                        Some(v) => (Some(v), false),
                        None => flag_value(a, next),
                    };
                    look.delimiter = v.filter(|v| !v.is_empty());
                    i += usize::from(sep);
                }
                "--nth" | "-n" => {
                    let (v, sep) = match attached {
                        Some(v) => (Some(v), false),
                        None => flag_value(a, next),
                    };
                    if let Some(v) = v {
                        look.nth = Nth::parse_list(&v);
                    }
                    i += usize::from(sep);
                }
                "--with-nth" => {
                    let (v, sep) = flag_value(a, next);
                    if let Some(v) = v {
                        look.with_nth = Nth::parse_list(&v);
                    }
                    i += usize::from(sep);
                }
                "--header-lines" => {
                    let (v, sep) = flag_value(a, next);
                    if let Some(n) = v.and_then(|v| v.parse::<usize>().ok()) {
                        look.header_lines = n;
                    }
                    i += usize::from(sep);
                }
                "--min-height" => {
                    let (v, sep) = flag_value(a, next);
                    if let Some(v) = v {
                        let plus = v.ends_with('+');
                        if let Ok(n) = v.trim_end_matches('+').parse::<u16>() {
                            look.min_height = n;
                            look.min_height_plus = plus;
                        }
                    }
                    i += usize::from(sep);
                }
                "--no-scrollbar" => look.scrollbar = false,
                "--bind" => {
                    let (v, sep) = flag_value(a, next);
                    if let Some(v) = v {
                        look.binds.extend(parse_bind_spec(&v));
                    }
                    i += usize::from(sep);
                }
                _ => {}
            }
            i += 1;
        }
        look
    }

    /// The command and flags a child process starts with: `--with-shell` if
    /// given, else `$SHELL -c`, else `sh -c` — fzf's rule, so a preview that
    /// works under `fzf` works here.
    pub fn shell(&self) -> Vec<String> {
        if let Some(w) = &self.with_shell {
            let words = shell_split(w);
            if !words.is_empty() {
                return words;
            }
        }
        match std::env::var("SHELL") {
            Ok(sh) if !sh.is_empty() => vec![sh, "-c".to_string()],
            _ => vec!["sh".to_string(), "-c".to_string()],
        }
    }

    /// The actions bound to `key`, if any. Later bindings win, so the command
    /// line overrides `$FZF_DEFAULT_OPTS` for the same key.
    pub fn bound(&self, key: Key) -> Option<&[Action]> {
        self.binds
            .iter()
            .rev()
            .find(|(k, _)| *k == key)
            .map(|(_, a)| a.as_slice())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_split_keeps_quoted_prompt_intact() {
        // The real $FZF_DEFAULT_OPTS from a themed zsh setup: the prompt has a
        // trailing space and parentheses, so quoting must survive the split.
        let v = shell_split("--prompt='<<)ZPWR(>> ' --reverse --height 100%");
        assert_eq!(
            v,
            vec!["--prompt=<<)ZPWR(>> ", "--reverse", "--height", "100%"]
        );
    }

    #[test]
    fn shell_split_handles_double_quotes_and_escapes() {
        let v = shell_split(r#"--prompt="a b" --header=c\ d"#);
        assert_eq!(v, vec!["--prompt=a b", "--header=c d"]);
        // An empty quoted value is a word, not nothing.
        assert_eq!(shell_split("--prompt=''"), vec!["--prompt="]);
    }

    #[test]
    fn later_options_win() {
        // env options are spliced first, so a command-line flag must override.
        let args: Vec<String> = ["--layout=reverse", "--layout=default"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(Look::parse(&args).layout, Layout::Default);
    }

    #[test]
    fn parses_layout_border_info_pointer() {
        let args: Vec<String> = [
            "--reverse",
            "--border",
            "--info=inline",
            "--pointer",
            ">",
            "--marker",
            "*",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let look = Look::parse(&args);
        assert_eq!(look.layout, Layout::Reverse);
        assert_eq!(look.border, Some((BorderType::Rounded, Borders::ALL)));
        assert_eq!(look.info, Info::Inline(" < ".to_string()));
        assert_eq!(look.pointer, ">");
        assert_eq!(look.marker, "*");
    }

    #[test]
    fn color_spec_overrides_scheme_slots() {
        let args: Vec<String> = ["--color=light,hl:#ff0000,fg+:2:bold:underline"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let c = Look::parse(&args).colors;
        // The base scheme applied…
        assert_eq!(c.prompt, Ent::b(25));
        // …then the per-slot overrides on top of it.
        assert_eq!(c.hl.color, Some(Color::Rgb(0xff, 0, 0)));
        // `2` is a palette index, not a name — fzf writes it as `38;5;2`.
        assert_eq!(c.fg_plus.color, Some(Color::Indexed(2)));
        assert!(c.fg_plus.attrs.contains(Modifier::BOLD));
        assert!(c.fg_plus.attrs.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn binds_map_fzf_actions() {
        let args: Vec<String> =
            ["--bind=ctrl-n:page-down,ctrl-p:page-up,tab:toggle+down,ctrl-j:down,ctrl-k:up"]
                .iter()
                .map(|s| s.to_string())
                .collect();
        let look = Look::parse(&args);
        assert_eq!(look.bound(Key::Ctrl('n')), Some(&[Action::PageDown][..]));
        assert_eq!(look.bound(Key::Ctrl('p')), Some(&[Action::PageUp][..]));
        assert_eq!(
            look.bound(Key::Tab),
            Some(&[Action::Toggle, Action::Down][..])
        );
        assert_eq!(look.bound(Key::Ctrl('j')), Some(&[Action::Down][..]));
        assert_eq!(look.bound(Key::Ctrl('k')), Some(&[Action::Up][..]));
    }

    #[test]
    fn unsupported_action_drops_only_that_binding() {
        // `execute(...)` has no arb analog: that key keeps its built-in meaning,
        // while the other binding in the same spec still applies.
        let args: Vec<String> = ["--bind=ctrl-o:execute(vim {}),ctrl-k:up"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let look = Look::parse(&args);
        assert_eq!(look.bound(Key::Ctrl('o')), None);
        assert_eq!(look.bound(Key::Ctrl('k')), Some(&[Action::Up][..]));
    }

    #[test]
    fn preview_window_defaults_are_fzfs() {
        // fzf(1): "POSITION: (default: right)", half the window.
        let w = PreviewWindow::default();
        assert_eq!(w.pos, PreviewPos::Right);
        assert_eq!(w.size, PreviewSize::Percent(50));
        assert!(!w.hidden && !w.wrap);
    }

    #[test]
    fn preview_window_takes_both_separators_in_any_order() {
        // The modern comma form and the older colon form are both in the wild.
        let a = PreviewWindow::parse("down,30%,wrap");
        assert_eq!(a.pos, PreviewPos::Down);
        assert_eq!(a.size, PreviewSize::Percent(30));
        assert!(a.wrap);
        let b = PreviewWindow::parse("right:55%");
        assert_eq!(b.pos, PreviewPos::Right);
        assert_eq!(b.size, PreviewSize::Percent(55));
        // Order does not matter to fzf, so it must not matter here.
        assert_eq!(PreviewWindow::parse("20,up"), PreviewWindow::parse("up,20"));
        assert_eq!(PreviewWindow::parse("up,20").size, PreviewSize::Cells(20));
    }

    #[test]
    fn unmodeled_parts_of_the_spec_are_ignored_not_rejected() {
        // A spec written for fzf must never fail here just because arb does not
        // act on every part of the grammar.
        let w = PreviewWindow::parse("left,40%,border-sharp,follow,cycle,+3/2,~2,noinfo");
        assert_eq!(w.pos, PreviewPos::Left);
        assert_eq!(w.size, PreviewSize::Percent(40));
    }

    #[test]
    fn hidden_and_zero_size_draw_no_pane() {
        use ratatui::layout::Rect;
        let body = Rect { x: 0, y: 0, width: 100, height: 30 };
        let (list, pane) = PreviewWindow::parse("hidden").split(body);
        assert!(pane.is_none(), "hidden draws nothing");
        assert_eq!(list, body, "and the list takes the whole body");
        // fzf(1): "If size is given as 0, preview window will not be visible,
        // but fzf will still execute the command in the background."
        assert!(PreviewWindow::parse("right,0").split(body).1.is_none());
        assert!(PreviewWindow::parse("right,0%").split(body).1.is_none());
    }

    #[test]
    fn the_split_places_and_sizes_the_pane() {
        use ratatui::layout::Rect;
        let body = Rect { x: 0, y: 0, width: 100, height: 30 };

        let (list, pane) = PreviewWindow::parse("right,60%").split(body);
        let pane = pane.expect("drawn");
        assert_eq!((list.width, pane.width), (40, 60));
        assert_eq!(pane.x, 40, "right means after the list");

        let (list, pane) = PreviewWindow::parse("left,25").split(body);
        let pane = pane.expect("drawn");
        assert_eq!((pane.width, list.width), (25, 75));
        assert_eq!(pane.x, 0, "left means before the list");

        let (list, pane) = PreviewWindow::parse("up,10").split(body);
        let pane = pane.expect("drawn");
        assert_eq!((pane.height, list.height), (10, 20));
        assert_eq!(pane.y, 0);

        let (list, pane) = PreviewWindow::parse("down,3").split(body);
        let pane = pane.expect("drawn");
        assert_eq!((list.height, pane.height), (27, 3));
        assert_eq!(pane.y, 27);
    }

    #[test]
    fn the_list_always_keeps_a_cell() {
        use ratatui::layout::Rect;
        let body = Rect { x: 0, y: 0, width: 10, height: 4 };
        // An oversized request must not leave zero columns to pick from.
        let (list, pane) = PreviewWindow::parse("right,100%").split(body);
        assert_eq!(list.width, 1);
        assert_eq!(pane.expect("drawn").width, 9);
    }

    #[test]
    fn look_parses_the_flag_in_both_spellings() {
        let attached = Look::parse(&[
            "arb".into(),
            "--fzf".into(),
            "--preview-window=up,7".into(),
        ]);
        assert_eq!(attached.preview_window.pos, PreviewPos::Up);
        assert_eq!(attached.preview_window.size, PreviewSize::Cells(7));
        let separate = Look::parse(&[
            "arb".into(),
            "--fzf".into(),
            "--preview-window".into(),
            "down,20%".into(),
        ]);
        assert_eq!(separate.preview_window.pos, PreviewPos::Down);
        assert_eq!(separate.preview_window.size, PreviewSize::Percent(20));
    }

}
