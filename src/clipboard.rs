// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Two-tier clipboard helper.
//!
//! 1. Try `arboard` — talks to the OS clipboard directly (NSPasteboard on
//!    macOS, X11/Wayland on Linux, Win32 on Windows). Reliable, works
//!    regardless of terminal config, and makes the copied text
//!    immediately pastable with the normal OS paste shortcut.
//! 2. Fall back to OSC 52 — a terminal escape sequence that asks the
//!    emulator to set the system clipboard. Needed for SSH / tmux /
//!    headless contexts where arboard can't see a display, but
//!    silently no-ops on terminals that don't proxy it (Terminal.app,
//!    Alacritty without `selection.save_to_clipboard = true`, iTerm2
//!    without "Allow apps to access clipboard").
//!
//! Format of the OSC 52 escape: `ESC ] 52 ; c ; <base64-text> ESC \\`
//!   - `c` selects the clipboard (vs `p` for X primary selection).
//!   - We hand-roll base64 so we don't pull in a crate for ~30 bytes
//!     of typical payload.

use std::io::{self, Write};

/// Copy `text` to the system clipboard. Returns `Ok(())` if either the
/// direct OS clipboard write or the OSC 52 fallback succeeded.
///
/// Direct write is preferred — OSC 52 success here only means the
/// escape sequence got onto stdout, not that the terminal forwarded it
/// to the OS clipboard. With arboard out front the OSC 52 path mostly
/// matters for SSH / tmux pass-through.
pub fn copy(text: &str) -> io::Result<()> {
    if write_os_clipboard(text) {
        return Ok(());
    }
    write_osc52(text)
}

/// Direct OS-clipboard write. Returns `true` on success. arboard can
/// fail in headless / SSH / GUI-less containers — that's why we fall
/// back to OSC 52 in [`copy`] when this returns `false`.
fn write_os_clipboard(text: &str) -> bool {
    match arboard::Clipboard::new() {
        Ok(mut cb) => cb.set_text(text.to_string()).is_ok(),
        Err(_) => false,
    }
}

/// Read the system clipboard. Returns `None` when the clipboard is
/// empty, contains a non-text payload, or arboard can't reach a
/// display (headless / SSH). No OSC 52 fallback — OSC 52 *reads* are
/// almost universally disabled (security risk), so attempting one
/// would just hang waiting for a response that'll never come.
/// Callers that need clipboard reads in SSH should accept the
/// limitation and fall back to bracketed-paste.
pub fn paste() -> Option<String> {
    let mut cb = arboard::Clipboard::new().ok()?;
    cb.get_text().ok()
}

fn write_osc52(text: &str) -> io::Result<()> {
    let encoded = base64_encode(text.as_bytes());
    let payload = format!("\x1b]52;c;{encoded}\x1b\\");
    let mut out = io::stdout().lock();
    out.write_all(payload.as_bytes())?;
    out.flush()
}

/// RFC 4648 base64. ~30 lines of code; pulling in a crate for a single
/// call site felt heavier than this is worth.
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHA: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
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
