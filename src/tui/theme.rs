//! The compact reference palette (design doc M5 §7): a dark ground, subtle
//! borders, cream text and muted green highlights.

use ratatui::style::Color;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Theme {
    pub background: Color,
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
            background: Color::Rgb(0x10, 0x18, 0x1a),
            panel: Color::Rgb(0x19, 0x1e, 0x21),
            line: Color::Rgb(0x34, 0x41, 0x45),
            muted: Color::Rgb(0x93, 0xa1, 0x9f),
            text: Color::Rgb(0xdc, 0xe5, 0xdf),
            cream: Color::Rgb(0xe7, 0xed, 0xdf),
            green: Color::Rgb(0xb3, 0xd8, 0x9c),
            amber: Color::Rgb(0xe7, 0xb4, 0x78),
            cyan: Color::Rgb(0x8b, 0xbe, 0xb6),
        }
    }
}
