//! Colours, laid out like the web sidebar's tokens (`web.html` `:root`) but
//! taken from the terminal's own theme, the way Claude Code and every other
//! terminal program follow it.
//!
//! Text and accents are the default foreground and the ANSI palette (`Reset`,
//! `Red`, `Blue`…), so the theme decides them and they keep their place
//! relative to everything else on screen. The surfaces the palette can't
//! express — a card a shade off the background, a current row tinted blue —
//! are mixed from the colours the terminal reports (OSC 10/11 and OSC 4).

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use ratatui::style::Color;

type Rgb = (u8, u8, u8);

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
    /// Group and pane headings.
    pub muted: Color,
    /// Ages, secondary lines, placeholders.
    pub faint: Color,
    pub primary: Color,
    pub idle: Color,
    pub busy: Color,
    pub attention: Color,
}

/// What the terminal said about its theme.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Reported {
    pub bg: Rgb,
    pub fg: Rgb,
    pub blue: Rgb,
    pub yellow: Rgb,
}

impl Default for Reported {
    /// Ghostty's defaults, for a terminal that doesn't answer.
    fn default() -> Self {
        Self {
            bg: (0x28, 0x2c, 0x34),
            fg: (0xff, 0xff, 0xff),
            blue: (0x00, 0x1e, 0x80),
            yellow: (0xd3, 0x87, 0x00),
        }
    }
}

impl Theme {
    pub(crate) fn from_reported(r: Reported) -> Self {
        let dark = luma(r.bg) <= 0.5;
        // Tints read weaker on a dark background, so they mix in further.
        let tint = if dark { 0.32 } else { 0.18 };
        Self {
            card: mix(r.bg, r.fg, 0.06),
            card_hover: mix(r.bg, r.fg, 0.12),
            current: mix(r.bg, r.blue, tint),
            on_current: Color::Reset,
            need: mix(r.bg, r.yellow, tint),
            band: mix(r.bg, r.fg, 0.09),
            text: Color::Reset,
            muted: Color::Reset,
            faint: Color::DarkGray,
            primary: Color::Blue,
            idle: Color::Green,
            busy: Color::Red,
            attention: Color::Yellow,
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::from_reported(Reported::default())
    }
}

fn luma((r, g, b): Rgb) -> f32 {
    (0.2126 * f32::from(r) + 0.7152 * f32::from(g) + 0.0722 * f32::from(b)) / 255.0
}

/// `a` moved `t` of the way toward `b`.
fn mix(a: Rgb, b: Rgb, t: f32) -> Color {
    let m = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * t).round() as u8;
    Color::Rgb(m(a.0, b.0), m(a.1, b.1), m(a.2, b.2))
}

/// Ask the terminal for its colours. Must run in raw mode, between event
/// reads (never while crossterm is waiting on stdin), or the replies are
/// echoed / eaten as keys. Whatever doesn't answer within 150ms keeps its
/// default.
pub(crate) fn detect() -> Theme {
    Theme::from_reported(query())
}

/// Whether macOS is in dark mode. Ghostty's `theme = light:…,dark:…` follows
/// it, but a running sidebar only learns its background by asking, so it
/// watches this and asks again when it flips. `AppleInterfaceStyle` is absent
/// (non-zero exit) in light mode.
pub(crate) fn os_dark() -> bool {
    std::process::Command::new("defaults")
        .args(["read", "-g", "AppleInterfaceStyle"])
        .output()
        .is_ok_and(|o| o.status.success() && o.stdout.starts_with(b"Dark"))
}

/// The four queries, in one write: background, foreground, ANSI blue, yellow.
const QUERIES: &[u8] = b"\x1b]11;?\x1b\\\x1b]10;?\x1b\\\x1b]4;4;?\x1b\\\x1b]4;3;?\x1b\\";

fn query() -> Reported {
    let mut rep = Reported::default();
    let mut out = std::io::stdout();
    if out.write_all(QUERIES).and_then(|_| out.flush()).is_err() {
        return rep;
    }
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
        let mut chunk = [0u8; 256];
        match stdin.read(&mut chunk) {
            Ok(n) if n > 0 => buf.extend_from_slice(&chunk[..n]),
            _ => break,
        }
        // The yellow reply comes last; once it's in, everything is.
        if reply(&buf, "4;3").is_some() {
            break;
        }
    }
    let fill = |slot: &mut Rgb, key: &str| {
        if let Some(c) = reply(&buf, key) {
            *slot = c;
        }
    };
    fill(&mut rep.bg, "11");
    fill(&mut rep.fg, "10");
    fill(&mut rep.blue, "4;4");
    fill(&mut rep.yellow, "4;3");
    rep
}

/// The colour in `ESC ] <key> ; rgb:RRRR/GGGG/BBBB` (BEL or ST terminated),
/// 1–4 hex digits per channel.
fn reply(buf: &[u8], key: &str) -> Option<Rgb> {
    let s = String::from_utf8_lossy(buf);
    let head = format!("\x1b]{key};rgb:");
    let body = &s[s.find(&head)? + head.len()..];
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
    fn reads_each_reply_by_key_in_both_terminators_and_widths() {
        let buf = b"\x1b]11;rgb:2828/2c2c/3434\x1b\\\x1b]10;rgb:ff/ff/ff\x07\x1b]4;4;rgb:0000/0000/ffff\x1b\\";
        assert_eq!(reply(buf, "11"), Some((0x28, 0x2c, 0x34)));
        assert_eq!(reply(buf, "10"), Some((255, 255, 255)));
        assert_eq!(reply(buf, "4;4"), Some((0, 0, 255)));
        assert_eq!(reply(buf, "4;3"), None, "not answered");
        assert_eq!(reply(b"\x1b]11;rgb:28", "11"), None, "incomplete reply");
    }

    #[test]
    fn surfaces_mix_from_the_reported_colours_and_text_follows_the_palette() {
        let light = Theme::from_reported(Reported {
            bg: (255, 255, 255),
            fg: (0, 0, 0),
            blue: (0, 0, 255),
            yellow: (255, 255, 0),
        });
        // A card is the background a few percent toward the foreground.
        assert_eq!(light.card, Color::Rgb(240, 240, 240));
        assert_eq!(light.current, Color::Rgb(209, 209, 255));
        // Text is the terminal's, so a theme change moves it with everything else.
        assert_eq!(light.text, Color::Reset);
        assert_eq!(light.idle, Color::Green);
    }
}
