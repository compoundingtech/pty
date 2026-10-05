//! Encoding semantic input for the child: keys, mouse, focus, paste.
//!
//! This is the other half of the terminal boundary. A consumer that owns a
//! surface (a TUI pane, a GUI widget) knows *what the user did*; only the
//! terminal knows what bytes the child expects for it, because that depends on
//! state the child itself set: DECCKM, the keypad mode, `modifyOtherKeys`, the
//! kitty keyboard flags, the mouse tracking mode and report format, focus
//! reporting, bracketed paste. Splitting that knowledge — semantic events on
//! one side, a second encoder on the other — means two implementations of the
//! kitty keyboard protocol and a class of parity bug nobody can test.
//!
//! So the events here are deliberately dumb and owned ([`KeyEvent`],
//! [`MouseEvent`]), and the encoding happens inside the terminal:
//! [`crate::actor::TerminalActor::encode_key`] and friends, or the `pty`
//! crate's `TerminalHandle::send_key` to encode and write in one ordered
//! step.
//!
//! The encoders themselves are libghostty's ([`libghostty_vt::key`],
//! [`libghostty_vt::mouse`], [`libghostty_vt::focus`],
//! [`libghostty_vt::paste`]), configured from the live terminal. The key and
//! modifier vocabulary is re-exported rather than re-declared: a second
//! `Key` enum would be a translation table that silently drifts.
//!
//! What this module does *not* do: decide anything. It never swallows a key
//! for its own use and it has no notion of a shortcut, a leader, or a detach
//! sequence. A consumer that reserves keys evaluates them before it calls
//! here.

use libghostty_vt::terminal::Terminal;
use libghostty_vt::{focus, key, mouse, paste};

pub use libghostty_vt::key::{Action as KeyAction, Key, KittyKeyFlags, Mods};
pub use libghostty_vt::mouse::{Action as MouseAction, Button as MouseButton};

use crate::actor::Modes;
use crate::graphics::CellSize;

/// The cell metrics used for mouse coordinates when the terminal has none
/// (kitty graphics are what otherwise gives a terminal a cell size). Only the
/// ratio of position to cell matters for a cell-addressed report, so any
/// consistent pair works; SGR-pixels reports scale with it.
const DEFAULT_CELL: CellSize = CellSize {
    width: 8,
    height: 16,
};

/// One key event from the surface.
///
/// The three text-ish fields are separate on purpose, because the kitty
/// keyboard protocol reports them separately:
///
/// - `key` is the logical key (`Key::KeyA`, `Key::ArrowUp`, `Key::Enter`).
/// - `text` is what the key produced with the user's layout and shift state —
///   kitty's "associated text", reported when the child asked for
///   [`KittyKeyFlags::REPORT_ASSOCIATED`].
/// - `unshifted` is the same key without shift, which is what the shifted-key
///   alternate is derived against for
///   [`KittyKeyFlags::REPORT_ALTERNATES`].
///
/// Folding shift into one character upstream loses the alternate, and with it
/// any way for the child to tell `shift+a` from `A` typed on a layout where
/// they differ.
///
/// A character key needs at least one of `text` and `unshifted` to have an
/// identity the protocol can report: an event that carries neither encodes to
/// nothing under the kitty protocol, because there is no codepoint to name.
/// Named keys (`Key::Enter`, `Key::ArrowUp`) need neither.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyEvent {
    /// The logical key.
    pub key: Key,
    /// Modifiers held.
    pub mods: Mods,
    /// Press, repeat, or release. Release only reaches the child when it
    /// asked for [`KittyKeyFlags::REPORT_EVENTS`].
    pub action: KeyAction,
    /// The text the key produced (shift applied), or `None` for a key that
    /// produces none. Must not be a C0 control or a platform function-key
    /// code: pass `None` and let the logical key speak.
    pub text: Option<String>,
    /// The same key with no shift applied.
    pub unshifted: Option<char>,
    /// Modifiers the surface already consumed (a compose or IME step), which
    /// the child should not see again.
    pub consumed_mods: Mods,
    /// Whether this event is part of an in-progress composition.
    pub composing: bool,
}

