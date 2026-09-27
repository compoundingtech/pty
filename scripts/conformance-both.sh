#!/usr/bin/env bash
# Run the conformance suite against two pty binaries and print a side-by-side
# summary: one row per test file with pass/fail counts for each binary, then
# the tests whose outcome differs (the parity debt).
#
# Usage: scripts/conformance-both.sh [--node <bin>] [--rust <bin>] [--out <dir>] [test-file ...]
#   --node   the reference binary (default: `pty` on PATH)
#   --rust   the binary under port (default: target/debug/pty after `cargo build -p pty`)
#   --out    where per-file logs and `red.txt` go (default: target/conformance)
#   test-file  names like `integration_sync` (default: every tests/*.rs)
#
# Each side's raw log lives in <out>/<label>/<file>.log; <out>/red.txt lists the
# tests that pass on the first binary and fail on the second, with the first
# line of each failure message.
set -u
cd "$(dirname "$0")/.."

NODE_BIN="$(command -v pty || true)"
RUST_BIN="$PWD/target/debug/pty"
OUT="$PWD/target/conformance"
FILES=()
while [ $# -gt 0 ]; do
  case "$1" in
    --node) NODE_BIN="$2"; shift 2 ;;
    --rust) RUST_BIN="$2"; shift 2 ;;
    --out) OUT="$2"; shift 2 ;;
    -h|--help) sed -n '2,15p' "$0"; exit 0 ;;
    *) FILES+=("$1"); shift ;;
  esac
done
[ -x "$NODE_BIN" ] || { echo "node pty not found (use --node)"; exit 2; }
[ -x "$RUST_BIN" ] || { echo "rust pty not found at $RUST_BIN (cargo build -p pty, or --rust)"; exit 2; }

# Check that the reference binary is the Node tool, at the commit we pinned.
#
# The default reference is whatever `pty` is on PATH, and on a machine that has
# the port installed that is a RUST build. Nothing here noticed: the run
# compared Rust against Rust, labelled one column "node", printed a full row of
# passes and wrote an empty red.txt. A run that measures nothing looks exactly
# like a run that found perfect parity, and it is the more encouraging of the
# two, so it is the one that gets believed.
#
# The two binaries name themselves apart. The port carries a `-rust` tag by
# decision (`0.13.x-rust+<short-sha>`, docs/parity.md section 14); the Node tool
# does not (`0.12.0+86dcc5e`).
NODE_VERSION="$("$NODE_BIN" --version 2>/dev/null || true)"
RUST_VERSION="$("$RUST_BIN" --version 2>/dev/null || true)"
[ -n "$NODE_VERSION" ] || NODE_VERSION="unknown"
[ -n "$RUST_VERSION" ] || RUST_VERSION="unknown"

case "$NODE_VERSION" in
  *-rust*)
    echo "refusing: the reference binary is a build of this port, not the Node pty."
    echo "  --node $NODE_BIN"
    echo "  reports $NODE_VERSION"
    echo "Comparing the port against itself reports perfect parity and measures nothing."
    echo "Build the Node tool at the pinned commit and point --node at it:"
    echo "  ref=\$(cat crates/pty-conformance/node-ref)"
    echo "  git clone --filter=blob:none https://github.com/compoundingtech/pty-original-experiment /some/disk/path/node-pty"
    echo "  git -C /some/disk/path/node-pty checkout --detach \$ref"
    echo "  (cd /some/disk/path/node-pty && npm ci && npm run build)"
    echo "  scripts/conformance-both.sh --node /some/disk/path/node-pty/bin/pty"
    exit 2 ;;
esac

# Off the pinned commit is a warning and not a refusal: comparing against
# another Node commit is a thing somebody may mean to do. Say it loudly, because
# a reference seven commits stale invented eight divergences on 2026-09-05 and
# every one was an artifact of the reference rather than a fact about the port.
NODE_REF_SHORT="$(cut -c1-7 crates/pty-conformance/node-ref 2>/dev/null || true)"
case "$NODE_VERSION" in
  *"$NODE_REF_SHORT"*) ;;
  *)
    echo "WARNING: the reference binary is not at the pinned commit."
    echo "  --node $NODE_BIN reports $NODE_VERSION"
    echo "  crates/pty-conformance/node-ref pins $NODE_REF_SHORT"
    echo "  Differences found in this run may belong to the reference, not to the port." ;;
