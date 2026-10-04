//! Retained primary history reads with owner-local, tracked backwards boundaries.
use std::collections::BTreeMap;
use std::io::Write;
use std::time::{Duration, Instant};

use libghostty_vt::cell::CellWide;
use libghostty_vt::error::Error;
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
            let Ok(first) = term.grid_ref(point(0)) else {
                return HistoryResponse::Unavailable;
            };
            let Ok(wrapped) = first.row().and_then(|row| row.is_wrap_continuation()) else {
                return HistoryResponse::Unavailable;
            };
            let ansi = match row_ansi(term, y as u32, columns) {
                Ok(ansi) => ansi,
                Err(error) => return error,
            };
            bytes += ansi.len();
            if bytes > MAX_PAGE_BYTES {
                return HistoryResponse::TooLarge;
            }
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

/// Ghostty's VT formatter omits cell hyperlinks (its hyperlink extra is only the
/// cursor's current link). Format disjoint cell-link spans, preserving each span's
/// original styles/text through the formatter and emitting OSC 8 ourselves.
fn row_ansi(term: &Terminal<'_, '_>, y: u32, columns: u16) -> Result<String, HistoryResponse> {
    let mut out = Vec::new();
    let mut current_uri = Vec::new();
    let mut next_uri = Vec::new();
    let mut current_len = 0;
    let mut start = 0;
    let mut column = 0;
    while column < columns {
        let cell = term.grid_ref(Point::History(PointCoordinate { x: column, y }))
            .map_err(|_| HistoryResponse::Unavailable)?;
        let wide = cell.cell().and_then(|cell| cell.wide())
            .map_err(|_| HistoryResponse::Unavailable)?;
        if matches!(wide, CellWide::SpacerHead | CellWide::SpacerTail) {
            column += 1;
            continue;
        }
        let length = match cell.hyperlink_uri(&mut next_uri) {
            Ok(length) => length,
            Err(Error::OutOfSpace { required }) if required <= MAX_ROW_BYTES => {
                next_uri.resize(required, 0);
                cell.hyperlink_uri(&mut next_uri).map_err(|_| HistoryResponse::Unavailable)?
            }
            Err(Error::OutOfSpace { .. }) => return Err(HistoryResponse::TooLarge),
            Err(_) => return Err(HistoryResponse::Unavailable),
        };
        if next_uri[..length].iter().any(|byte| *byte < 0x20 || *byte == 0x7f)
            || std::str::from_utf8(&next_uri[..length]).is_err()
        {
            return Err(HistoryResponse::Unavailable);
        }
        if current_uri[..current_len] != next_uri[..length] {
            if start < column {
                append_span(term, y, start, column - 1, &current_uri[..current_len], &mut out)?;
            }
            std::mem::swap(&mut current_uri, &mut next_uri);
            current_len = length;
            start = column;
        }
        column = column.saturating_add(if wide == CellWide::Wide { 2 } else { 1 });
    }
    append_span(term, y, start, columns.saturating_sub(1), &current_uri[..current_len], &mut out)?;
    let close = b"\x1b]8;;\x1b\\\x1b[0m";
    if out.len() + close.len() > MAX_ROW_BYTES {
        return Err(HistoryResponse::TooLarge);
    }
    out.extend_from_slice(close);
    String::from_utf8(out).map_err(|_| HistoryResponse::Unavailable)
}

fn append_span(
    term: &Terminal<'_, '_>,
    y: u32,
    start: u16,
    end: u16,
    uri: &[u8],
    out: &mut Vec<u8>,
) -> Result<(), HistoryResponse> {
    let first = term.grid_ref(Point::History(PointCoordinate { x: start, y }))
        .map_err(|_| HistoryResponse::Unavailable)?;
    let last = term.grid_ref(Point::History(PointCoordinate { x: end, y }))
        .map_err(|_| HistoryResponse::Unavailable)?;
    let selection = Selection::new(first, last, false);
    let options = FormatterOptions::new()
        .with_format(Format::Vt)
        .with_unwrap(false)
        .with_trim(false)
        .with_cursor(false)
        .with_modes(false)
        .with_kitty_keyboard(false)
        .with_selection(&selection);
    let mut formatter = Formatter::new(term, options).map_err(|_| HistoryResponse::Unavailable)?;
    let length = formatter.format_len().map_err(|_| HistoryResponse::Unavailable)?;
    write!(out, "\x1b[1;{}H\x1b[0m\x1b]8;;", start + 1)
        .map_err(|_| HistoryResponse::Unavailable)?;
    if out.len() + uri.len() + 2 + length > MAX_ROW_BYTES {
        return Err(HistoryResponse::TooLarge);
    }
    out.extend_from_slice(uri);
    out.extend_from_slice(b"\x1b\\");
    let offset = out.len();
    out.resize(offset + length, 0);
    let written = formatter.format_buf(&mut out[offset..]).map_err(|_| HistoryResponse::Unavailable)?;
    out.truncate(offset + written);
    if length == 0 {
        let backgrounds = crate::serialize::history_background(term, y, start, end);
        if out.len() + backgrounds.len() > MAX_ROW_BYTES {
            return Err(HistoryResponse::TooLarge);
        }
        out.extend_from_slice(backgrounds.as_bytes());
    }
    Ok(())
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
