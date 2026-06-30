/**
 * Shared helpers for working with session tag filters.
 *
 * These are used by the pty CLI (`pty list --filter-tag`, `pty --filter-tag`)
 * and are exposed on `@myobie/pty/client` so tools like pty-relay can accept
 * and apply the same filter syntax.
 */

/**
 * Extract all `--filter-tag key=value` pairs from `args` (repeatable).
 *
 * Mutates `args`: consumed `--filter-tag` and its value are removed in place.
 * Returns the collected tags as an object.
 *
 * Throws if `--filter-tag` appears without a following `key=value` token.
 */
export function extractFilterTags(args: string[]): Record<string, string> {
  const tags: Record<string, string> = {};
  for (let i = 0; i < args.length; i++) {
    if (args[i] !== "--filter-tag") continue;
    const kv = args[i + 1];
    if (!kv || !kv.includes("=")) {
      throw new Error(`--filter-tag expects "key=value"`);
    }
    const eq = kv.indexOf("=");
    tags[kv.slice(0, eq)] = kv.slice(eq + 1);
    args.splice(i, 2);
    i--;
  }
  return tags;
}

/**
 * Returns `true` if `sessionTags` contains every key/value pair in `filterTags`
 * (AND match). An empty `filterTags` always matches. A session with no tags
 * only matches an empty `filterTags`.
 */
export function matchesAllTags(
  sessionTags: Record<string, string> | undefined,
  filterTags: Record<string, string>,
): boolean {
  for (const [k, v] of Object.entries(filterTags)) {
    if (sessionTags?.[k] !== v) return false;
  }
  return true;
}

/**
 * Keys that pty itself treats as internal bookkeeping. `strategy` drives
 * the `[permanent]` marker and tells `pty gc` to respawn the session on
 * exit; `ptyfile*` keys wire up the toml-managed-session plumbing. The
 * user-facing `parent=<name>` orphan-kill tag is intentionally NOT
 * reserved — it's a regular tag visible in `pty list`. Reserved keys
 * are visible in `pty list --tags` but hidden from the default listing.
 */
const EXACT_RESERVED = new Set([
  "ptyfile",
  "ptyfile.session",
  "ptyfile.tags",
  "strategy",
]);

/**
 * Returns `true` if the tag key is "reserved" — either one of pty's
 * internal bookkeeping keys (see above) or any key starting with `:`.
 *
 * The `:` prefix is a convention for **tool-owned tags** (e.g.,
 * pty-layout stamps `:l<pid>-<rand>` keys on sessions it owns a view
 * of). Consumers should hide reserved keys from user-facing listings
 * by default but still allow writes — tools need to set and unset them.
 *
 * Exposed on `@myobie/pty/client` so downstream tools (pty-relay,
 * pty-layout) can use the same rule without duplicating deny-lists.
 */
export function isReservedTagKey(key: string): boolean {
  if (EXACT_RESERVED.has(key)) return true;
  if (key.startsWith(":")) return true;
  return false;
}
