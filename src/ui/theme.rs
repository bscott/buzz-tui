//! The colour palette.
//!
//! Every colour the interface draws comes from here. No other module may write
//! a colour literal, because a palette that leaks into widget code cannot be
//! swapped at runtime and drifts out of tune one hard-coded shade at a time.
//!
//! The seventeen tokens are semantic rather than descriptive: a widget asks for
//! `accent` or `surface0`, never for "blue" or "dark grey". Themes therefore
//! differ only in what those tokens resolve to, and adding one is a matter of
//! filling in the same slots. The named [`Style`] constructors below exist for
//! the same reason — the renderer names an intent, not a colour pairing.
//!
//! A user can retune any single token from the `[theme]` table in
//! `config.toml`. A typo there is a cosmetic mistake, so overrides report
//! diagnostics and leave the rest of the palette standing rather than refusing
//! to start.

use std::collections::BTreeMap;
use std::str::FromStr;

use anyhow::{Result, anyhow, bail};
use ratatui::style::{Color, Modifier, Style};

/// The seventeen semantic colour tokens.
///
/// The surface tokens are ordered by prominence rather than by brightness, so
/// that light and dark themes can both satisfy them: `panel_bg` is the chrome
/// behind everything, `surface_dim` marks a region that is merely active,
/// `surface0` marks the keyboard cursor, and `surface1` is the strongest fill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub accent: Color,
    pub panel_bg: Color,
    pub sidebar_bg: Color,
    pub surface0: Color,
    pub surface1: Color,
    pub surface_dim: Color,
    pub overlay0: Color,
    pub overlay1: Color,
    pub text: Color,
    pub subtext0: Color,
    pub mauve: Color,
    pub green: Color,
    pub yellow: Color,
    pub red: Color,
    pub teal: Color,
    pub blue: Color,
    pub peach: Color,
}

impl Default for Palette {
    fn default() -> Self {
        Self::catppuccin()
    }
}

/// Folds the spellings users actually type into one comparable form, so that
/// `Tokyo_Night` and `tokyo-night` are the same theme and `Panel-BG` is the
/// same token as `panel_bg`.
fn normalise(name: &str) -> String {
    name.trim().to_ascii_lowercase().replace([' ', '_'], "-")
}

