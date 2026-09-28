//! The answers a child gets when it asks the terminal a question — Node's
//! exact bytes (`src/server.ts:397-517`).
//!
//! libghostty answers most of these itself once the right callbacks and
//! defaults are installed, and its bytes match Node's exactly:
//!
//! | query | answer | who |
//! |---|---|---|
//! | DA1 `ESC[c` / `ESC[0c` | `ESC[?62;22c` | libghostty (`on_device_attributes`) |
//! | DA2 `ESC[>c` | `ESC[>0;382;0c` | libghostty (`on_device_attributes`) |
//! | DSR `ESC[6n` | `ESC[<y+1>;<x+1>R` | libghostty (native) |
//! | XTVERSION `ESC[>0q` | `DCS >\|pty(0.8) ST` | libghostty (`on_xtversion`) |
//! | OSC `10;?` | `ESC]10;rgb:c0c0/c0c0/c0c0 ESC\` | this module |
//! | OSC `11;?` | `ESC]11;rgb:0000/0000/0000 ESC\` | this module |
//! | OSC `4;i;?` | `ESC]4;i;rgb:0000/0000/0000 ESC\` | this module |
//!
//! The colour queries are answered here, not by libghostty, because libghostty
//! echoes the query's terminator (BEL for a BEL-terminated query) while Node
//! always answers with ST; the actor removes those OSCs from the stream before
//! `vt_write` so libghostty never answers them itself.

use libghostty_vt::style::RgbColor;
use libghostty_vt::terminal::{
    ConformanceLevel, DeviceAttributeFeature, DeviceAttributes, DeviceType,
    PrimaryDeviceAttributes, SecondaryDeviceAttributes, Terminal, TertiaryDeviceAttributes,
};

/// Node's DA1 answer (`src/server.ts:403`).
pub const DA1_RESPONSE: &[u8] = b"\x1b[?62;22c";
/// Node's DA2 answer (`src/server.ts:495`).
pub const DA2_RESPONSE: &[u8] = b"\x1b[>0;382;0c";
/// Node's XTVERSION answer (`src/server.ts:513`).
pub const XTVERSION_RESPONSE: &[u8] = b"\x1bP>|pty(0.8)\x1b\\";
/// The version string inside [`XTVERSION_RESPONSE`].
pub const XTVERSION_STRING: &str = "pty(0.8)";

/// The DA1/DA2/DA3 report libghostty gives once [`install`] ran.
pub const DEVICE_ATTRIBUTES: DeviceAttributes = DeviceAttributes {
    primary: PrimaryDeviceAttributes::new(
        ConformanceLevel::LEVEL_2,
        &[DeviceAttributeFeature::ANSI_COLOR],
    ),
    secondary: SecondaryDeviceAttributes {
        device_type: DeviceType(0),
        firmware_version: 382,
        rom_cartridge: 0,
    },
    tertiary: TertiaryDeviceAttributes { unit_id: 0 },
};

/// Node's default foreground (`rgb:c0c0/c0c0/c0c0`).
pub const DEFAULT_FG: RgbColor = RgbColor {
    r: 0xc0,
    g: 0xc0,
    b: 0xc0,
};
/// Node's default background (`rgb:0000/0000/0000`).
pub const DEFAULT_BG: RgbColor = RgbColor { r: 0, g: 0, b: 0 };

/// Install the callbacks and defaults that make libghostty answer DA1, DA2
/// and XTVERSION with Node's bytes, and give it Node's default colours (so
/// anything that reads them — `fg_color()`, the render state — sees what
/// Node's xterm had).
pub fn install(term: &mut Terminal<'_, '_>) {
    term.on_device_attributes(|_t| Some(DEVICE_ATTRIBUTES))
        .expect("install on_device_attributes");
    term.on_xtversion(|_t| Some(XTVERSION_STRING))
        .expect("install on_xtversion");
    let _ = term.set_default_fg_color(Some(DEFAULT_FG));
    let _ = term.set_default_bg_color(Some(DEFAULT_BG));
    // The palette is left as libghostty's default: OSC 4 queries never reach
    // libghostty (the actor answers them with Node's all-black constant), so
    // the palette only matters for what renderers read back.
}

/// Node's reply to a colour query, given the `(id, index)` pair from
/// [`crate::strip::Osc::color_query`]: OSC 10 → `c0c0/c0c0/c0c0`, OSC 11 →
/// `0000/0000/0000`, OSC 4 with an index → `4;<i>;rgb:0000/0000/0000`; OSC 4
/// without a parsable index is consumed silently (`src/server.ts:459-490`).
pub fn color_query_reply(id: u32, index: Option<u32>) -> Option<Vec<u8>> {
    match (id, index) {
        (10, _) => Some(b"\x1b]10;rgb:c0c0/c0c0/c0c0\x1b\\".to_vec()),
        (11, _) => Some(b"\x1b]11;rgb:0000/0000/0000\x1b\\".to_vec()),
        (4, Some(i)) => Some(format!("\x1b]4;{i};rgb:0000/0000/0000\x1b\\").into_bytes()),
        _ => None,
    }
}

/// Headless fallback using colors the child has already set in this terminal.
/// Untouched palette entries retain the existing all-black query response.
pub fn current_color_query_reply(term: &Terminal<'_, '_>, id: u32, index: Option<u32>) -> Option<Vec<u8>> {
    let current = match (id, index) {
        (10, _) => term.fg_color().ok().flatten(),
        (11, _) => term.bg_color().ok().flatten(),
        (4, Some(i)) if i < 256 => {
            let current = term.color_palette().ok()?.0[i as usize];
            let default = term.default_color_palette().ok()?.0[i as usize];
            (current != default).then_some(current)
        }
        _ => None,
    };
    if let Some(color) = current {
        let rgb = format!(
            "rgb:{:02x}{:02x}/{:02x}{:02x}/{:02x}{:02x}",
            color.r, color.r, color.g, color.g, color.b, color.b
        );
        let payload = if id == 4 {
            format!("4;{};{rgb}", index?)
        } else {
            format!("{id};{rgb}")
        };
        return Some(format!("\x1b]{payload}\x1b\\").into_bytes());
    }
    color_query_reply(id, index)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_replies_are_nodes_bytes() {
        assert_eq!(color_query_reply(10, None).unwrap(), b"\x1b]10;rgb:c0c0/c0c0/c0c0\x1b\\");
        assert_eq!(color_query_reply(11, None).unwrap(), b"\x1b]11;rgb:0000/0000/0000\x1b\\");
        assert_eq!(color_query_reply(4, Some(255)).unwrap(), b"\x1b]4;255;rgb:0000/0000/0000\x1b\\");
        assert_eq!(color_query_reply(4, None), None);
    }
}
