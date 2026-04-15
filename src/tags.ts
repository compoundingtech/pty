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
