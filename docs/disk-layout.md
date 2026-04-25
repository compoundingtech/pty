# pty on-disk layout

> **Pre-1.0.** Format may change in any release; pin to a pty version if you depend on these files. Breaking changes appear under `### Storage format` in the CHANGELOG.

For non-Node tools that want to read pty's state without paying Node startup. The CLI is the canonical writer; the files below are the canonical readable surface.

## Directory

`$PTY_SESSION_DIR` (default `~/.local/state/pty`, mode `0700`, single-user). Every CLI command honors the env var.

| file | purpose | tier |
|---|---|---|
| `<name>.json` | session metadata | 1 |
| `<name>.events.jsonl` | append-only event log | 1 |
| `<name>.sock` | daemon IPC socket (Unix) | 2 |
| `<name>.pid` | daemon pid (decimal) | 2 |
| `<name>.lock` | creation-race lock | 2 |
| `theme` | last-selected TUI theme | 2 |
| `supervisor/state.json` | supervisor state | 2 (internal) |
| `<name>.json.tmp.<pid>.<rand>` | atomic-write tmp — readers MUST ignore | n/a |
| `<name>.events.jsonl.tmp.<pid>.<rand>` | same | n/a |

**Tier 1**: we'll try not to break these; changes called out in CHANGELOG.
**Tier 2**: pty-internal; may move freely.

## Atomic write contract

pty writes to `<target>.tmp.<pid>.<rand>` then `rename()`s into place. POSIX same-filesystem rename is atomic — readers see the old version or the new one, never partial. When scanning the directory, filter `*.tmp.*`.

## `<name>.json` (tier 1)

Pretty-printed JSON. Source of truth: `SessionMetadata` in `src/sessions.ts`.

```ts
{
  command: string;            // resolved binary path
  args: string[];
  displayCommand: string;     // command as the user typed it
  cwd: string;
  createdAt: string;          // ISO 8601
  exitCode?: number;          // present after clean exit
  exitedAt?: string;
  lastLines?: string[];       // snapshotted at exit
  tags?: { [k: string]: string };
  state?: { [k: string]: unknown };
  displayName?: string;
}
```

- Status (`running` / `exited` / `vanished`) is *derived* from socket + pid, not stored.
- Reserved tag keys (`ptyfile*`, `strategy`, `supervisor.status`, anything starting with `:`) are pty/tool-internal; hidden from `pty list` unless `--tags`.
- Concurrent writers: last-write-wins; readers never see torn files. Cross-process writers can lose updates to the read-modify-write window.

## `<name>.events.jsonl` (tier 1)

Append-only JSONL, one event per line. Auto-truncates from 1000 → 500 lines via atomic rewrite (the inode changes; tailing readers should re-open).

Envelope: `{ session: string; type: string; ts: string; ...payload }`. Event types (source: `src/events.ts`):

| type | payload |
|---|---|
| `bell` | — |
| `title_change` | `value: string` |
| `notification` | `title?, body?, source?: "osc9" \| "osc99" \| "osc777"` |
| `focus_request` | — |
| `cursor_visible` | — |
| `session_start` | `tags?` |
| `session_exit` | `exitCode` |
| `session_exec` | `previousCommand, command` |
| `session_restart` | `restartCount, backoffMs` |
| `session_failed` | `restartCount, reason` |
| `supervisor_start` | — |
| `supervisor_stop` | — |
| `display_name_change` | `previous: string\|null, value: string\|null` |
| `tags_change` | `previous, value` (full snapshots) |
| `state.set` | `key, value` |
| `state.delete` | `key` |
| `user.<name>` | `data?, text?` — free-form, via `pty emit` |

A single line ≤ `PIPE_BUF` (~4 KB) is atomic per POSIX `O_APPEND`. Built-ins are well under. Keep large `user.*` / `state.set` payloads in `state` (atomic-rename), not events.

## Reading from outside pty

```sh
jq -r '.tags["role"] // empty' "$PTY_SESSION_DIR/myserver.json"
```

For live updates, tail `<name>.events.jsonl` via `inotify` / `kqueue`. Subscribe instead of polling — `state.set` / `tags_change` / `display_name_change` fire on every mutation.
