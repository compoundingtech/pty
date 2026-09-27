//! Terminal reset sequences, byte-for-byte the pty project's
//! `src/client.ts:37-59`.

/// Reset terminal modes a program may have enabled, so the terminal isn't left
/// "poisoned" after detach/peek (alt screen, mouse tracking, hidden cursor,
/// bracketed paste, …). Does not clear screen content.
pub const TERMINAL_SANITIZE: &str = concat!(
    "\x1b[?1049l", // leave alternate screen buffer
    "\x1b[?1l",    // reset cursor keys to normal (DECCKM)
    "\x1b[?7h",    // re-enable autowrap (DECAWM)
    "\x1b[?6l",    // reset origin mode (DECOM)
    "\x1b[?1000l", // disable mouse click tracking
    "\x1b[?1002l", // disable mouse button-event tracking
    "\x1b[?1003l", // disable mouse any-event tracking
    "\x1b[?1004l", // disable focus event reporting
    "\x1b[?1006l", // disable SGR mouse mode
    "\x1b[?25h",   // show cursor
    "\x1b[?2004l", // disable bracketed paste
    "\x1b[4l",     // reset insert mode (IRM) to replace
    "\x1b[r",      // reset scroll region (DECSTBM)
    "\x1b[0m",     // reset SGR attributes
    "\x1b[0 q",    // reset cursor style
    "\x1b>",       // reset application keypad mode (DECKPNM)
    "\x1b(B",      // reset G0 charset to ASCII
    "\x1b[<99u",   // pop all Kitty keyboard protocol levels
);

/// Move the cursor to the bottom of the visible screen so status messages
/// ("\[detached\]") land below the session content, not mid-screen.
pub const CURSOR_TO_BOTTOM: &str = "\x1b[999;1H";

/// Clear the screen and home the cursor — written before every SCREEN replay
/// in `attach` (`client.ts:646-650`).
pub const CLEAR_SCREEN_HOME: &str = "\x1b[2J\x1b[H";
