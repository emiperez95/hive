//! Colours, modelled on the web sidebar's tokens (`web.html` `:root`).
//!
//! The web page owns its background; a terminal split doesn't, and a palette
//! picked for a dark terminal turns to mud on a light one. So the terminal is
//! asked for its background (OSC 11) once at startup, and every surface is
//! derived from it: a card is the background nudged a few steps away, not a
//! fixed grey that only suits one theme.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use ratatui::style::Color;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Theme {
    /// A conversation row — the web's white card on the grey panel.
    pub card: Color,
    pub card_hover: Color,
    /// Where the attached client is: tinted card, accent bar, bold title.
    pub current: Color,
    pub on_current: Color,
    /// Blocked on a human — the one tint meant to catch the eye.
    pub need: Color,
    /// Pane headers.
    pub band: Color,
    pub text: Color,
    /// Group headers, ages, secondary lines.
    pub muted: Color,
    pub faint: Color,
    pub primary: Color,
    pub idle: Color,
    pub busy: Color,
    pub attention: Color,
}

impl Theme {
    pub(crate) fn from_bg(bg: (u8, u8, u8)) -> Self {
        let light = luma(bg) > 0.5;
        // How far a surface steps away from the background, toward the text.
        let step = |n: f32| shift(bg, if light { -n } else { n });
        if light {
            Self {
                card: step(10.0),
                card_hover: step(20.0),
                current: Color::Rgb(0xd8, 0xe2, 0xff),
                on_current: Color::Rgb(0x00, 0x1a, 0x42),
                need: Color::Rgb(0xff, 0xdc, 0xc6),
                band: step(14.0),
                text: Color::Rgb(0x19, 0x1c, 0x1e),
                muted: Color::Rgb(0x42, 0x47, 0x54),
                faint: Color::Rgb(0x72, 0x77, 0x85),
                primary: Color::Rgb(0x00, 0x58, 0xbe),
                idle: Color::Rgb(0x00, 0x6c, 0x49),
                busy: Color::Rgb(0xba, 0x1a, 0x1a),
                attention: Color::Rgb(0xb7, 0x5b, 0x00),
            }
        } else {
            Self {
                card: step(12.0),
                card_hover: step(22.0),
                current: Color::Rgb(0x1e, 0x33, 0x5c),
                on_current: Color::Rgb(0xd8, 0xe2, 0xff),
                need: Color::Rgb(0x4a, 0x2c, 0x12),
                band: step(16.0),
                text: Color::Rgb(0xe2, 0xe4, 0xe8),
                muted: Color::Rgb(0xa8, 0xae, 0xbc),
                faint: Color::Rgb(0x7a, 0x80, 0x8e),
                primary: Color::Rgb(0x8a, 0xb4, 0xff),
                idle: Color::Rgb(0x4c, 0xd6, 0x9a),
                busy: Color::Rgb(0xff, 0x7a, 0x70),
                attention: Color::Rgb(0xff, 0xa8, 0x5c),
            }
        }
    }
}

impl Default for Theme {
    /// Ghostty's default background.
    fn default() -> Self {
        Self::from_bg((0x28, 0x2c, 0x34))
    }
}

fn luma((r, g, b): (u8, u8, u8)) -> f32 {
    (0.2126 * f32::from(r) + 0.7152 * f32::from(g) + 0.0722 * f32::from(b)) / 255.0
}

fn shift((r, g, b): (u8, u8, u8), by: f32) -> Color {
    let s = |c: u8| (f32::from(c) + by).clamp(0.0, 255.0) as u8;
    Color::Rgb(s(r), s(g), s(b))
}

/// Ask the terminal for its background. Must run in raw mode and before the
/// event loop starts reading stdin, or the reply is echoed / eaten as keys.
/// A terminal that doesn't answer within 150ms gets the default.
pub(crate) fn detect() -> Theme {
    query_bg().map(Theme::from_bg).unwrap_or_default()
}

fn query_bg() -> Option<(u8, u8, u8)> {
    let mut out = std::io::stdout();
    out.write_all(b"\x1b]11;?\x1b\\").ok()?;
    out.flush().ok()?;
    let deadline = Instant::now() + Duration::from_millis(150);
    let mut buf = Vec::new();
    let mut stdin = std::io::stdin();
    while Instant::now() < deadline {
        let left = deadline.saturating_duration_since(Instant::now());
        let mut pfd = libc::pollfd {
            fd: 0,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd for the duration of the call.
        if unsafe { libc::poll(&mut pfd, 1, left.as_millis() as i32) } <= 0 {
            break;
        }
        let mut chunk = [0u8; 64];
        let n = stdin.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(rgb) = parse_osc11(&buf) {
            return Some(rgb);
        }
    }
    None
}

/// `ESC ] 11 ; rgb:RRRR/GGGG/BBBB` (BEL or ST), 1–4 hex digits per channel.
fn parse_osc11(buf: &[u8]) -> Option<(u8, u8, u8)> {
    let s = std::str::from_utf8(buf).ok()?;
    let body = &s[s.find("rgb:")? + 4..];
    let end = body.find(['\x07', '\x1b'])?;
    let mut ch = body[..end].split('/').map(|h| {
        let v = u32::from_str_radix(h, 16).ok()?;
        let max = (1u32 << (4 * h.len() as u32)) - 1;
        Some((v * 255 / max) as u8)
    });
    Some((ch.next()??, ch.next()??, ch.next()??))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_osc11_in_both_terminators_and_widths() {
        assert_eq!(
            parse_osc11(b"\x1b]11;rgb:2828/2c2c/3434\x1b\\"),
            Some((0x28, 0x2c, 0x34))
        );
        assert_eq!(
            parse_osc11(b"\x1b]11;rgb:ff/ff/ff\x07"),
            Some((255, 255, 255))
        );
        assert_eq!(parse_osc11(b"\x1b]11;rgb:28"), None, "incomplete reply");
    }

    #[test]
    fn surfaces_step_toward_the_text() {
        let dark = Theme::from_bg((0x28, 0x2c, 0x34));
        let light = Theme::from_bg((0xff, 0xff, 0xff));
        assert_eq!(dark.card, Color::Rgb(0x34, 0x38, 0x40));
        assert_eq!(light.card, Color::Rgb(0xf5, 0xf5, 0xf5));
        assert_ne!(dark.text, light.text);
    }
}
