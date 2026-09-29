//! Effective geometry: the per-axis minimum over the writable clients that
//! have negotiated, broadcast as GEOMETRY before the PTY is resized so it
//! precedes any output drawn for the new size.
//!
//! node: src/server.ts:1158-1202

use std::time::Instant;

use portable_pty::PtySize;
use pty_core::protocol::encode_geometry;

use super::lifecycle::Daemon;

impl Daemon {
    /// Resize to the smallest rows and cols across the writable clients.
    /// `true` when the size changed. Zero writers → nothing changes.
    ///
    /// node: src/server.ts:1158-1181
    pub(crate) fn negotiate_size(&mut self) -> bool {
        let mut rows = 0u16;
        let mut cols = 0u16;
        for c in self.clients.values().filter(|c| c.constrains_size()) {
            rows = if rows == 0 { c.rows } else { rows.min(c.rows) };
            cols = if cols == 0 { c.cols } else { cols.min(c.cols) };
        }
        if rows == 0 || cols == 0 {
            return false;
        }
        // A corrupted or hostile client can request the full u16 range.
        // Resizing the terminal actor to that grid blocks this daemon's
        // serving thread, including STATUS for every other client.
        if rows > 4096 || cols > 4096 || u32::from(rows) * u32::from(cols) > 250_000 {
            return false;
        }
        if rows == self.actor.rows() && cols == self.actor.cols() {
            return false;
        }
        self.actor.resize(cols, rows);
        self.broadcast_geometry(rows, cols);
        self.resize_pty(rows, cols);
        // Mode 2048 can produce a pty reply during resize, even if the
        // child never prints again. Send it after the kernel size changes.
        let replies = self.actor.take_pty_replies();
        self.write_pty(&replies);
        self.last_resize = Some(Instant::now());
        true
    }

    /// GEOMETRY to every attached or readonly socket, whatever its phase.
    ///
    /// node: src/server.ts:1183-1190
    pub(crate) fn broadcast_geometry(&self, rows: u16, cols: u16) {
        let packet = encode_geometry(rows, cols);
        for c in self.clients.values().filter(|c| c.gets_geometry()) {
            c.send(packet.clone());
        }
    }

    pub(crate) fn resize_pty(&self, rows: u16, cols: u16) {
        let cell = self.actor.cell_size();
        let pixels = |cells: u16, extent: u32| {
            ((cells as u32).saturating_mul(extent)).min(u16::MAX as u32) as u16
        };
        let _ = self.session.resize(PtySize {
            rows,
            cols,
            pixel_width: pixels(cols, cell.width),
            pixel_height: pixels(rows, cell.height),
        });
    }

    /// One column narrower and back: two SIGWINCHes that make the child
    /// redraw completely after a replayed SCREEN.
    ///
    /// node: src/server.ts:1195-1202
    pub(crate) fn nudge_redraw(&mut self) {
        let cols = self.actor.cols();
        let rows = self.actor.rows();
        if cols < 2 {
            return;
        }
        self.resize_pty(rows, cols - 1);
        self.actor.resize(cols - 1, rows);
        self.resize_pty(rows, cols);
        self.actor.resize(cols, rows);
    }
}