impl Palette {
    /// The themes offered by the cycle key, in cycling order.
    pub const NAMES: &'static [&'static str] =
        &["buzz", "tokyo-night", "gruvbox", "paper", "terminal"];

    /// Resolves a configured or aliased theme name to its canonical entry in
    /// [`Palette::NAMES`].
    fn canonical(name: &str) -> Option<&'static str> {
        Some(match normalise(name).as_str() {
            "buzz" | "catppuccin" | "catppuccin-mocha" | "mocha" => "buzz",
            "tokyo-night" | "tokyonight" => "tokyo-night",
            "gruvbox" => "gruvbox",
            "paper" | "light" => "paper",
            "terminal" => "terminal",
            _ => return None,
        })
    }

    /// Looks a theme up by name or alias, case-insensitively.
    pub fn by_name(name: &str) -> Option<Self> {
        match Self::canonical(name)? {
            "buzz" => Some(Self::catppuccin()),
            "tokyo-night" => Some(Self::tokyo_night()),
            "gruvbox" => Some(Self::gruvbox()),
            "paper" => Some(Self::paper()),
            "terminal" => Some(Self::terminal()),
            _ => None,
        }
    }

    /// The next theme in cycling order, wrapping at the end. An unrecognised
    /// current name starts the cycle from the beginning rather than sticking.
    pub fn next_theme(current: &str) -> &'static str {
        let index = Self::canonical(current)
            .and_then(|name| Self::NAMES.iter().position(|entry| *entry == name));
        match index {
            Some(index) => Self::NAMES[(index + 1) % Self::NAMES.len()],
            None => Self::NAMES[0],
        }
    }

    /// Catppuccin Mocha, the shipped default.
    pub fn catppuccin() -> Self {
        Self {
            accent: Color::Rgb(0x89, 0xb4, 0xfa),
            panel_bg: Color::Rgb(0x18, 0x18, 0x25),
            sidebar_bg: Color::Reset,
            surface0: Color::Rgb(0x31, 0x32, 0x44),
            surface1: Color::Rgb(0x45, 0x47, 0x5a),
            surface_dim: Color::Rgb(0x1e, 0x1e, 0x2e),
            overlay0: Color::Rgb(0x6c, 0x70, 0x86),
            overlay1: Color::Rgb(0x7f, 0x84, 0x9c),
            text: Color::Rgb(0xcd, 0xd6, 0xf4),
            subtext0: Color::Rgb(0xa6, 0xad, 0xc8),
            mauve: Color::Rgb(0xcb, 0xa6, 0xf7),
            green: Color::Rgb(0xa6, 0xe3, 0xa1),
            yellow: Color::Rgb(0xf9, 0xe2, 0xaf),
            red: Color::Rgb(0xf3, 0x8b, 0xa8),
            teal: Color::Rgb(0x94, 0xe2, 0xd5),
            blue: Color::Rgb(0x89, 0xb4, 0xfa),
            peach: Color::Rgb(0xfa, 0xb3, 0x87),
        }
    }

    pub fn tokyo_night() -> Self {
        Self {
            accent: Color::Rgb(0x7a, 0xa2, 0xf7),
            panel_bg: Color::Rgb(0x1a, 0x1b, 0x26),
            sidebar_bg: Color::Reset,
            surface0: Color::Rgb(0x24, 0x28, 0x3b),
            surface1: Color::Rgb(0x41, 0x48, 0x68),
            surface_dim: Color::Rgb(0x16, 0x16, 0x1e),
            overlay0: Color::Rgb(0x56, 0x5f, 0x89),
            overlay1: Color::Rgb(0x69, 0x71, 0x96),
            text: Color::Rgb(0xc0, 0xca, 0xf5),
            subtext0: Color::Rgb(0xa9, 0xb1, 0xd6),
            mauve: Color::Rgb(0xbb, 0x9a, 0xf7),
            green: Color::Rgb(0x9e, 0xce, 0x6a),
            yellow: Color::Rgb(0xe0, 0xaf, 0x68),
            red: Color::Rgb(0xf7, 0x76, 0x8e),
            teal: Color::Rgb(0x7d, 0xcf, 0xff),
            blue: Color::Rgb(0x7a, 0xa2, 0xf7),
            peach: Color::Rgb(0xff, 0x9e, 0x64),
        }
    }

    /// Gruvbox dark, using the hard background and the bright accent row so
    /// that coloured text stays legible against it.
    pub fn gruvbox() -> Self {
        Self {
            accent: Color::Rgb(0x83, 0xa5, 0x98),
            panel_bg: Color::Rgb(0x1d, 0x20, 0x21),
            sidebar_bg: Color::Reset,
            surface0: Color::Rgb(0x3c, 0x38, 0x36),
            surface1: Color::Rgb(0x50, 0x49, 0x45),
            surface_dim: Color::Rgb(0x28, 0x28, 0x28),
            overlay0: Color::Rgb(0x92, 0x83, 0x74),
            overlay1: Color::Rgb(0xa8, 0x99, 0x84),
            text: Color::Rgb(0xeb, 0xdb, 0xb2),
            subtext0: Color::Rgb(0xbd, 0xae, 0x93),
            mauve: Color::Rgb(0xd3, 0x86, 0x9b),
            green: Color::Rgb(0xb8, 0xbb, 0x26),
            yellow: Color::Rgb(0xfa, 0xbd, 0x2f),
            red: Color::Rgb(0xfb, 0x49, 0x34),
            teal: Color::Rgb(0x8e, 0xc0, 0x7c),
            blue: Color::Rgb(0x83, 0xa5, 0x98),
            peach: Color::Rgb(0xfe, 0x80, 0x19),
        }
    }

    /// A light theme for daylight and for terminals with a white background.
    /// The accents are darkened well past their dark-theme counterparts, since
    /// a pastel that sings on near-black vanishes on near-white.
    pub fn paper() -> Self {
        Self {
            accent: Color::Rgb(0x1e, 0x66, 0xf5),
            panel_bg: Color::Rgb(0xef, 0xf1, 0xf5),
            sidebar_bg: Color::Reset,
            surface0: Color::Rgb(0xcc, 0xd0, 0xda),
            surface1: Color::Rgb(0xbc, 0xc0, 0xcc),
            surface_dim: Color::Rgb(0xdc, 0xe0, 0xe8),
            overlay0: Color::Rgb(0x8c, 0x8f, 0xa1),
            overlay1: Color::Rgb(0x7c, 0x7f, 0x93),
            text: Color::Rgb(0x4c, 0x4f, 0x69),
            subtext0: Color::Rgb(0x6c, 0x6f, 0x85),
            mauve: Color::Rgb(0x88, 0x39, 0xef),
            green: Color::Rgb(0x40, 0xa0, 0x2b),
            yellow: Color::Rgb(0xdf, 0x8e, 0x1d),
            red: Color::Rgb(0xd2, 0x0f, 0x39),
            teal: Color::Rgb(0x17, 0x92, 0x99),
            blue: Color::Rgb(0x1e, 0x66, 0xf5),
            peach: Color::Rgb(0xfe, 0x64, 0x0b),
        }
    }

    /// Defers to whatever the terminal is already configured with. Backgrounds
    /// reset to the host's own, and the sixteen ANSI slots carry the accents,
    /// so a carefully tuned terminal theme is respected instead of overridden.
    pub fn terminal() -> Self {
        Self {
            accent: Color::Cyan,
            panel_bg: Color::Reset,
            sidebar_bg: Color::Reset,
            surface0: Color::DarkGray,
            surface1: Color::Gray,
            // Doubles as the chip foreground once `panel_bg` resets, so it has
            // to read against both the host background and the accent fill.
            surface_dim: Color::DarkGray,
            overlay0: Color::DarkGray,
            overlay1: Color::Gray,
            text: Color::Reset,
            subtext0: Color::Gray,
            mauve: Color::Magenta,
            green: Color::Green,
            yellow: Color::Yellow,
            red: Color::Red,
            teal: Color::LightCyan,
            blue: Color::Blue,
            peach: Color::LightRed,
        }
    }

    /// Applies `[theme]` overrides from config; returns human-readable
    /// diagnostics for unparsable entries rather than failing.
    pub fn apply_overrides(&mut self, overrides: &BTreeMap<String, String>) -> Vec<String> {
        let mut diagnostics = Vec::new();
        for (key, value) in overrides {
            let slot = match normalise(key).as_str() {
                "accent" => &mut self.accent,
                "panel-bg" => &mut self.panel_bg,
                "sidebar-bg" => &mut self.sidebar_bg,
                "surface0" => &mut self.surface0,
                "surface1" => &mut self.surface1,
                "surface-dim" => &mut self.surface_dim,
                "overlay0" => &mut self.overlay0,
                "overlay1" => &mut self.overlay1,
                "text" => &mut self.text,
                "subtext0" => &mut self.subtext0,
                "mauve" => &mut self.mauve,
                "green" => &mut self.green,
                "yellow" => &mut self.yellow,
                "red" => &mut self.red,
                "teal" => &mut self.teal,
                "blue" => &mut self.blue,
                "peach" => &mut self.peach,
                _ => {
                    diagnostics.push(format!("unknown theme token `{key}`"));
                    continue;
                }
            };
            match parse_color(value) {
                Ok(color) => *slot = color,
                Err(err) => diagnostics.push(format!("theme token `{key}`: {err}")),
            }
        }
        diagnostics
    }

    /// Foreground for text drawn on an `accent` background: `panel_bg`, falling
    /// back to `surface_dim` when `panel_bg` is `Reset`.
    pub fn contrast_fg(&self) -> Color {
        match self.panel_bg {
            Color::Reset => self.surface_dim,
            other => other,
        }
    }
}

