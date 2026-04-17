// Bracketed paste (DECSET 2004) helpers.
//
// When a receiving terminal has bracketed paste mode enabled, pasted
// text is wrapped in these markers so applications can distinguish
// "user typed this" from "user pasted this". Shells suppress
// history/autocomplete during paste, vim enters paste mode, and TUI
// agents (claude, aider) treat the block as one input event rather
// than a sequence of keystrokes.
//
// `send --paste` wraps the entire payload in START…END so multi-line
// prompts injected into agent sessions don't get submitted partway.

/** Sent BEFORE pasted content. CSI 200 ~. */
export const BRACKETED_PASTE_START = "\x1b[200~";
/** Sent AFTER pasted content. CSI 201 ~. */
export const BRACKETED_PASTE_END = "\x1b[201~";
