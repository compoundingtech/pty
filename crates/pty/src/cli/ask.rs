//! The `[Y/n]` prompts in `attach` and `restart`: write the question to
//! stdout, read one line from stdin, and count only an answer someone gave
//! (decision 0020).
//!
//! Node's `readline` prompt has no tty check, and only a lone `n` declines;
//! here `no` declines too. At EOF Node's question never resolves, and the
//! process exits 0 having done nothing.
//!
//! node: src/cli.ts:4069-4078 (`ask`), 3922-3926 and 1836-1840 (callers).

#![allow(dead_code)] // callers: the restart / dead-session prompts

use std::io::{BufRead, IsTerminal, Write};

/// Did the answer decline? A lone `n` or `no`, in any case. Node compares
/// `answer.toLowerCase() === "n"`, so its `no` proceeds (decision 0020).
pub fn declined(answer: &str) -> bool {
    matches!(answer.to_lowercase().as_str(), "n" | "no")
}

/// What a restart prompt was told.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Consent {
    Yes,
    /// A lone `n` or `no`.
    No,
    /// Stdin was at EOF, or is not a terminal and did not say `y` or `yes`.
    NoAnswer,
}

/// Print `prompt` (no newline) and read one line, counting only an answer.
/// At a terminal, as in Node, anything but a decline says yes, an empty line
/// included. Anywhere else only `y` or `yes` does. EOF is never an answer.
pub fn ask_consent(prompt: &str) -> Consent {
    let answer = read_answer(prompt);
    consent(answer.as_deref(), std::io::stdin().is_terminal())
}

fn consent(answer: Option<&str>, terminal: bool) -> Consent {
    match answer {
        None => Consent::NoAnswer,
        Some(answer) if declined(answer) => Consent::No,
        Some(_) if terminal => Consent::Yes,
        Some(answer) if answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes") => {
            Consent::Yes
        }
        Some(_) => Consent::NoAnswer,
    }
}

/// Print `prompt` and read one line without its line terminator. `None` at
/// EOF or on a read error.
fn read_answer(prompt: &str) -> Option<String> {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(prompt.as_bytes());
    let _ = out.flush();
    drop(out);
    let mut line = String::new();
    match std::io::stdin().lock().read_line(&mut line) {
        Ok(0) | Err(_) => return None,
        Ok(_) => {}
    }
    while line.ends_with('\n') || line.ends_with('\r') {
        line.pop();
    }
    Some(line)
}

#[cfg(test)]
mod tests {
    use super::{Consent, consent, declined};

    #[test]
    fn a_lone_n_or_no_declines() {
        for no in ["n", "N", "no", "No", "NO"] {
            assert!(declined(no), "{no:?}");
        }
        for other in ["", "y", "nope", "no thanks", " n"] {
            assert!(!declined(other), "{other:?}");
        }
    }

    /// `attach`'s dead-session prompt and `restart`'s running-session prompt
    /// both ask through [`super::ask_consent`], so this covers both.
    #[test]
    fn consent_needs_an_answer_and_off_a_terminal_a_yes() {
        for terminal in [true, false] {
            assert_eq!(
                consent(None, terminal),
                Consent::NoAnswer,
                "terminal={terminal}"
            );
            for no in ["n", "N", "no", "No", "NO"] {
                assert_eq!(
                    consent(Some(no), terminal),
                    Consent::No,
                    "{no:?} terminal={terminal}"
                );
            }
            for yes in ["y", "Y", "yes", "YES"] {
                assert_eq!(
                    consent(Some(yes), terminal),
                    Consent::Yes,
                    "{yes:?} terminal={terminal}"
                );
            }
        }
        for other in ["", "nope", "future-relay-input"] {
            assert_eq!(
                consent(Some(other), true),
                Consent::Yes,
                "{other:?} at a terminal"
            );
            assert_eq!(
                consent(Some(other), false),
                Consent::NoAnswer,
                "{other:?} off a terminal"
            );
        }
    }
}