/// Named semantic styles. Every one of these is used by the renderer, so keep
/// them exhaustive and obvious.
impl Palette {
    pub fn base(&self) -> Style {
        Style::new().fg(self.text)
    }

    pub fn dim(&self) -> Style {
        Style::new().fg(self.overlay0)
    }

    pub fn muted(&self) -> Style {
        Style::new().fg(self.subtext0)
    }

    pub fn strong(&self) -> Style {
        Style::new().fg(self.text).add_modifier(Modifier::BOLD)
    }

    pub fn accent_text(&self) -> Style {
        Style::new().fg(self.accent)
    }

    pub fn accent_strong(&self) -> Style {
        Style::new().fg(self.accent).add_modifier(Modifier::BOLD)
    }

    pub fn chip(&self) -> Style {
        Style::new()
            .bg(self.accent)
            .fg(self.contrast_fg())
            .add_modifier(Modifier::BOLD)
    }

    pub fn chip_alt(&self) -> Style {
        Style::new()
            .bg(self.mauve)
            .fg(self.contrast_fg())
            .add_modifier(Modifier::BOLD)
    }

    /// Where the keyboard cursor is.
    pub fn selected(&self) -> Style {
        Style::new()
            .bg(self.surface0)
            .fg(self.text)
            .add_modifier(Modifier::BOLD)
    }