impl KeyEvent {
    /// A plain press of `key` with no modifiers and no text.
    pub fn press(key: Key) -> KeyEvent {
        KeyEvent {
            key,
            mods: Mods::empty(),
            action: KeyAction::Press,
            text: None,
            unshifted: None,
            consumed_mods: Mods::empty(),
            composing: false,
        }
    }

    /// A press of a character key: `text` is what it typed, `unshifted` the
    /// same key without shift.
    pub fn typed(key: Key, text: &str, unshifted: Option<char>) -> KeyEvent {
        KeyEvent {
            text: Some(text.to_string()),
            unshifted,
            ..KeyEvent::press(key)
        }
    }

    /// The same event with `mods` held.
    pub fn with_mods(mut self, mods: Mods) -> KeyEvent {
        self.mods = mods;
        self
    }
}

/// One mouse event from the surface, addressed in terminal cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseEvent {
    /// Press, release, or motion. A wheel notch is a `Press` of
    /// [`MouseButton::Four`]/[`MouseButton::Five`] (vertical) or
    /// [`MouseButton::Six`]/[`MouseButton::Seven`] (horizontal), as in the
    /// protocol.
    pub action: MouseAction,
    /// The button, or `None` for motion with no button.
    pub button: Option<MouseButton>,
    /// Modifiers held.
    pub mods: Mods,
    /// Column in the grid, 0-based.
    pub col: u16,
    /// Row in the grid, 0-based.
    pub row: u16,
    /// Whether any button is held, which is what distinguishes a drag from a
    /// bare move for `?1002`.
    pub any_button_pressed: bool,
}

impl MouseEvent {
    /// A press of `button` at a cell.
    pub fn press(button: MouseButton, col: u16, row: u16) -> MouseEvent {
        MouseEvent {
            action: MouseAction::Press,
            button: Some(button),
            mods: Mods::empty(),
            col,
            row,
            any_button_pressed: true,
        }
    }

    /// One wheel notch at a cell: `up` chooses button 4 or 5.
    pub fn wheel(up: bool, col: u16, row: u16) -> MouseEvent {
        MouseEvent {
            any_button_pressed: false,
            ..MouseEvent::press(
                if up {
                    MouseButton::Four
                } else {
                    MouseButton::Five
                },
                col,
                row,
            )
        }
    }

    /// Whether this is a wheel notch rather than a real button.
    pub fn is_wheel(&self) -> bool {
        matches!(
            self.button,
            Some(MouseButton::Four | MouseButton::Five | MouseButton::Six | MouseButton::Seven)
        )
    }
}

/// Encode a key event for the child, using `term`'s own keyboard state
/// (DECCKM, keypad, `modifyOtherKeys`, and the kitty keyboard flags).
///
/// Returns no bytes for an event the child should not see at all — a bare
/// modifier press, or a release while the child has not asked for release
/// events.
pub fn key(term: &Terminal, ev: &KeyEvent) -> Vec<u8> {
    let Ok(mut encoder) = key::Encoder::new() else {
        return Vec::new();
    };
    encoder.set_options_from_terminal(term);
    let Ok(mut event) = key::Event::new() else {
        return Vec::new();
    };
    event
        .set_key(ev.key)
        .set_mods(ev.mods)
        .set_action(ev.action)
        .set_consumed_mods(ev.consumed_mods)
        .set_composing(ev.composing);
    // The encoder wants the unmodified character; a control or a platform
    // function-key code has to be withheld so it uses the logical key.
    let text = ev
        .text
        .as_deref()
        .filter(|t| !t.chars().any(is_unencodable_text));
    event.set_utf8(text);
    if let Some(c) = ev.unshifted {
        event.set_unshifted_codepoint(c);
    }
    let mut out = Vec::new();
    let _ = encoder.encode_to_vec(&event, &mut out);
    out
}

/// A character the key encoder must not be given as associated text.
fn is_unencodable_text(c: char) -> bool {
    let c = c as u32;
    c < 0x20 || c == 0x7f || (0xf700..=0xf8ff).contains(&c)
}

