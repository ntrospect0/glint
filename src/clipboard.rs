// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Minimal OSC 52 clipboard helper.
//!
//! OSC 52 is a terminal escape sequence that asks the terminal emulator
//! to set the system clipboard. It works in modern terminals (Kitty,
//! WezTerm, iTerm2, Alacritty with `selection_clipboard: true`, recent
//! tmux with `set -g allow-passthrough on`) and silently does nothing
//! everywhere else — so we treat it as a best-effort hint, not a
//! guaranteed copy.
//!
//! Format: `ESC ] 52 ; c ; <base64-encoded-text> ESC \\`
//!   - `c` selects the clipboard (vs `p` for X primary selection).
//!   - The text payload must be base64; we hand-roll the encoder so we
//!     don't pull in a crate for ~30 bytes of typical clipboard content.

use std::io::{self, Write};

/// Try to copy `text` to the system clipboard via OSC 52. Returns
/// `Ok(())` if the escape sequence was written successfully — that's
/// not the same as "the terminal honored it." Falls back silently
/// on terminals without OSC 52 support; the caller decides whether
/// to surface a feedback line.
pub fn copy(text: &str) -> io::Result<()> {
    let encoded = base64_encode(text.as_bytes());
    let payload = format!("\x1b]52;c;{encoded}\x1b\\");
    let mut out = io::stdout().lock();
    out.write_all(payload.as_bytes())?;
    out.flush()
}

/// RFC 4648 base64. ~30 lines of code; pulling in a crate for a single
/// call site felt heavier than this is worth.
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHA: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((bytes.len() + 2) / 3 * 4);
    let chunks = bytes.chunks(3);
    for chunk in chunks {
        let (b0, b1, b2, pad) = match chunk.len() {
            3 => (chunk[0], chunk[1], chunk[2], 0),
            2 => (chunk[0], chunk[1], 0, 1),
            1 => (chunk[0], 0, 0, 2),
            _ => unreachable!(),
        };
        let n = ((b0 as u32) << 16) | ((b1 as u32) << 8) | (b2 as u32);
        out.push(ALPHA[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHA[((n >> 12) & 0x3f) as usize] as char);
        if pad < 2 {
            out.push(ALPHA[((n >> 6) & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
        if pad < 1 {
            out.push(ALPHA[(n & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_known_vectors() {
        // RFC 4648 examples — these are the canonical reference outputs.
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_handles_full_byte_range() {
        // 0x00..0xff exercises every 6-bit slot; we don't compare to a
        // gold string (too long), just confirm output is well-formed
        // base64: only alphabet chars + `=`, length a multiple of 4.
        let input: Vec<u8> = (0..=255u8).collect();
        let out = base64_encode(&input);
        assert_eq!(out.len() % 4, 0);
        assert!(out
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '='));
    }
}