    /// What is currently open, which is not always what is selected.
    pub fn active(&self) -> Style {
        Style::new()
            .bg(self.surface_dim)
            .fg(self.text)
            .add_modifier(Modifier::BOLD)
    }

    pub fn panel(&self) -> Style {
        Style::new().bg(self.panel_bg)
    }

    pub fn border(&self) -> Style {
        Style::new().fg(self.surface_dim)
    }

    pub fn border_focused(&self) -> Style {
        Style::new().fg(self.accent)
    }

    pub fn error(&self) -> Style {
        Style::new().fg(self.red)
    }

    pub fn warn(&self) -> Style {
        Style::new().fg(self.yellow)
    }

    pub fn success(&self) -> Style {
        Style::new().fg(self.green)
    }

    pub fn pending(&self) -> Style {
        Style::new().fg(self.overlay0).add_modifier(Modifier::DIM)
    }

    pub fn mention(&self) -> Style {
        Style::new().fg(self.peach).add_modifier(Modifier::BOLD)
    }

    pub fn link(&self) -> Style {
        Style::new()
            .fg(self.blue)
            .add_modifier(Modifier::UNDERLINED)
    }

    pub fn code(&self) -> Style {
        Style::new().fg(self.teal).bg(self.surface_dim)
    }

    pub fn quote(&self) -> Style {
        Style::new().fg(self.overlay1).add_modifier(Modifier::ITALIC)
    }
}

/// Accepts `#rrggbb`, `#rgb`, `rgb(r, g, b)`, an ANSI index `0`-`255`, the
/// CSS-ish names ratatui knows, and `reset`/`default`/`none`/`transparent`.
///
/// The spellings are deliberately generous: this parses hand-written config,
/// where a value copied out of a web palette or a colour picker should just
/// work.
pub fn parse_color(text: &str) -> Result<Color> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        bail!("empty colour value");
    }
    let lower = trimmed.to_ascii_lowercase();
    if matches!(lower.as_str(), "reset" | "default" | "none" | "transparent") {
        return Ok(Color::Reset);
    }
    if let Some(body) = lower.strip_prefix('#') {
        return parse_hex(body, trimmed);
    }
    if let Some(body) = lower
        .strip_prefix("rgb(")
        .and_then(|rest| rest.strip_suffix(')'))
    {
        return parse_rgb(body, trimmed);
    }
    // ratatui already knows the ANSI names and bare indices; reuse it so the
    // accepted vocabulary matches what the rest of the ecosystem writes.
    Color::from_str(&lower).map_err(|_| {
        anyhow!(
            "`{trimmed}` is not a colour; expected `#rrggbb`, `#rgb`, `rgb(r, g, b)`, an index `0`-`255`, or a colour name"
        )
    })
}

fn parse_hex(body: &str, original: &str) -> Result<Color> {
    let digits = body.as_bytes();
    let nibble = |byte: u8| -> Result<u8> {
        char::from(byte)
            .to_digit(16)
            .map(|value| value as u8)
            .ok_or_else(|| anyhow!("`{original}` is not a hex colour; `{}` is not a hex digit", char::from(byte)))
    };
    match digits.len() {
        // The short form doubles each digit, so `#abc` is `#aabbcc`.
        3 => Ok(Color::Rgb(
            nibble(digits[0])? * 0x11,
            nibble(digits[1])? * 0x11,
            nibble(digits[2])? * 0x11,
        )),
        6 => Ok(Color::Rgb(
            nibble(digits[0])? * 0x10 + nibble(digits[1])?,
            nibble(digits[2])? * 0x10 + nibble(digits[3])?,
            nibble(digits[4])? * 0x10 + nibble(digits[5])?,
        )),
        _ => bail!("`{original}` is not a hex colour; expected 3 or 6 digits after `#`"),
    }
}

