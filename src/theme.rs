//! Color themes — the 31 storageshower palettes shared by the sibling `iftoprs`
//! and `htoprs` HUD apps, ported verbatim, plus a user `custom` palette.
//!
//! A theme is a **6-color palette** of 256-color terminal indices
//! `(c1..c6) = (primary, accent, alt, mid, dim, bg)`, exactly as the sibling apps
//! store it. A [`Palette`] exposes those slots by role so arb's widgets recolor
//! as one system when the active `theme` changes; a widget's `-color <slot>`
//! (accent/primary/alt/mid/dim/bg) resolves through it. With no `theme` directive
//! the palette is `None` and color resolution stays exactly as before (cyan
//! default + the fixed named colors) — themes are purely additive.

use ratatui::style::Color;

/// A resolved 6-color theme palette (256-color terminal indices). `Copy` — 6
/// bytes, passed by value into the render functions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    /// `(c1, c2, c3, c4, c5, c6)` = (primary, accent, alt, mid, dim, bg).
    pub c: [u8; 6],
}

impl Palette {
    /// `c2` — the bright accent (default widget border/fill, focused controls).
    pub fn accent(&self) -> Color {
        Color::Indexed(self.c[1])
    }
    /// `c1` — the primary label color.
    pub fn primary(&self) -> Color {
        Color::Indexed(self.c[0])
    }
    /// `c3` — a distinct alternate hue.
    pub fn alt(&self) -> Color {
        Color::Indexed(self.c[2])
    }
    /// `c4` — a mid tone.
    pub fn mid(&self) -> Color {
        Color::Indexed(self.c[3])
    }
    /// `c5` — a dim tone.
    pub fn dim(&self) -> Color {
        Color::Indexed(self.c[4])
    }
    /// `c6` — the darkest (backgrounds).
    pub fn bg(&self) -> Color {
        Color::Indexed(self.c[5])
    }
    /// Resolve a `-color <slot>` name to a palette color, if it names a slot.
    pub fn slot(&self, name: &str) -> Option<Color> {
        Some(match name {
            "accent" => self.accent(),
            "primary" => self.primary(),
            "alt" => self.alt(),
            "mid" => self.mid(),
            "dim" => self.dim(),
            "bg" | "dark" | "background" => self.bg(),
            _ => return None,
        })
    }
}

/// The 31 built-in themes, in display order — `(kebab-name, [c1..c6])`. Palettes
/// are the storageshower values ported verbatim from `iftoprs`/`htoprs`.
pub const THEMES: &[(&str, [u8; 6])] = &[
    ("neon-sprawl", [27, 48, 135, 141, 63, 99]),
    ("acid-rain", [28, 46, 34, 40, 22, 35]),
    ("ice-breaker", [19, 39, 25, 33, 21, 32]),
    ("synth-wave", [91, 177, 128, 134, 93, 97]),
    ("rust-belt", [172, 214, 178, 220, 166, 130]),
    ("ghost-wire", [37, 50, 44, 87, 30, 23]),
    ("red-sector", [160, 203, 196, 210, 124, 88]),
    ("sakura-den", [175, 218, 182, 225, 169, 132]),
    ("data-stream", [22, 46, 28, 119, 34, 22]),
    ("solar-flare", [202, 220, 196, 213, 160, 125]),
    ("neon-noir", [201, 231, 93, 219, 57, 53]),
    ("chrome-heart", [250, 255, 246, 253, 243, 239]),
    ("blade-runner", [208, 37, 166, 73, 130, 23]),
    ("void-walker", [55, 99, 54, 141, 92, 17]),
    ("toxic-waste", [118, 190, 154, 226, 82, 58]),
    ("cyber-frost", [159, 195, 153, 189, 111, 67]),
    ("plasma-core", [199, 213, 163, 207, 126, 89]),
    ("steel-nerve", [68, 110, 60, 146, 24, 236]),
    ("dark-signal", [30, 43, 23, 79, 29, 16]),
    ("glitch-pop", [201, 51, 226, 47, 196, 21]),
    ("holo-shift", [123, 219, 159, 183, 87, 133]),
    ("night-city", [214, 227, 209, 223, 172, 94]),
    ("deep-net", [19, 33, 17, 75, 26, 16]),
    ("laser-grid", [46, 201, 51, 226, 196, 21]),
    ("quantum-flux", [135, 75, 171, 111, 98, 61]),
    ("bio-hazard", [148, 184, 106, 192, 64, 22]),
    ("darkwave", [53, 140, 89, 176, 127, 52]),
    ("overlock", [196, 208, 160, 214, 124, 52]),
    ("megacorp", [252, 39, 245, 81, 242, 236]),
    ("zaibatsu", [167, 216, 131, 224, 95, 52]),
    ("iftopcolor", [21, 46, 28, 48, 33, 19]),
];

/// Normalize a theme name for lenient matching: lowercase, drop `-`/`_`/space so
/// `neon-noir`, `NeonNoir`, `neon_noir` and `neonnoir` all resolve.
fn norm(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Resolve a theme name to its palette (lenient matching); `None` if unknown.
pub fn by_name(name: &str) -> Option<Palette> {
    let want = norm(name);
    THEMES
        .iter()
        .find(|(n, _)| norm(n) == want)
        .map(|&(_, c)| Palette { c })
}

/// A `theme custom c1 c2 c3 c4 c5 c6` palette from six 256-color indices.
pub fn custom(c: [u8; 6]) -> Palette {
    Palette { c }
}

/// Every theme's kebab-name, in display order (for `--list-themes` / cycling).
pub fn names() -> impl Iterator<Item = &'static str> {
    THEMES.iter().map(|&(n, _)| n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thirty_one_themes_all_distinct() {
        assert_eq!(THEMES.len(), 31);
        let mut seen = std::collections::HashSet::new();
        for (n, _) in THEMES {
            assert!(seen.insert(*n), "duplicate theme name {n}");
        }
    }

    #[test]
    fn lenient_name_matching() {
        let p = by_name("neon-noir").unwrap();
        assert_eq!(by_name("NeonNoir"), Some(p));
        assert_eq!(by_name("neon_noir"), Some(p));
        assert_eq!(by_name("neonnoir"), Some(p));
        assert_eq!(by_name("NEON NOIR"), Some(p));
        assert_eq!(by_name("nope"), None);
    }

    #[test]
    fn palette_slots_map_to_indices() {
        // neon-noir = [201, 231, 93, 219, 57, 53]
        let p = by_name("neon-noir").unwrap();
        assert_eq!(p.accent(), Color::Indexed(231)); // c2
        assert_eq!(p.primary(), Color::Indexed(201)); // c1
        assert_eq!(p.bg(), Color::Indexed(53)); // c6
        assert_eq!(p.slot("dim"), Some(Color::Indexed(57))); // c5
        assert_eq!(p.slot("bogus"), None);
    }

    #[test]
    fn custom_palette_from_six_indices() {
        let p = custom([1, 2, 3, 4, 5, 6]);
        assert_eq!(p.accent(), Color::Indexed(2));
        assert_eq!(p.bg(), Color::Indexed(6));
    }
}
