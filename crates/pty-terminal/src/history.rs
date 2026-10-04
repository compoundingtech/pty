//! Retained primary history reads with owner-local, tracked backwards boundaries.
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use libghostty_vt::fmt::{Format, Formatter, FormatterOptions};
use libghostty_vt::screen::TrackedGridRef;
use libghostty_vt::selection::Selection;
use libghostty_vt::terminal::{Point, PointCoordinate, PointSpace, Terminal};
use pty_core::protocol::{HistoryRequest, HistoryResponse, HistoryRow};

const CURSOR_LEASE: Duration = Duration::from_secs(300);
const MAX_CURSORS: usize = 256;
const MAX_ROW_BYTES: usize = 65_536;
const MAX_PAGE_BYTES: usize = 2 * 1024 * 1024;

struct Boundary {
    anchor: TrackedGridRef,
    expires: Instant,
}

#[derive(Default)]
pub(crate) struct History {
    boundaries: BTreeMap<String, Boundary>,
    serial: u64,
}

impl History {
    pub(crate) fn invalidate(&mut self) {
        self.boundaries.clear();
    }

    pub(crate) fn page(
        &mut self,
        term: &Terminal<'_, '_>,
        alternate: bool,
        generation: &str,
        request: &HistoryRequest,
    ) -> HistoryResponse {
        if request.expected_generation != generation {
            return HistoryResponse::StaleGeneration;
        }
        if request.limit == 0 || request.limit > 200
            || request.before.as_ref().is_some_and(|cursor| cursor.len() > 128)
        {
            return HistoryResponse::InvalidRequest;
        }
        if alternate {
            return HistoryResponse::AlternateScreen;
        }
        let now = Instant::now();
        self.boundaries.retain(|_, boundary| boundary.expires > now && boundary.anchor.has_value());
        let Ok(retained_rows) = term.scrollback_rows() else {
            return HistoryResponse::Unavailable;
        };
        let Ok(columns) = term.cols() else {
            return HistoryResponse::Unavailable;
        };
        let end = match &request.before {
            None => retained_rows,
            Some(cursor) => {
                if cursor.split_once(':').map(|(owner, _)| owner) != Some(generation) {
                    return HistoryResponse::CursorGap;
                }
                let Some(boundary) = self.boundaries.get(cursor) else {
                    return HistoryResponse::CursorGap;
                };
                match boundary.anchor.point(PointSpace::History) {
                    Ok(Some(point)) if point.x == 0 => point.y as usize,
                    _ => return HistoryResponse::CursorGap,
                }
            }
        };
        let start = end.saturating_sub(usize::from(request.limit));
        let mut rows = Vec::with_capacity(end - start);
        let mut bytes = 0;
        for y in start..end {
            let point = |x| Point::History(PointCoordinate { x, y: y as u32 });
            let (Ok(first), Ok(last)) = (term.grid_ref(point(0)), term.grid_ref(point(columns.saturating_sub(1)))) else {
                return HistoryResponse::Unavailable;
            };
            let Ok(wrapped) = first.row().and_then(|row| row.is_wrap_continuation()) else {
                return HistoryResponse::Unavailable;
            };
            let selection = Selection::new(first, last, false);
            let options = FormatterOptions::new()
                .with_format(Format::Vt)
                .with_unwrap(false)
                .with_trim(false)
                .with_cursor(false)
                .with_modes(false)
                .with_kitty_keyboard(false)
                .with_selection(&selection);
            let Ok(mut formatter) = Formatter::new(term, options) else {
                return HistoryResponse::Unavailable;
            };
            let Ok(length) = formatter.format_len() else {
                return HistoryResponse::Unavailable;
            };
            bytes += length;
            if length > MAX_ROW_BYTES || bytes > MAX_PAGE_BYTES {
                return HistoryResponse::TooLarge;
            }
            let mut buffer = vec![0; length];
            let Ok(written) = formatter.format_buf(&mut buffer) else {
                return HistoryResponse::Unavailable;
            };
            buffer.truncate(written);
            let Ok(mut ansi) = String::from_utf8(buffer) else {
                return HistoryResponse::Unavailable;
            };
            let backgrounds = crate::serialize::history_background(term, y as u32);
            bytes += backgrounds.len();
            if ansi.len() + backgrounds.len() > MAX_ROW_BYTES || bytes > MAX_PAGE_BYTES {
                return HistoryResponse::TooLarge;
            }
            ansi.push_str(&backgrounds);
            rows.push(HistoryRow { ansi, wrapped });
        }
        let next_before = if start == 0 {
            None
        } else {
            let Ok(anchor) = term.track_grid_ref(Point::History(PointCoordinate { x: 0, y: start as u32 })) else {
                return HistoryResponse::Unavailable;
            };
            self.serial += 1;
            let token = format!("{generation}:{:016x}", self.serial);
            if self.boundaries.len() == MAX_CURSORS {
                let oldest = self.boundaries.keys().next().cloned().expect("nonempty cursor table");
                self.boundaries.remove(&oldest);
            }
            self.boundaries.insert(token.clone(), Boundary { anchor, expires: now + CURSOR_LEASE });
            Some(token)
        };
        HistoryResponse::Page { columns, retained_rows, rows, next_before }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TerminalActor;

    #[test]
    fn expired_and_capacity_evicted_cursors_are_gaps_not_offsets() {
        let mut actor = TerminalActor::new(3, 80, 100);
        for row in 0..12 {
            actor.write(format!("L{row}\r\n").as_bytes());
        }
        let mut history = History::default();
        let request = HistoryRequest { expected_generation: "owner".into(), limit: 2, before: None };
        let HistoryResponse::Page { next_before: Some(first), .. } =
            history.page(actor.terminal(), false, "owner", &request)
        else { panic!("expected cursor") };
        history.boundaries.get_mut(&first).unwrap().expires = Instant::now() - Duration::from_secs(1);
        assert!(matches!(
            history.page(actor.terminal(), false, "owner", &HistoryRequest { expected_generation: "owner".into(), limit: 2, before: Some(first) }),
            HistoryResponse::CursorGap
        ));
        let HistoryResponse::Page { next_before: Some(first), .. } =
            history.page(actor.terminal(), false, "owner", &request)
        else { panic!("expected cursor") };
        for _ in 0..MAX_CURSORS {
            history.page(actor.terminal(), false, "owner", &request);
        }
        assert!(matches!(
            history.page(actor.terminal(), false, "owner", &HistoryRequest { expected_generation: "owner".into(), limit: 2, before: Some(first) }),
            HistoryResponse::CursorGap
        ));
    }
}