fn parse_rgb(body: &str, original: &str) -> Result<Color> {
    let mut channels = [0u8; 3];
    let mut parts = body.split(',');
    for channel in &mut channels {
        let part = parts
            .next()
            .ok_or_else(|| anyhow!("`{original}` needs three channels, as `rgb(r, g, b)`"))?
            .trim();
        *channel = part
            .parse::<u8>()
            .map_err(|_| anyhow!("`{original}` has a channel outside `0`-`255`: `{part}`"))?;
    }
    if parts.next().is_some() {
        bail!("`{original}` needs three channels, as `rgb(r, g, b)`");
    }
    Ok(Color::Rgb(channels[0], channels[1], channels[2]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn overrides(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    /// Rough perceptual weighting, enough to tell a light theme from a dark one.
    fn luminance(color: Color) -> f32 {
        match color {
            Color::Rgb(r, g, b) => {
                0.2126 * f32::from(r) + 0.7152 * f32::from(g) + 0.0722 * f32::from(b)
            }
            other => panic!("expected an rgb colour, got {other:?}"),
        }
    }

    #[test]
    fn every_listed_theme_resolves() {
        for name in Palette::NAMES {
            assert!(
                Palette::by_name(name).is_some(),
                "`{name}` is offered by the cycle key but does not resolve"
            );
        }
    }

    #[test]
    fn aliases_and_casing_resolve_to_the_same_palette() {
        for alias in ["catppuccin", "mocha", "CATPPUCCIN", "Catppuccin-Mocha"] {
            assert_eq!(Palette::by_name(alias), Some(Palette::catppuccin()));
        }
        for alias in ["tokyonight", "Tokyo Night", "TOKYO-NIGHT", "tokyo_night"] {
            assert_eq!(Palette::by_name(alias), Some(Palette::tokyo_night()));
        }
        assert_eq!(Palette::by_name("light"), Some(Palette::paper()));
        assert_eq!(Palette::by_name(" Paper "), Some(Palette::paper()));
        assert_eq!(Palette::by_name("TERMINAL"), Some(Palette::terminal()));
    }

    #[test]
    fn unknown_theme_names_do_not_resolve() {
        assert_eq!(Palette::by_name("solarized"), None);
        assert_eq!(Palette::by_name(""), None);
    }

    #[test]
    fn cycling_visits_every_theme_and_wraps() {
        let mut seen = Vec::new();
        let mut current = Palette::NAMES[0];
        for _ in 0..Palette::NAMES.len() {
            seen.push(current);
            current = Palette::next_theme(current);
        }
        assert_eq!(seen, Palette::NAMES.to_vec());
        assert_eq!(current, Palette::NAMES[0], "the cycle must wrap");
    }

    #[test]
    fn cycling_starts_over_from_an_unknown_or_aliased_name() {
        assert_eq!(Palette::next_theme("solarized"), Palette::NAMES[0]);
        // An alias cycles from the theme it names, not from the start.
        assert_eq!(Palette::next_theme("mocha"), "tokyo-night");
    }

    #[test]
    fn colours_parse_from_every_documented_spelling() {
        assert_eq!(parse_color("#89b4fa").unwrap(), Color::Rgb(0x89, 0xb4, 0xfa));
        assert_eq!(parse_color("#ABC").unwrap(), Color::Rgb(0xaa, 0xbb, 0xcc));
        assert_eq!(parse_color("  #abc  ").unwrap(), Color::Rgb(0xaa, 0xbb, 0xcc));
        assert_eq!(parse_color("rgb(1,2,3)").unwrap(), Color::Rgb(1, 2, 3));
        assert_eq!(parse_color("rgb( 1 , 2 , 3 )").unwrap(), Color::Rgb(1, 2, 3));
        assert_eq!(parse_color("rgb(255, 255, 255)").unwrap(), Color::Rgb(255, 255, 255));
        assert_eq!(parse_color("200").unwrap(), Color::Indexed(200));
        assert_eq!(parse_color("0").unwrap(), Color::Indexed(0));
        assert_eq!(parse_color("255").unwrap(), Color::Indexed(255));
        assert_eq!(parse_color("LightBlue").unwrap(), Color::LightBlue);
        for keyword in ["reset", "default", "none", "transparent", "Reset"] {
            assert_eq!(parse_color(keyword).unwrap(), Color::Reset);
        }
    }

    #[test]
    fn malformed_colours_are_rejected() {
        for bad in [
            "#gg0000",
            "#12345",
            "#",
            "rgb(300,0,0)",
            "rgb(1,2)",
            "rgb(1,2,3,4)",
            "rgb(,,)",
            "256",
            "-1",
            "",
            "   ",
            "chartreuse",
        ] {
            assert!(
                parse_color(bad).is_err(),
                "`{bad}` should not parse as a colour"
            );
        }
    }

    #[test]
    fn overrides_patch_only_the_tokens_they_name() {
        let mut palette = Palette::catppuccin();
        let baseline = palette;
        let diagnostics = palette.apply_overrides(&overrides(&[
            ("accent", "#ff0000"),
            ("panel-bg", "rgb(1, 2, 3)"),
            ("surface_dim", "reset"),
        ]));
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_eq!(palette.accent, Color::Rgb(0xff, 0, 0));
        assert_eq!(palette.panel_bg, Color::Rgb(1, 2, 3));
        assert_eq!(palette.surface_dim, Color::Reset);
        // Everything else is untouched.
        assert_eq!(palette.text, baseline.text);
        assert_eq!(palette.blue, baseline.blue);
        assert_eq!(palette.surface0, baseline.surface0);
    }

    #[test]
    fn bad_overrides_are_reported_and_survived() {
        let mut palette = Palette::catppuccin();
        let baseline = palette;
        let diagnostics = palette.apply_overrides(&overrides(&[
            ("accent", "#ff0000"),
            ("banana", "#00ff00"),
            ("red", "not-a-colour"),
        ]));
        assert_eq!(diagnostics.len(), 2, "{diagnostics:?}");
        assert!(
            diagnostics.iter().any(|d| d.contains("banana")),
            "an unknown token must be named: {diagnostics:?}"
        );
        assert!(
            diagnostics
                .iter()
                .any(|d| d.contains("red") && d.contains("not-a-colour")),
            "an unparsable value must be reported: {diagnostics:?}"
        );
        // The good override still landed and the bad one changed nothing.
        assert_eq!(palette.accent, Color::Rgb(0xff, 0, 0));
        assert_eq!(palette.red, baseline.red);
    }

    #[test]
    fn contrast_falls_back_when_the_panel_is_transparent() {
        let terminal = Palette::terminal();
        assert_eq!(terminal.panel_bg, Color::Reset);
        assert_eq!(terminal.contrast_fg(), terminal.surface_dim);

        let mocha = Palette::catppuccin();
        assert_eq!(mocha.contrast_fg(), mocha.panel_bg);
        assert_eq!(Palette::paper().contrast_fg(), Palette::paper().panel_bg);
    }

    #[test]
    fn chips_never_paint_text_in_their_own_background() {
        for name in Palette::NAMES {
            let palette = Palette::by_name(name).unwrap();
            let chip = palette.chip();
            assert_ne!(chip.fg, chip.bg, "`{name}` chip text is invisible");
            let alt = palette.chip_alt();
            assert_ne!(alt.fg, alt.bg, "`{name}` alternate chip text is invisible");
        }
    }

    #[test]
    fn the_light_theme_is_actually_light() {
        let paper = Palette::paper();
        assert!(
            luminance(paper.text) < luminance(paper.panel_bg),
            "paper must draw dark text on a light panel"
        );
        for surface in [paper.panel_bg, paper.surface_dim, paper.surface0, paper.surface1] {
            assert!(
                luminance(surface) > luminance(paper.text) + 40.0,
                "every paper surface must stay clear of the text colour"
            );
        }
        // The dark themes hold the opposite invariant.
        for palette in [Palette::catppuccin(), Palette::tokyo_night(), Palette::gruvbox()] {
            assert!(luminance(palette.text) > luminance(palette.panel_bg));
        }
    }
}
