//! Streaming scanner over the child's output.
//!
//! Node's daemon (`src/server.ts:262-274`) removes terminal *queries* from the
//! bytes it broadcasts as DATA — if they reached the attached client's real
//! terminal, that terminal would answer them and the answer would arrive as
//! garbage keystrokes. Node does this with regexes over each chunk; this
//! module does it with a small VT tokenizer so a query split across two PTY
//! reads is still recognised, and so the same pass can pick out the sequences
//! Node tracks with parser hooks (`src/server.ts:343-517`): DEC private mode
//! set/reset, the kitty keyboard push/pop stack, and the OSC 9/99/777
//! notifications.
//!
//! Only CSI and OSC sequences are tokenized; everything else is passed through
//! untouched as [`Token::Raw`]. A chunk that ends inside a candidate sequence
//! keeps the partial bytes until the next chunk completes it (bounded: a CSI
//! longer than [`MAX_CSI`] bytes or an OSC longer than [`MAX_OSC`] bytes is
//! given up on and passed through).

/// Longest CSI sequence the scanner will hold back while waiting for its
/// final byte. Everything Node strips or tracks is far shorter.
pub const MAX_CSI: usize = 64;
/// Longest OSC payload the scanner will buffer (notifications need the whole
/// payload for their title/body).
pub const MAX_OSC: usize = 4096;

/// How an OSC sequence was terminated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Terminator {
    /// `BEL` (0x07).
    Bel,
    /// `ESC \` (ST).
    St,
}

/// A parsed CSI sequence (`ESC [ <prefix> <params> <intermediates> <final>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Csi {
    /// The exact bytes of the sequence.
    pub raw: Vec<u8>,
    /// Private-marker byte (`<`, `=`, `>`, `?`) if the sequence has one.
    pub prefix: Option<u8>,
    /// Intermediate bytes (0x20..=0x2f), e.g. `$` in `CSI ? 2026 $ p`.
    pub intermediates: Vec<u8>,
    /// Numeric parameters. Empty when none were given; a missing value in a
    /// `;`-separated list is `0` (xterm's default). Sub-parameters after `:`
    /// are dropped.
    pub params: Vec<u16>,
    /// The final byte (0x40..=0x7e).
    pub final_byte: u8,
}

impl Csi {
    /// First parameter, or `default` when the sequence has none.
    pub fn param(&self, i: usize, default: u16) -> u16 {
        self.params.get(i).copied().unwrap_or(default)
    }

    /// Primary device attributes query Node answers: `ESC[c` / `ESC[0c`
    /// (`src/server.ts:397-405`: `params.length === 0 || params[0] === 0`).
    pub fn is_da1_query(&self) -> bool {
        self.final_byte == b'c'
            && self.prefix.is_none()
            && self.intermediates.is_empty()
            && (self.params.is_empty() || self.params[0] == 0)
    }

    /// Secondary device attributes query: `CSI > ... c`
    /// (`src/server.ts:491-498`, answered for any parameters).
    pub fn is_da2_query(&self) -> bool {
        self.final_byte == b'c' && self.prefix == Some(b'>') && self.intermediates.is_empty()
    }

    /// Cursor position report request `ESC[6n`
    /// (`src/server.ts:499-508`: exactly one parameter, `6`).
    pub fn is_dsr_query(&self) -> bool {
        self.final_byte == b'n'
            && self.prefix.is_none()
            && self.intermediates.is_empty()
            && self.params.len() == 1
            && self.params[0] == 6
    }

    /// XTVERSION query `CSI > 0 q` (`src/server.ts:509-516`, answered for any
    /// parameters).
    pub fn is_xtversion_query(&self) -> bool {
        self.final_byte == b'q' && self.prefix == Some(b'>') && self.intermediates.is_empty()
    }

    /// CSI 14t, 16t and 18t ask for window, cell and grid geometry.
    pub fn size_query(&self) -> Option<u16> {
        if self.final_byte == b't' && self.prefix.is_none() && self.intermediates.is_empty() {
            match self.params.as_slice() {
                [14] | [16] | [18] => Some(self.params[0]),
                _ => None,
            }
        } else {
            None
        }
    }

