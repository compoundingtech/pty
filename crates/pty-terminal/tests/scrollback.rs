//! The scrollback promise: a terminal asked for N lines of history retains N
//! lines, and says so.
//!
//! libghostty's `max_scrollback` is a byte budget for the history page list,
//! not a line count — its own doc comment says "lines" — and the page list
//! evicts whole pages. Passing 10 000 for "10 000 lines" buys one page: 745
//! rows at 80 columns, 456 at 200, 3 310 at 20. Every case here is about the
//! conversion that makes the declared line count true, and about the reported
//! numbers being the truth rather than the request.

use pty_terminal::actor::{DEFAULT_SCROLLBACK, MAX_SCROLLBACK_BYTES};
use pty_terminal::{Range, TerminalActor};

/// Feed `n` numbered lines, each short enough that it cannot wrap, so a row
/// written is a row of history and nothing here depends on reflow.
fn fill(a: &mut TerminalActor, n: usize) {
    for i in 0..n {
        a.write(format!("L{i}\r\n").as_bytes());
    }
}

/// The oldest of 10 000 lines is still reachable, at the geometry the
/// terminal was built with. This is the substrate's promised replay window;
/// before the byte conversion it retained 745 of them.
#[test]
fn ten_thousand_lines_of_history_are_all_retained() {
    let mut a = TerminalActor::new(24, 80, DEFAULT_SCROLLBACK);
    fill(&mut a, 10_008);

    // 10 008 written lines plus the row the cursor sits on, all within the
    // 10 024-row capacity of a 24-row terminal with 10 000 lines of history.
    assert!(
        a.buffer_length() >= 10_008,
        "expected every line retained, got {} rows",
        a.buffer_length()
    );
    let text = a.plain(Range::Full);
    let first = text.lines().next().unwrap_or("");
    assert_eq!(first, "L0", "the oldest line is still the oldest line");
    assert!(text.contains("\nL9999\n"), "and the newest are there too");
}

/// The same promise at widths where the byte cost of a row differs by 5x. A
/// budget derived from the line count has to scale with the width, or a wide
/// terminal keeps a fraction of the history a narrow one does.
#[test]
fn the_promise_holds_at_every_width() {
    for cols in [20u16, 80, 200, 400] {
        let mut a = TerminalActor::new(24, cols, 10_000);
        fill(&mut a, 10_008);
        assert!(
            a.buffer_length() >= 10_008,
            "{cols} columns: expected 10 008 rows, got {}",
            a.buffer_length()
        );
        assert_eq!(
            a.plain(Range::Full).lines().next().unwrap_or(""),
            "L0",
            "{cols} columns: the oldest line was evicted"
        );
    }
}

/// A small scrollback is a line promise too, and the promise is a floor: the
/// history is a list of pages and libghostty never holds less than one, so a
/// terminal asked for 100 lines keeps at least 100 and in practice more.
/// Bounded, though — a terminal asked for 100 lines does not keep 5 000.
#[test]
fn a_small_scrollback_keeps_at_least_what_it_promised() {
    let mut a = TerminalActor::new(24, 80, 100);
    fill(&mut a, 5_000);
    let text = a.plain(Range::Full);
    for i in 4_900..4_999 {
        assert!(
            text.contains(&format!("L{i}\n")),
            "the promised window must be there: L{i} is missing"
        );
    }
    assert!(text.contains("L4999"), "including the newest line");
    assert!(!text.contains("L0\n"), "and the far past is evicted");
    assert!(
        a.buffer_length() < 5_000,
        "a 100-line request must not keep 5 000 rows, got {}",
        a.buffer_length()
    );
}

/// `scrollback_used` and `scrollback_capacity` are the numbers a consumer
/// budgets against, so they have to describe the terminal rather than the
/// request.
#[test]
fn used_and_capacity_are_honest() {
    let mut a = TerminalActor::new(24, 80, DEFAULT_SCROLLBACK);
    assert_eq!(a.scrollback(), DEFAULT_SCROLLBACK);
    assert_eq!(a.scrollback_capacity(), 24 + DEFAULT_SCROLLBACK);
    assert_eq!(a.scrollback_used(), 24, "an empty terminal is its viewport");

    fill(&mut a, 10_008);
    assert_eq!(a.scrollback_used(), a.buffer_length());
    // Capacity is a floor, not a ceiling (see its doc comment): what it
    // promises has to actually be there.
    assert!(
        a.scrollback_used() >= 10_008,
        "capacity is not honest if the rows are not there: {}",
        a.scrollback_used()
    );
}

