//! The `[Y/n]` prompts in `attach` and `restart`: write the question to
//! stdout, read one line from stdin, and count only an answer someone gave
//! (decision 0020).
//!
//! Node's `readline` prompt has no tty check, and only a lone `n` declines.
//! At EOF its question never resolves, and the process exits 0 having done
//! nothing.
//!
//! node: src/cli.ts:4069-4078 (`ask`), 3922-3926 and 1836-1840 (callers).

#![allow(dead_code)] // callers: the restart / dead-session prompts

use std::io::{BufRead, IsTerminal, Write};

/// Did the answer decline? Node compares `answer.toLowerCase() === "n"`
/// (the restart prompts) — only a lone `n`/`N` says no; anything else,
/// including an empty line, proceeds.
pub fn declined(answer: &str) -> bool {
    answer.to_lowercase() == "n"
}

/// What a restart prompt was told.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Consent {
    Yes,
    /// A lone `n`.
    No,
    /// Stdin was at EOF, or is not a terminal and did not say `y` or `yes`.
    NoAnswer,
}

/// Print `prompt` (no newline) and read one line, counting only an answer.
/// At a terminal, as in Node, anything but a lone `n` says yes, an empty line
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
    fn only_a_lone_n_declines() {
        assert!(declined("n"));
        assert!(declined("N"));
        assert!(!declined(""));
        assert!(!declined("y"));
        assert!(!declined("no"));
    }

    #[test]
    fn consent_needs_an_answer_and_off_a_terminal_a_yes() {
        for terminal in [true, false] {
            assert_eq!(
                consent(None, terminal),
                Consent::NoAnswer,
                "terminal={terminal}"
            );
            for no in ["n", "N"] {
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
        for other in ["", "no", "future-relay-input"] {
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