    /// True when the session terminal answers the query itself and the
    /// attached client's terminal must not answer it again.
    ///
    /// Node's regexes (`src/server.ts:264-274`) match only the shortest spelling
    /// of each (`ESC[c`, `ESC[>c`, `ESC[6n`, `ESC[>0q`); this strips every
    /// spelling Node *answers* (`ESC[0c`, `ESC[>0c`, `ESC[>q`) as well, because
    /// a leaked spelling would be answered a second time by the client's real
    /// terminal. The same applies to DSR 5n, DECRQM and the kitty keyboard
    /// state query, which libghostty also answers locally, and to the geometry
    /// queries answered from the session's size.
    pub fn is_stripped_query(&self) -> bool {
        self.is_da1_query()
            || self.is_da2_query()
            || self.is_dsr_query()
            || self.is_xtversion_query()
            || (self.final_byte == b'n'
                && self.prefix.is_none()
                && self.intermediates.is_empty()
                && self.params.as_slice() == [5])
            || (self.final_byte == b'p'
                && self.prefix == Some(b'?')
                && self.intermediates == [b'$']
                && !self.params.is_empty())
            || (self.final_byte == b'u'
                && self.prefix == Some(b'?')
                && self.intermediates.is_empty()
                && self.params.is_empty())
            || self.size_query().is_some()
    }

    /// Kitty keyboard push `CSI > flags u` → the pushed flags
    /// (`src/server.ts:378-385`; no parameter pushes `0`, xterm's default).
    pub fn kitty_push(&self) -> Option<u8> {
        if self.final_byte == b'u' && self.prefix == Some(b'>') && self.intermediates.is_empty() {
            Some(self.param(0, 0).min(u8::MAX as u16) as u8)
        } else {
            None
        }
    }

    /// Kitty keyboard pop `CSI < [n] u` (`src/server.ts:386-391`: Node pops
    /// exactly one entry whatever `n` says).
    pub fn is_kitty_pop(&self) -> bool {
        self.final_byte == b'u' && self.prefix == Some(b'<') && self.intermediates.is_empty()
    }

    /// DEC private mode set (`CSI ? ... h`) or reset (`CSI ? ... l`) → the
    /// modes and whether they were set.
    pub fn dec_modes(&self) -> Option<(&[u16], bool)> {
        if self.prefix == Some(b'?') && self.intermediates.is_empty() {
            match self.final_byte {
                b'h' => Some((&self.params, true)),
                b'l' => Some((&self.params, false)),
                _ => None,
            }
        } else {
            None
        }
    }
}

/// A parsed OSC sequence (`ESC ] <payload> BEL|ST`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Osc {
    /// The exact bytes of the sequence, terminator included.
    pub raw: Vec<u8>,
    /// Everything between `ESC ]` and the terminator.
    pub payload: Vec<u8>,
    /// How it was terminated.
    pub terminator: Terminator,
}

impl Osc {
    /// The numeric identifier before the first `;` (or the whole payload when
    /// there is no `;`), and the data after it (`""` when absent). This is the
    /// split xterm's `registerOscHandler` makes.
    pub fn split(&self) -> (Option<u32>, &[u8]) {
        let (id, data) = match self.payload.iter().position(|&b| b == b';') {
            Some(i) => (&self.payload[..i], &self.payload[i + 1..]),
            None => (&self.payload[..], &self.payload[self.payload.len()..]),
        };
        let id = std::str::from_utf8(id).ok().and_then(|s| s.parse::<u32>().ok());
        (id, data)
    }

    /// The colour query this OSC is, if any: `Some((10, None))` for
    /// `OSC 10 ; ?`, `Some((11, None))` for `OSC 11 ; ?`, and
    /// `Some((4, index))` for `OSC 4 ; index ; ?` (Node: `src/server.ts:459-490`
    /// — OSC 10/11 must be exactly `?`; OSC 4 is consumed whenever the data
    /// contains a `?`, and answered when it starts with a number).
    pub fn color_query(&self) -> Option<(u32, Option<u32>)> {
        let (id, data) = self.split();
        match id? {
            10 | 11 if data == b"?" => Some((id?, None)),
            4 if data.contains(&b'?') => {
                let digits: String = std::str::from_utf8(data)
                    .unwrap_or("")
                    .chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect();
                Some((4, digits.parse::<u32>().ok()))
            }
            _ => None,
        }
    }
}

/// One item of scanner output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    /// Bytes carrying nothing the scanner cares about (text, other escapes).
    Raw(Vec<u8>),
    /// Full terminal reset (`ESC c`).
    Ris,
    /// A complete CSI sequence.
    Csi(Csi),
    /// A complete OSC sequence.
    Osc(Osc),
}