/// Encode a mouse event for the child, using `term`'s tracking mode and
/// report format.
///
/// `None` means this event is not reportable in the mode the child chose, and
/// the surface keeps it: no tracking at all, a wheel notch under X10 (`?9`,
/// which reports button presses only), or an event the format drops. That
/// distinction is the whole point of the return type — a consumer that wants
/// to scroll its own viewport with the wheel needs to know the child would
/// not have heard it.
pub fn mouse(term: &Terminal, modes: &Modes, ev: &MouseEvent, cell: CellSize) -> Option<Vec<u8>> {
    if !modes.mouse_reporting() {
        return None;
    }
    // X10 reports presses of real buttons and nothing else. libghostty's
    // encoder is told the mode, but the rule is stated here so the answer
    // does not depend on how the encoder happens to treat a wheel button.
    if !modes.mouse_tracking() && (ev.is_wheel() || ev.action != MouseAction::Press) {
        return None;
    }
    let cell = if cell.width == 0 || cell.height == 0 {
        DEFAULT_CELL
    } else {
        cell
    };
    let cols = term.cols().unwrap_or(0).max(1) as u32;
    let rows = term.rows().unwrap_or(0).max(1) as u32;

    let mut encoder = mouse::Encoder::new().ok()?;
    encoder.set_options_from_terminal(term);
    // This Ghostty revision marks every 1003 report as motion, including a wheel
    // or button press. Normal tracking has the same press/release eligibility;
    // use it for those events, while retaining 1003 for actual motion.
    if ev.action != MouseAction::Motion && modes.mouse_tracking() {
        encoder.set_tracking_mode(mouse::TrackingMode::Normal);
    }

    encoder
        .set_size(mouse::EncoderSize {
            screen_width: cols * cell.width,
            screen_height: rows * cell.height,
            cell_width: cell.width,
            cell_height: cell.height,
            padding_top: 0,
            padding_bottom: 0,
            padding_right: 0,
            padding_left: 0,
        })
        .set_any_button_pressed(ev.any_button_pressed);

    let mut event = mouse::Event::new().ok()?;
    event
        .set_action(ev.action)
        .set_button(ev.button)
        .set_mods(ev.mods)
        // The middle of the cell: a cell-addressed report rounds to the same
        // cell from anywhere inside it, and an SGR-pixels report gets a
        // position that is actually in the cell it names.
        .set_position(mouse::Position {
            x: (ev.col as f32 + 0.5) * cell.width as f32,
            y: (ev.row as f32 + 0.5) * cell.height as f32,
        });

    let mut out = Vec::new();
    encoder.encode_to_vec(&event, &mut out).ok()?;
    (!out.is_empty()).then_some(out)
}

/// Encode a focus change, or `None` when the child did not ask for focus
/// events (`?1004`).
pub fn focus(modes: &Modes, gained: bool) -> Option<Vec<u8>> {
    if !modes.focus_events {
        return None;
    }
    let event = if gained {
        focus::Event::Gained
    } else {
        focus::Event::Lost
    };
    let mut buf = [0u8; 8];
    let n = event.encode(&mut buf).ok()?;
    Some(buf[..n].to_vec())
}

/// Encode pasted text: bracketed when the child asked for `?2004`, with
/// control bytes stripped and newlines turned into carriage returns when it
/// did not.
pub fn paste(modes: &Modes, text: &str) -> Vec<u8> {
    let mut data = text.as_bytes().to_vec();
    // Bracketing adds `ESC[200~` and `ESC[201~`; the encoder never grows the
    // payload itself.
    let mut buf = vec![0u8; data.len() + 16];
    match paste::encode(&mut data, modes.bracketed_paste, &mut buf) {
        Ok(n) => {
            buf.truncate(n);
            buf
        }
        Err(_) => Vec::new(),
    }
}

/// Whether pasting `text` is safe without asking the user: a paste carrying a
/// newline (or a forged bracketed-paste end) can run a command the user never
/// typed. Not enforced here — a consumer decides whether to confirm — but the
/// judgement belongs with the terminal, not with each surface.
pub fn paste_is_safe(text: &str) -> bool {
    paste::is_safe(text)
}
