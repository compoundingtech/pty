# TODO

- Show connection count per session in `pty list` and the web UI session list. Requires adding a protocol message (e.g., INFO) so the CLI/web can query the daemon for its current client count.
- Allow killing stale connections. With the web frontend, browser tabs may disconnect uncleanly and leave stale connections. Need a way to list and kill individual connections per session.
- Upgrade to xterm.js 6.1.0 when released — fixes native touch scrolling (currently worked around with a manual touchmove handler).