impl Token {
    /// The exact bytes this token stands for.
    pub fn raw(&self) -> &[u8] {
        match self {
            Token::Raw(b) => b,
            Token::Ris => b"\x1bc",
            Token::Csi(c) => &c.raw,
            Token::Osc(o) => &o.raw,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    /// Saw `ESC`.
    Esc,
    /// Inside `ESC [`.
    Csi,
    /// Inside `ESC ]`.
    Osc,
    /// Inside `ESC ]`, saw `ESC` (an ST needs `\` next).
    OscEsc,
}

/// The streaming tokenizer. Feed it chunks in order; it yields tokens whose
/// raw bytes, concatenated, reproduce the input (minus any partial sequence
/// still held back at the end of the last chunk).
#[derive(Debug)]
pub struct OutputScanner {
    state: State,
    /// Bytes of the in-progress escape sequence (starts with `ESC`).
    seq: Vec<u8>,
}

impl Default for OutputScanner {
    fn default() -> Self {
        Self::new()
    }
}

impl OutputScanner {
    /// A fresh scanner in the ground state.
    pub fn new() -> Self {
        OutputScanner {
            state: State::Ground,
            seq: Vec::new(),
        }
    }

    /// Forget any partial sequence (used on terminal reset).
    pub fn reset(&mut self) {
        self.state = State::Ground;
        self.seq.clear();
    }

    /// Bytes held back because they might start a sequence of interest.
    pub fn pending(&self) -> &[u8] {
        &self.seq
    }

    /// Tokenize a chunk.
    pub fn feed(&mut self, data: &[u8]) -> Vec<Token> {
        let mut out = Vec::new();
        let mut raw = Vec::new();
        let flush_raw = |raw: &mut Vec<u8>, out: &mut Vec<Token>| {
            if !raw.is_empty() {
                out.push(Token::Raw(std::mem::take(raw)));
            }
        };
        let mut i = 0;
        while i < data.len() {
            let b = data[i];
            i += 1;
            match self.state {
                State::Ground => {
                    if b == 0x1b {
                        self.seq.clear();
                        self.seq.push(b);
                        self.state = State::Esc;
                    } else {
                        raw.push(b);
                    }
                }
                State::Esc => match b {
                    b'c' => {
                        self.seq.clear();
                        self.state = State::Ground;
                        flush_raw(&mut raw, &mut out);
                        out.push(Token::Ris);
                    }
                    b'[' => {
                        self.seq.push(b);
                        self.state = State::Csi;
                    }
                    b']' => {
                        self.seq.push(b);
                        self.state = State::Osc;
                    }
                    0x1b => {
                        // ESC ESC: the first was a lone escape.
                        raw.extend_from_slice(&self.seq);
                        self.seq.clear();
                        self.seq.push(b);
                    }
                    _ => {
                        raw.extend_from_slice(&self.seq);
                        raw.push(b);
                        self.seq.clear();
                        self.state = State::Ground;
                    }
                },
                State::Csi => {
                    if (0x40..=0x7e).contains(&b) {
                        self.seq.push(b);
                        let csi = parse_csi(&self.seq);
                        self.seq.clear();
                        self.state = State::Ground;
                        flush_raw(&mut raw, &mut out);
                        out.push(Token::Csi(csi));
                    } else if b == 0x1b {
                        // Aborted by a new escape.
                        raw.extend_from_slice(&self.seq);
                        self.seq.clear();
                        self.seq.push(b);
                        self.state = State::Esc;
                    } else if b == 0x18 || b == 0x1a {
                        // CAN / SUB abort the sequence.
                        raw.extend_from_slice(&self.seq);
                        raw.push(b);
                        self.seq.clear();
                        self.state = State::Ground;
                    } else {
                        self.seq.push(b);
                        if self.seq.len() > MAX_CSI {
                            raw.extend_from_slice(&self.seq);
                            self.seq.clear();
                            self.state = State::Ground;
                        }
                    }
                }
                State::Osc => match b {
                    0x07 => {
                        self.seq.push(b);
                        let osc = make_osc(&self.seq, Terminator::Bel);
                        self.seq.clear();
                        self.state = State::Ground;
                        flush_raw(&mut raw, &mut out);
                        out.push(Token::Osc(osc));
                    }
                    0x1b => {
                        self.seq.push(b);
                        self.state = State::OscEsc;
                    }
                    0x18 | 0x1a => {
                        raw.extend_from_slice(&self.seq);
                        raw.push(b);
                        self.seq.clear();
                        self.state = State::Ground;
                    }
                    _ => {
                        self.seq.push(b);
                        if self.seq.len() > MAX_OSC + 2 {
                            raw.extend_from_slice(&self.seq);
                            self.seq.clear();
                            self.state = State::Ground;
                        }
                    }
                },
                State::OscEsc => {
                    if b == b'\\' {
                        self.seq.push(b);
                        let osc = make_osc(&self.seq, Terminator::St);
                        self.seq.clear();
                        self.state = State::Ground;
                        flush_raw(&mut raw, &mut out);
                        out.push(Token::Osc(osc));
                    } else {
                        // ESC not followed by `\`: the OSC is abandoned and
                        // this ESC starts something new.
                        let esc = self.seq.pop();
                        debug_assert_eq!(esc, Some(0x1b));
                        raw.extend_from_slice(&self.seq);
                        self.seq.clear();
                        self.seq.push(0x1b);
                        self.state = State::Esc;
                        // Re-process `b` in the Esc state.
                        i -= 1;
                    }
                }
            }
        }
        flush_raw(&mut raw, &mut out);
        out
    }
}

fn parse_csi(seq: &[u8]) -> Csi {
    // seq = ESC [ body final
    let body = &seq[2..seq.len() - 1];
    let final_byte = seq[seq.len() - 1];
    let mut prefix = None;
    let mut params_bytes: &[u8] = body;
    if let Some(&first) = body.first()
        && (0x3c..=0x3f).contains(&first)
    {
        prefix = Some(first);
        params_bytes = &body[1..];
    }
    // Intermediates: trailing 0x20..=0x2f bytes.
    let inter_start = params_bytes
        .iter()
        .position(|b| (0x20..=0x2f).contains(b))
        .unwrap_or(params_bytes.len());
    let intermediates = params_bytes[inter_start..].to_vec();
    let params_bytes = &params_bytes[..inter_start];
    let mut params = Vec::new();
    if !params_bytes.is_empty() {
        for part in params_bytes.split(|&b| b == b';') {
            let digits: &[u8] = match part.iter().position(|&b| b == b':') {
                Some(p) => &part[..p],
                None => part,
            };
            let mut v: u32 = 0;
            for &d in digits {
                if d.is_ascii_digit() {
                    v = (v * 10 + (d - b'0') as u32).min(u16::MAX as u32);
                }
            }
            params.push(v as u16);
        }
    }
    Csi {
        raw: seq.to_vec(),
        prefix,
        intermediates,
        params,
        final_byte,
    }
}

fn make_osc(seq: &[u8], terminator: Terminator) -> Osc {
    let end = match terminator {
        Terminator::Bel => seq.len() - 1,
        Terminator::St => seq.len() - 2,
    };
    Osc {
        raw: seq.to_vec(),
        payload: seq[2..end].to_vec(),
        terminator,
    }
}

/// One-shot form of the scanner: the bytes of `data` with every terminal
/// query removed — DA1/DA2/DSR/XTVERSION and the OSC 10/11/4 `?` colour
/// queries (BEL- or ST-terminated). Port of Node's `stripTerminalQueries`
/// (`src/server.ts:264-274`); the daemon uses the streaming scanner through
/// [`crate::TerminalActor::write`] instead.
pub fn strip_terminal_queries(data: &[u8]) -> Vec<u8> {
    let mut scanner = OutputScanner::new();
    let mut out = Vec::with_capacity(data.len());
    for tok in scanner.feed(data) {
        match &tok {
            Token::Csi(c) if c.is_stripped_query() => {}
            Token::Osc(o) if o.color_query().is_some() => {}
            _ => out.extend_from_slice(tok.raw()),
        }
    }
    out.extend_from_slice(scanner.pending());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip(s: &str) -> String {
        String::from_utf8(strip_terminal_queries(s.as_bytes())).unwrap()
    }

    /// node: tests/terminal-queries.test.ts:20-78
    #[test]
    fn strips_each_query_form() {
        for q in [
            "\x1b]10;?\x07",
            "\x1b]10;?\x1b\\",
            "\x1b]11;?\x07",
            "\x1b]11;?\x1b\\",
            "\x1b]4;7;?\x07",
            "\x1b]4;255;?\x07",
            "\x1b]4;0;?\x1b\\",
            "\x1b[c",
            "\x1b[>c",
            "\x1b[6n",
            "\x1b[>0q",
        ] {
            assert_eq!(strip(q), "", "{q:?}");
        }
    }

    /// node: tests/terminal-queries.test.ts:64-71
    #[test]
    fn preserves_text_and_normal_sequences() {
        assert_eq!(strip("hello world"), "hello world");
        let ansi = "\x1b[1;31mred bold\x1b[0m";
        assert_eq!(strip(ansi), ansi);
    }

    #[test]
    fn strips_state_queries_answered_by_the_session_terminal() {
        assert_eq!(
            strip("a\x1b[?2004$pb\x1b[5nc\x1b[?ud"),
            "abcd"
        );
        assert_eq!(strip("\x1b[?2004h\x1b[?2004l"), "\x1b[?2004h\x1b[?2004l");
    }

    /// node: tests/terminal-queries.test.ts:73-80
    #[test]
    fn strips_embedded_and_multiple() {
        assert_eq!(strip("before\x1b]11;?\x07after"), "beforeafter");
        assert_eq!(strip("\x1b]10;?\x07\x1b]11;?\x07\x1b[c"), "");
    }

    /// node: tests/terminal-queries.test.ts:82-93
    #[test]
    fn preserves_non_query_osc() {
        let title = "\x1b]0;my title\x07";
        assert_eq!(strip(title), title);
        let set = "\x1b]10;rgb:ffff/0000/0000\x07";
        assert_eq!(strip(set), set);
    }

    #[test]
    fn query_split_across_chunks_is_still_stripped() {
        let mut sc = OutputScanner::new();
        let mut out = Vec::new();
        for chunk in [&b"abc\x1b"[..], b"]11;", b"?\x07def", b"\x1b[", b"6", b"nX"] {
            for tok in sc.feed(chunk) {
                match &tok {
                    Token::Csi(c) if c.is_stripped_query() => {}
                    Token::Osc(o) if o.color_query().is_some() => {}
                    _ => out.extend_from_slice(tok.raw()),
                }
            }
        }
        assert_eq!(out, b"abcdefX");
        assert!(sc.pending().is_empty());
    }

    #[test]
    fn tokens_reproduce_input_bytes() {
        let input = b"a\x1b[?1049h\x1b[>7u\x1bPfoo\x1b\\b\x1b]0;t\x1b\\\x1bMz\x1b\x1bq";
        let mut sc = OutputScanner::new();
        let toks = sc.feed(input);
        let mut joined = Vec::new();
        for t in &toks {
            joined.extend_from_slice(t.raw());
        }
        joined.extend_from_slice(sc.pending());
        assert_eq!(joined, input);
        assert!(matches!(&toks[1], Token::Csi(c) if c.prefix == Some(b'?') && c.params == vec![1049] && c.final_byte == b'h'));
        assert!(matches!(&toks[2], Token::Csi(c) if c.kitty_push() == Some(7)));
    }

    #[test]
    fn csi_params_and_intermediates() {
        let mut sc = OutputScanner::new();
        let toks = sc.feed(b"\x1b[?2026$p\x1b[1;;3m\x1b[38:2:1:2:3m\x1b[<u");
        let Token::Csi(a) = &toks[0] else { panic!() };
        assert_eq!(a.prefix, Some(b'?'));
        assert_eq!(a.params, vec![2026]);
        assert_eq!(a.intermediates, b"$");
        assert_eq!(a.final_byte, b'p');
        let Token::Csi(b) = &toks[1] else { panic!() };
        assert_eq!(b.params, vec![1, 0, 3]);
        let Token::Csi(c) = &toks[2] else { panic!() };
        assert_eq!(c.params, vec![38]);
        let Token::Csi(d) = &toks[3] else { panic!() };
        assert!(d.is_kitty_pop());
        assert_eq!(d.params, Vec::<u16>::new());
    }

    #[test]
    fn osc_color_query_forms() {
        let mut sc = OutputScanner::new();
        let toks = sc.feed(b"\x1b]10;?\x07\x1b]4;17;?\x1b\\\x1b]4;abc;?\x07\x1b]11;rgb:0/0/0\x07\x1b]4;1;?;2;?\x07");
        let q: Vec<_> = toks
            .iter()
            .map(|t| match t {
                Token::Osc(o) => o.color_query(),
                _ => None,
            })
            .collect();
        assert_eq!(q, vec![Some((10, None)), Some((4, Some(17))), Some((4, None)), None, Some((4, Some(1)))]);
    }

    #[test]
    fn abandoned_osc_passes_through() {
        let mut sc = OutputScanner::new();
        let input = b"\x1b]0;title\x1b[31mred";
        let toks = sc.feed(input);
        let mut joined = Vec::new();
        for t in &toks {
            joined.extend_from_slice(t.raw());
        }
        assert_eq!(joined, input);
        assert!(toks.iter().any(|t| matches!(t, Token::Csi(c) if c.params == vec![31])));
    }
}
