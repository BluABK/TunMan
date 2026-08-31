//! Log-formatting helpers: a stable per-tunnel colour, and an
//! ANSI-stripping writer.
//!
//! The stripping is not theoretical. Child output goes straight into the
//! log and `ssh` will happily emit escape sequences of its own; without
//! [`StripAnsi`] those reach the file as literal escape bytes and make it
//! unreadable in an editor. The in-app Log tab stores stripped text too and
//! re-colours it from [`tag_rgb`] rather than trusting whatever arrived.

use std::io;

/// A stable colour for `name`, so each tunnel keeps the same chip colour in the
/// Log tab across restarts without anyone having to assign one.
///
/// Hashed into one of 12 hues at fixed saturation and value, which keeps every
/// result legible on both a light and a dark background — picking freely in RGB
/// would eventually land on one that vanishes into the panel.
pub fn tag_rgb(name: &str) -> (u8, u8, u8) {
    let mut h: u32 = 2166136261;
    for b in name.as_bytes() {
        h ^= *b as u32;
        h = h.wrapping_mul(16777619);
    }
    hsv_to_rgb((h % 12) as f32 * 30.0, 0.55, 0.95)
}

fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (u8, u8, u8) {
    let c = v * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = v - c;
    let (r, g, b) = match (h as u32) / 60 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    (
        ((r + m) * 255.0).round() as u8,
        ((g + m) * 255.0).round() as u8,
        ((b + m) * 255.0).round() as u8,
    )
}

/// `MakeWriter` wrapper that strips ANSI escapes, for sinks that must stay
/// plain text (the rolling file log).
pub struct StripAnsiMake<M>(pub M);

impl<'a, M: tracing_subscriber::fmt::MakeWriter<'a>> tracing_subscriber::fmt::MakeWriter<'a>
    for StripAnsiMake<M>
{
    type Writer = StripAnsi<M::Writer>;

    fn make_writer(&'a self) -> Self::Writer {
        StripAnsi { inner: self.0.make_writer(), state: AnsiState::Text }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum AnsiState {
    Text,
    /// Saw `ESC`; the next byte decides the sequence type.
    Esc,
    /// Inside `ESC [ ...` — skip until a final byte (0x40..=0x7E).
    Csi,
}

/// Writer that filters ANSI escapes out of the byte stream. **Stateful across
/// `write` calls**, so a sequence split over two writes is still removed.
pub struct StripAnsi<W> {
    inner: W,
    state: AnsiState,
}

impl<W: io::Write> io::Write for StripAnsi<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut clean = Vec::with_capacity(buf.len());
        for &b in buf {
            match self.state {
                AnsiState::Text => {
                    if b == 0x1b {
                        self.state = AnsiState::Esc;
                    } else {
                        clean.push(b);
                    }
                }
                AnsiState::Esc => {
                    self.state = if b == b'[' { AnsiState::Csi } else { AnsiState::Text };
                }
                AnsiState::Csi => {
                    if (0x40..=0x7e).contains(&b) {
                        self.state = AnsiState::Text;
                    }
                }
            }
        }
        self.inner.write_all(&clean)?;
        // The escapes were "written" (dropped), so report the whole input as
        // consumed — a short count would make the caller retry them.
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Remove ANSI CSI escapes from a string — the string-level sibling of
/// [`StripAnsi`], used before a line enters the in-app ring buffer.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for c2 in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&c2) {
                        break;
                    }
                }
            }
            continue; // a bare ESC (or ESC + non-'[') is dropped either way
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn strip_ansi_removes_colour_but_keeps_the_text() {
        assert_eq!(strip_ansi("\x1b[38;2;1;2;3m[vps]\x1b[0m up"), "[vps] up");
        assert_eq!(strip_ansi("plain"), "plain");
    }

    /// The writer is stateful on purpose: a rolling-file write can land in the
    /// middle of an escape sequence, and a stateless strip would emit its tail
    /// as literal garbage.
    #[test]
    fn the_writer_strips_a_sequence_split_across_two_writes() {
        let mut w = StripAnsi { inner: Vec::new(), state: AnsiState::Text };
        w.write_all(b"a\x1b[38;2;1").unwrap();
        w.write_all(b";2;3mb").unwrap();
        assert_eq!(String::from_utf8(w.inner).unwrap(), "ab");
    }

    #[test]
    fn a_tunnel_keeps_the_same_colour_across_runs() {
        assert_eq!(tag_rgb("vps-fi"), tag_rgb("vps-fi"));
        assert_ne!(tag_rgb("vps-fi"), tag_rgb("vps-de"));
    }
}
