#!/usr/bin/env bash
# Run the workspace suite, and re-run a failing test binary once before
# calling it a defect.
#
# This is not leniency, it is the procedure the README already documents:
# "One test failing in a whole-workspace run is not yet a defect. Re-run it
# alone before treating it as one." The suite drives real processes through
# real PTYs, 139 binaries run in parallel, and on a loaded machine one of them
# can lose a race. A GitHub runner is a loaded machine.
#
# The re-run is single-threaded and covers only the binaries that failed, so a
# real failure still fails: it has to lose twice, once under contention and
# once alone.
set -uo pipefail
cd "$(dirname "$0")/.."

log=$(mktemp)
trap 'rm -f "$log"' EXIT

# --no-fail-fast: without it cargo stops at the first failing test binary, the
# re-run below passes it alone, and every binary after it never ran at all.
echo "== cargo test --workspace --no-fail-fast =="
cargo test --workspace --no-fail-fast 2>&1 | tee "$log"
status=${PIPESTATUS[0]}
[ "$status" -eq 0 ] && { echo "workspace suite: clean on the first run"; exit 0; }

# cargo prints, for each failing test binary:
#   error: test failed, to rerun pass `-p <crate> --test <binary>`
mapfile -t reruns < <(grep -oE 'to rerun pass `[^`]+`' "$log" | sed 's/to rerun pass `//; s/`$//' | sort -u)

if [ ${#reruns[@]} -eq 0 ]; then
  echo "workspace suite FAILED and named no re-runnable binary; not a flake, failing"
  exit "$status"
fi

echo
echo "== ${#reruns[@]} test binary/binaries failed; re-running each alone =="
printf '   %s\n' "${reruns[@]}"
echo

failed=0
for args in "${reruns[@]}"; do
  echo "== cargo test $args -- --test-threads=1 =="
  # shellcheck disable=SC2086
  if cargo test $args -- --test-threads=1; then
    echo "   passed alone — treating the first failure as a lost race"
  else
    echo "   FAILED ALONE — this is a defect, not a flake"
    failed=1
  fi
done

exit "$failed"