/// A request beyond the memory bound is reported as what it is. libghostty
/// takes the budget at construction and exposes no setter, so this cannot be
/// fixed by asking for more later — it can only be told truthfully.
#[test]
fn a_request_beyond_the_memory_bound_reports_what_fits() {
    let a = TerminalActor::new(24, 80, 10_000_000);
    assert_eq!(a.scrollback_request(), 10_000_000);
    assert_eq!(a.scrollback_bytes(), MAX_SCROLLBACK_BYTES);
    assert!(
        a.scrollback() < 10_000_000,
        "the request cannot be met and must not be reported as met"
    );
    assert!(
        a.scrollback() > DEFAULT_SCROLLBACK,
        "but the bound still buys more than the default: {}",
        a.scrollback()
    );
    assert_eq!(a.scrollback_capacity(), 24 + a.scrollback());
}

/// Widening the terminal makes each row cost more out of a budget fixed at
/// construction, so the retainable line count drops. The reported number
/// follows it instead of repeating the original promise.
#[test]
fn a_widened_terminal_reports_the_history_it_can_still_hold() {
    let mut a = TerminalActor::new(24, 80, DEFAULT_SCROLLBACK);
    assert_eq!(a.scrollback(), DEFAULT_SCROLLBACK);
    let budget = a.scrollback_bytes();

    a.resize(400, 24);
    assert_eq!(a.scrollback_bytes(), budget, "the budget does not move");
    assert!(
        a.scrollback() < DEFAULT_SCROLLBACK,
        "a five-times wider row cannot hold the same line count: {}",
        a.scrollback()
    );
    assert_eq!(a.scrollback_capacity(), 24 + a.scrollback());
    assert_eq!(a.scrollback_request(), DEFAULT_SCROLLBACK);
}

fn page(a: &mut TerminalActor, before: Option<String>) -> (Vec<String>, Option<String>) {
    use pty_core::protocol::{HistoryRequest, HistoryResponse};
    let HistoryResponse::Page { rows, next_before, .. } =
        a.history("owner-generation", &HistoryRequest { expected_generation: "owner-generation".into(), limit: 2, before })
    else { panic!("expected history page") };
    let text = rows.into_iter().map(|row| {
        let mut replay = TerminalActor::new(2, 80, 0);
        replay.write(row.ansi.as_bytes());
        replay.plain(Range::Viewport)
    }).collect();
    (text, next_before)
}

#[test]
fn history_pages_remain_anchored_across_append_and_report_retention_loss() {
    use pty_core::protocol::{HistoryRequest, HistoryResponse};
    let mut actor = TerminalActor::new(3, 80, 100);
    fill(&mut actor, 12);
    let (first, cursor) = page(&mut actor, None);
    assert_eq!(first, ["L8", "L9"]);
    actor.write(b"L12\r\nL13\r\n");
    let (older, _) = page(&mut actor, cursor.clone());
    assert_eq!(older, ["L6", "L7"]);
    fill(&mut actor, 5_000);
    assert!(matches!(
        actor.history("owner-generation", &HistoryRequest { expected_generation: "owner-generation".into(), limit: 2, before: cursor }),
        HistoryResponse::CursorGap
    ));
}

#[test]
fn history_rejects_reset_geometry_generation_and_alternate_without_consuming_cursor() {
    use pty_core::protocol::{HistoryRequest, HistoryResponse};
    let mut actor = TerminalActor::new(3, 80, 100);
    fill(&mut actor, 12);
    let (_, cursor) = page(&mut actor, None);
    let request = HistoryRequest { expected_generation: "owner-generation".into(), limit: 2, before: cursor };
    assert!(matches!(actor.history("different-owner", &request), HistoryResponse::StaleGeneration));
    actor.write(b"\x1b[?1049hALT\r\nALT\r\nALT");
    assert!(matches!(actor.history("owner-generation", &request), HistoryResponse::AlternateScreen));
    actor.write(b"\x1b[?1049l");
    assert_eq!(page(&mut actor, request.before.clone()).0, ["L6", "L7"]);
    actor.resize(40, 3);
    assert!(matches!(actor.history("owner-generation", &request), HistoryResponse::CursorGap));
    fill(&mut actor, 12);
    let (_, cursor) = page(&mut actor, None);
    actor.write(b"\x1b[3J");
    assert!(matches!(actor.history("owner-generation", &HistoryRequest { expected_generation: "owner-generation".into(), limit: 2, before: cursor }), HistoryResponse::CursorGap));
    fill(&mut actor, 12);
    let (_, cursor) = page(&mut actor, None);
    actor.write(b"\x1bc");
    assert!(matches!(actor.history("owner-generation", &HistoryRequest { expected_generation: "owner-generation".into(), limit: 2, before: cursor }), HistoryResponse::CursorGap));
}
