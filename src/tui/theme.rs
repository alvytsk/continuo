//! A gruvbox-dark palette: warm greys for borders, cream text, green for
//! what plays and yellow for the selection. The player paints no background of its
//! own, so the terminal's background shows through.

use ratatui::style::Color;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Theme {
    /// Dark text drawn on green highlights.
    pub ink: Color,
    pub panel: Color,
    pub line: Color,
    pub muted: Color,
    pub text: Color,
    pub cream: Color,
    pub green: Color,
    pub amber: Color,
    pub cyan: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            ink: Color::Rgb(0x28, 0x28, 0x28),
            panel: Color::Rgb(0x3c, 0x38, 0x36),
            line: Color::Rgb(0x50, 0x49, 0x45),
            muted: Color::Rgb(0x92, 0x83, 0x74),
            text: Color::Rgb(0xeb, 0xdb, 0xb2),
            cream: Color::Rgb(0xfb, 0xf1, 0xc7),
            green: Color::Rgb(0xb8, 0xbb, 0x26),
            amber: Color::Rgb(0xfa, 0xbd, 0x2f),
            cyan: Color::Rgb(0x8e, 0xc0, 0x7c),
        }
    }
}
