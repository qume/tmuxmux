use egui::Color32;

/// Default foreground: slightly-off white, like most terminal themes.
pub const DEFAULT_FG: Color32 = Color32::from_rgb(0xd8, 0xd8, 0xd8);
/// Default background: pure black.
pub const DEFAULT_BG: Color32 = Color32::BLACK;
/// Selection highlight background.
pub const SELECTION_BG: Color32 = Color32::from_rgb(0x33, 0x55, 0x88);

/// THE fix for the white-background bug in the previous attempt: the default
/// color means *different things* for foreground and background. The old code
/// mapped `Color::Default` to white for both, so every uncolored cell got a
/// white background rectangle painted over the black base.
pub fn convert_fg(c: vt100_ctt::Color, bold: bool) -> Color32 {
    match c {
        vt100_ctt::Color::Default => {
            if bold {
                Color32::WHITE
            } else {
                DEFAULT_FG
            }
        }
        // Bold + basic ANSI color conventionally brightens to the high-intensity variant.
        vt100_ctt::Color::Idx(i) if bold && i < 8 => xterm_256(i + 8),
        vt100_ctt::Color::Idx(i) => xterm_256(i),
        vt100_ctt::Color::Rgb(r, g, b) => Color32::from_rgb(r, g, b),
    }
}

pub fn convert_bg(c: vt100_ctt::Color) -> Color32 {
    match c {
        vt100_ctt::Color::Default => DEFAULT_BG,
        vt100_ctt::Color::Idx(i) => xterm_256(i),
        vt100_ctt::Color::Rgb(r, g, b) => Color32::from_rgb(r, g, b),
    }
}

pub fn xterm_256(idx: u8) -> Color32 {
    match idx {
        // Standard + bright ANSI colors (xterm defaults).
        0 => Color32::from_rgb(0, 0, 0),
        1 => Color32::from_rgb(205, 0, 0),
        2 => Color32::from_rgb(0, 205, 0),
        3 => Color32::from_rgb(205, 205, 0),
        4 => Color32::from_rgb(60, 110, 230), // nudged lighter than xterm's for legibility on black
        5 => Color32::from_rgb(205, 0, 205),
        6 => Color32::from_rgb(0, 205, 205),
        7 => Color32::from_rgb(229, 229, 229),
        8 => Color32::from_rgb(127, 127, 127),
        9 => Color32::from_rgb(255, 0, 0),
        10 => Color32::from_rgb(0, 255, 0),
        11 => Color32::from_rgb(255, 255, 0),
        12 => Color32::from_rgb(92, 92, 255),
        13 => Color32::from_rgb(255, 0, 255),
        14 => Color32::from_rgb(0, 255, 255),
        15 => Color32::from_rgb(255, 255, 255),
        16..=231 => {
            let i = idx as u32 - 16;
            let r = i / 36;
            let g = (i % 36) / 6;
            let b = i % 6;
            let level = |v: u32| -> u8 {
                if v == 0 {
                    0
                } else {
                    (55 + v * 40) as u8
                }
            };
            Color32::from_rgb(level(r), level(g), level(b))
        }
        232..=255 => {
            let val = (8 + (idx as u32 - 232) * 10) as u8;
            Color32::from_rgb(val, val, val)
        }
    }
}
