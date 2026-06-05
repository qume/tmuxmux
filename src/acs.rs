//! DEC Special Graphics (ACS) translation.
//!
//! vt100-ctt deliberately ignores charset escapes (`ESC ( 0`, SO/SI), so when
//! tmux or curses apps draw lines via the alternate character set, the raw
//! ASCII bytes show up instead ("qqqq" for a horizontal line). This filter
//! sits in front of the parser: it tracks G0/G1 charset designation and
//! shift-in/shift-out state, and rewrites graphics characters to their
//! Unicode equivalents. It is escape-sequence aware so that final bytes of
//! CSI/OSC/DCS sequences (which are also letters like 'q') are never touched.

#[derive(Clone, Copy, PartialEq)]
enum Charset {
    Ascii,
    DecGraphics,
}

#[derive(Clone, Copy, PartialEq)]
enum State {
    Ground,
    Esc,
    DesignateG0,
    DesignateG1,
    Csi,
    Osc,
    OscEsc,
    /// DCS / SOS / PM / APC — consumed until ST (ESC \).
    Str,
    StrEsc,
}

pub struct AcsFilter {
    state: State,
    g0: Charset,
    g1: Charset,
    /// true = G1 invoked (after SO), false = G0 (after SI / default)
    shifted: bool,
}

impl AcsFilter {
    pub fn new() -> Self {
        AcsFilter {
            state: State::Ground,
            g0: Charset::Ascii,
            g1: Charset::Ascii,
            shifted: false,
        }
    }

    fn active(&self) -> Charset {
        if self.shifted {
            self.g1
        } else {
            self.g0
        }
    }

    /// Translate `input`, appending parser-ready bytes to `out`.
    pub fn feed(&mut self, input: &[u8], out: &mut Vec<u8>) {
        for &b in input {
            match self.state {
                State::Ground => match b {
                    0x1b => {
                        // Don't emit yet — if this turns out to be a charset
                        // designation we consume the whole sequence.
                        self.state = State::Esc;
                    }
                    0x0e => {
                        // SO: invoke G1. Swallowed — the parser no-ops it anyway.
                        self.shifted = true;
                    }
                    0x0f => {
                        // SI: invoke G0.
                        self.shifted = false;
                    }
                    0x5f..=0x7e if self.active() == Charset::DecGraphics => {
                        out.extend_from_slice(dec_graphic(b));
                    }
                    _ => out.push(b),
                },
                State::Esc => match b {
                    b'(' => self.state = State::DesignateG0,
                    b')' => self.state = State::DesignateG1,
                    0x1b => {
                        // ESC ESC: emit the first, stay in Esc for the second.
                        out.push(0x1b);
                    }
                    b'[' => {
                        self.state = State::Csi;
                        out.extend_from_slice(&[0x1b, b]);
                    }
                    b']' => {
                        self.state = State::Osc;
                        out.extend_from_slice(&[0x1b, b]);
                    }
                    b'P' | b'X' | b'^' | b'_' => {
                        self.state = State::Str;
                        out.extend_from_slice(&[0x1b, b]);
                    }
                    b'c' => {
                        // RIS: full reset, charsets included.
                        self.g0 = Charset::Ascii;
                        self.g1 = Charset::Ascii;
                        self.shifted = false;
                        self.state = State::Ground;
                        out.extend_from_slice(&[0x1b, b]);
                    }
                    _ => {
                        self.state = State::Ground;
                        out.extend_from_slice(&[0x1b, b]);
                    }
                },
                State::DesignateG0 => {
                    self.g0 = if b == b'0' {
                        Charset::DecGraphics
                    } else {
                        Charset::Ascii
                    };
                    self.state = State::Ground;
                }
                State::DesignateG1 => {
                    self.g1 = if b == b'0' {
                        Charset::DecGraphics
                    } else {
                        Charset::Ascii
                    };
                    self.state = State::Ground;
                }
                State::Csi => {
                    out.push(b);
                    // Parameter/intermediate bytes 0x20..=0x3f continue; a
                    // final byte 0x40..=0x7e ends the sequence.
                    if (0x40..=0x7e).contains(&b) {
                        self.state = State::Ground;
                    }
                }
                State::Osc => {
                    out.push(b);
                    match b {
                        0x07 => self.state = State::Ground,
                        0x1b => self.state = State::OscEsc,
                        _ => {}
                    }
                }
                State::OscEsc => {
                    out.push(b);
                    self.state = if b == b'\\' { State::Ground } else { State::Osc };
                }
                State::Str => {
                    out.push(b);
                    if b == 0x1b {
                        self.state = State::StrEsc;
                    }
                }
                State::StrEsc => {
                    out.push(b);
                    self.state = if b == b'\\' { State::Ground } else { State::Str };
                }
            }
        }
    }
}

/// VT100 special graphics → Unicode (xterm's mapping).
fn dec_graphic(b: u8) -> &'static [u8] {
    match b {
        0x5f => " ".as_bytes(),        // blank
        b'`' => "\u{25c6}".as_bytes(), // ◆
        b'a' => "\u{2592}".as_bytes(), // ▒
        b'b' => "\u{2409}".as_bytes(), // ␉
        b'c' => "\u{240c}".as_bytes(), // ␌
        b'd' => "\u{240d}".as_bytes(), // ␍
        b'e' => "\u{240a}".as_bytes(), // ␊
        b'f' => "\u{00b0}".as_bytes(), // °
        b'g' => "\u{00b1}".as_bytes(), // ±
        b'h' => "\u{2424}".as_bytes(), // ␤
        b'i' => "\u{240b}".as_bytes(), // ␋
        b'j' => "\u{2518}".as_bytes(), // ┘
        b'k' => "\u{2510}".as_bytes(), // ┐
        b'l' => "\u{250c}".as_bytes(), // ┌
        b'm' => "\u{2514}".as_bytes(), // └
        b'n' => "\u{253c}".as_bytes(), // ┼
        b'o' => "\u{23ba}".as_bytes(), // ⎺
        b'p' => "\u{23bb}".as_bytes(), // ⎻
        b'q' => "\u{2500}".as_bytes(), // ─
        b'r' => "\u{23bc}".as_bytes(), // ⎼
        b's' => "\u{23bd}".as_bytes(), // ⎽
        b't' => "\u{251c}".as_bytes(), // ├
        b'u' => "\u{2524}".as_bytes(), // ┤
        b'v' => "\u{2534}".as_bytes(), // ┴
        b'w' => "\u{252c}".as_bytes(), // ┬
        b'x' => "\u{2502}".as_bytes(), // │
        b'y' => "\u{2264}".as_bytes(), // ≤
        b'z' => "\u{2265}".as_bytes(), // ≥
        b'{' => "\u{03c0}".as_bytes(), // π
        b'|' => "\u{2260}".as_bytes(), // ≠
        b'}' => "\u{00a3}".as_bytes(), // £
        b'~' => "\u{00b7}".as_bytes(), // ·
        _ => {
            // 0x5f..=0x7e is fully covered above; unreachable, but stay safe.
            b" "
        }
    }
}
