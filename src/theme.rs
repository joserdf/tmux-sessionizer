//! Color themes for the overview UI.
//!
//! ratatui has no theme system of its own — colours are plain `Color` values —
//! so the UI reads its palette from the [`Theme`] returned by [`current`]. The
//! active theme is held in a thread-local (rendering is single-threaded) and
//! swapped via [`set`]; the UI cycles through [`THEMES`].

use std::cell::Cell;

use ratatui::style::Color;

/// A named palette of semantic colours used across the overview.
#[derive(Clone, Copy)]
pub struct Theme {
    pub name: &'static str,
    pub accent: Color,
    pub muted: Color,
    pub border: Color,
    pub select_bg: Color,
    pub green: Color,
    pub red: Color,
    pub yellow: Color,
    pub magenta: Color,
    pub cyan: Color,
    pub white: Color,
}

const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::Rgb(r, g, b)
}

impl Theme {
    /// The original built-in palette (mostly terminal ANSI colours).
    pub const DEFAULT: Theme = Theme {
        name: "default",
        accent: Color::Cyan,
        muted: rgb(90, 90, 100),
        border: rgb(60, 60, 70),
        select_bg: rgb(38, 40, 54),
        green: Color::Green,
        red: Color::Red,
        yellow: Color::Yellow,
        magenta: Color::Magenta,
        cyan: Color::Cyan,
        white: Color::White,
    };

    /// Catppuccin Mocha — soft pastels on a warm dark base.
    pub const CATPPUCCIN: Theme = Theme {
        name: "catppuccin",
        accent: rgb(0xcb, 0xa6, 0xf7),    // mauve
        muted: rgb(0x6c, 0x70, 0x86),     // overlay0
        border: rgb(0x45, 0x47, 0x5a),    // surface1
        select_bg: rgb(0x31, 0x32, 0x44), // surface0
        green: rgb(0xa6, 0xe3, 0xa1),
        red: rgb(0xf3, 0x8b, 0xa8),
        yellow: rgb(0xf9, 0xe2, 0xaf),
        magenta: rgb(0xf5, 0xc2, 0xe7), // pink
        cyan: rgb(0x94, 0xe2, 0xd5),    // teal
        white: rgb(0xcd, 0xd6, 0xf4),   // text
    };

    /// Tokyo Night — cool blues and purples on near-black.
    pub const TOKYO_NIGHT: Theme = Theme {
        name: "tokyo-night",
        accent: rgb(0x7a, 0xa2, 0xf7), // blue
        muted: rgb(0x56, 0x5f, 0x89),  // comment
        border: rgb(0x29, 0x2e, 0x42),
        select_bg: rgb(0x2f, 0x33, 0x4d),
        green: rgb(0x9e, 0xce, 0x6a),
        red: rgb(0xf7, 0x76, 0x8e),
        yellow: rgb(0xe0, 0xaf, 0x68),
        magenta: rgb(0xbb, 0x9a, 0xf7), // purple
        cyan: rgb(0x7d, 0xcf, 0xff),
        white: rgb(0xc0, 0xca, 0xf5),
    };

    /// Dracula — vivid high-contrast purples, cyan and pink.
    pub const DRACULA: Theme = Theme {
        name: "dracula",
        accent: rgb(0xbd, 0x93, 0xf9), // purple
        muted: rgb(0x62, 0x72, 0xa4),
        border: rgb(0x44, 0x47, 0x5a),
        select_bg: rgb(0x3b, 0x3e, 0x52),
        green: rgb(0x50, 0xfa, 0x7b),
        red: rgb(0xff, 0x55, 0x55),
        yellow: rgb(0xf1, 0xfa, 0x8c),
        magenta: rgb(0xff, 0x79, 0xc6), // pink
        cyan: rgb(0x8b, 0xe9, 0xfd),
        white: rgb(0xf8, 0xf8, 0xf2),
    };
}

/// All selectable themes, in cycle order.
pub const THEMES: &[Theme] = &[
    Theme::DEFAULT,
    Theme::CATPPUCCIN,
    Theme::TOKYO_NIGHT,
    Theme::DRACULA,
];

/// Index of the theme with the given name, or 0 (default) when unknown.
pub fn by_name(name: &str) -> usize {
    THEMES.iter().position(|t| t.name == name).unwrap_or(0)
}

thread_local! {
    static CURRENT: Cell<Theme> = const { Cell::new(Theme::DEFAULT) };
}

/// Set the active theme for subsequent draws.
pub fn set(theme: Theme) {
    CURRENT.with(|c| c.set(theme));
}

/// The active theme.
pub fn current() -> Theme {
    CURRENT.with(|c| c.get())
}
