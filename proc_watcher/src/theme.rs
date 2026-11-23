//! Catppuccin Frappé (https://catppuccin.com/palette) with a lavender accent.
//!
//! The background is never painted: the terminal's own background shows
//! through, so procwatch matches whatever theme the terminal uses.

use ratatui::style::Color;

pub const ROSEWATER: Color = Color::Rgb(0xf2, 0xd5, 0xcf);
pub const RED: Color = Color::Rgb(0xe7, 0x82, 0x84);
pub const MAROON: Color = Color::Rgb(0xea, 0x99, 0x9c);
pub const PEACH: Color = Color::Rgb(0xef, 0x9f, 0x76);
pub const YELLOW: Color = Color::Rgb(0xe5, 0xc8, 0x90);
pub const GREEN: Color = Color::Rgb(0xa6, 0xd1, 0x89);
pub const TEAL: Color = Color::Rgb(0x81, 0xc8, 0xbe);
pub const SAPPHIRE: Color = Color::Rgb(0x85, 0xc1, 0xdc);
pub const BLUE: Color = Color::Rgb(0x8c, 0xaa, 0xee);
pub const PINK: Color = Color::Rgb(0xf4, 0xb8, 0xe4);
pub const LAVENDER: Color = Color::Rgb(0xba, 0xbb, 0xf1);
pub const TEXT: Color = Color::Rgb(0xc6, 0xd0, 0xf5);
pub const SUBTEXT0: Color = Color::Rgb(0xa5, 0xad, 0xce);
pub const OVERLAY1: Color = Color::Rgb(0x83, 0x8b, 0xa7);
pub const OVERLAY0: Color = Color::Rgb(0x73, 0x79, 0x94);
pub const SURFACE2: Color = Color::Rgb(0x62, 0x68, 0x80);
pub const SURFACE1: Color = Color::Rgb(0x51, 0x57, 0x6d);
pub const SURFACE0: Color = Color::Rgb(0x41, 0x45, 0x59);

pub const ACCENT: Color = LAVENDER;
pub const CPU: Color = SAPPHIRE;
pub const MEMORY: Color = LAVENDER;

/// Green under 50%, yellow under 80%, red above: for load ratios in `0.0..=1.0`.
pub fn load(ratio: f64) -> Color {
    match ratio {
        r if r < 0.5 => GREEN,
        r if r < 0.8 => YELLOW,
        _ => RED,
    }
}