esac
if [ ${#FILES[@]} -eq 0 ]; then
  for f in crates/pty-conformance/tests/*.rs; do
    FILES+=("$(basename "$f" .rs)")
  done
fi

cargo test -p pty-conformance --no-run -q 2>&1 | grep -v '^\s*$' | grep -iv 'warning' || true

# One `cargo test` per file, on purpose.
#
# The obvious optimisation is to pass every `--test` to a single invocation.
# It saves almost nothing and it can silently break the comparison.
#
# It saves almost nothing: measured 2026-09-05, five files cost 10.48 s as five
# invocations and 10.27 s as one, and the tests inside account for 10.21 s of
# that. Cargo overhead is about 0.05 s per file. The suite is slow because it
# drives real processes through real PTYs and waits for them, not because of
# how it is invoked.
#
# It can break the comparison: `cargo test` STOPS at the first failing target.
# A single invocation that hits a failure never runs the remaining files, and
# the failing files are exactly the ones this script exists to look at — a run
# that stopped early reports fewer differences, which reads as agreement. Any
# collapse of this loop needs `--no-fail-fast`, and even then it costs the
# per-file logs the summary table is built from.
run_side() {
  local label="$1" bin="$2"
  mkdir -p "$OUT/$label"
  for f in "${FILES[@]}"; do
    PTY_TEST_BIN="$bin" cargo test -p pty-conformance --test "$f" -- --test-threads=4 \
      > "$OUT/$label/$f.log" 2>&1 || true
  done
}

echo "node: $NODE_BIN ($NODE_VERSION)"
echo "rust: $RUST_BIN ($RUST_VERSION)"
echo "logs: $OUT"
run_side node "$NODE_BIN"
run_side rust "$RUST_BIN"

python3 - "$OUT" "${FILES[@]}" <<'PY'
import re, sys, os
out = sys.argv[1]
files = sys.argv[2:]
res = {}
def parse(path):
    tests = {}
    fails = {}
    if not os.path.exists(path):
        return tests, fails
    text = open(path, errors="replace").read()
    for m in re.finditer(r"^test (\S+) \.\.\. (ok|FAILED|ignored)", text, re.M):
        tests[m.group(1)] = m.group(2)
    # First line of each failure's stdout section (the panic message).
    for m in re.finditer(r"^---- (\S+) stdout ----\n(.*?)(?=^---- |\nfailures:|\Z)", text, re.M | re.S):
        body = m.group(2)
        # The panic location line is followed by the message; keep the
        # message's first line (or the location when there is none).
        first = ""
        where = ""
        for line in body.splitlines():
            line = line.strip()
            if not line or line.startswith("note: run with"):
                continue
            if re.match(r"thread '.*' (\(\d+\) )?panicked at ", line):
                where = line
                tail = line.split("panicked at ", 1)[1]
                rest = tail.split(":", 3)[3].strip() if tail.count(":") >= 3 else ""
                if rest:
                    first = rest
                    break
                continue
            first = line
            break
        msg = (first or where)[:200]
        # The rig's Out summary follows; the first stderr line usually names
        # the cause (an unknown command, a different message).
        if "--- stderr ---" in body:
            err = body.split("--- stderr ---", 1)[1]
            for line in err.splitlines():
                line = line.strip()
                if line and not line.startswith("note: run with"):
                    msg += " :: " + line[:160]
                    break
        fails[m.group(1)] = msg
    return tests, fails
rows = []
red = []
for f in files:
    nt, nf = parse(f"{out}/node/{f}.log")
    rt, rf = parse(f"{out}/rust/{f}.log")
    def counts(t):
        return sum(1 for v in t.values() if v == "ok"), sum(1 for v in t.values() if v == "FAILED")
    np_, nfail = counts(nt)
    rp, rfail = counts(rt)
    rows.append((f, np_, nfail, rp, rfail))
    for name, status in sorted(nt.items()):
        if status == "ok" and rt.get(name) == "FAILED":
            red.append(f"{f}::{name} -- {rf.get(name, '')}")
        if status == "FAILED":
            red.append(f"{f}::{name} -- NODE RED: {nf.get(name, '')}")
w = max(len(r[0]) for r in rows) if rows else 10
print()
print(f"{'file':<{w}}  {'node pass/fail':>14}  {'rust pass/fail':>14}")
print(f"{'-'*w}  {'-'*14}  {'-'*14}")
tn = [0, 0, 0, 0]
for f, a, b, c, d in rows:
    flag = "" if b == 0 and d == 0 else ("  <- rust" if b == 0 else "  <- NODE")
    print(f"{f:<{w}}  {a:>7}/{b:<6}  {c:>7}/{d:<6}{flag}")
    tn[0] += a; tn[1] += b; tn[2] += c; tn[3] += d
print(f"{'-'*w}  {'-'*14}  {'-'*14}")
print(f"{'total':<{w}}  {tn[0]:>7}/{tn[1]:<6}  {tn[2]:>7}/{tn[3]:<6}")
with open(f"{out}/red.txt", "w") as fh:
    fh.write("\n".join(red) + ("\n" if red else ""))
print(f"\n{len(red)} tests differ; see {out}/red.txt")
for line in red[:200]:
    print("  " + line)
PY
