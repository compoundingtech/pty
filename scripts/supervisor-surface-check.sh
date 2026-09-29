#!/usr/bin/env bash
# Drive one pty binary the way a process supervisor drives it, and say whether
# every call worked.
#
# A supervisor starts long-lived sessions, watches them, types into them,
# reads what they printed, and takes them away again. That is a small and very
# specific slice of this tool, and it is the slice that has to keep working
# across an implementation change. This script runs that slice end to end,
# with the flags a supervisor really passes, against whichever binary you name.
#
# The flags here are taken from st2 (github.com/compoundingtech/st2), which
# runs its agents in pty sessions: `src/run.rs` for run, list, metadata patch,
# kill and rm; `src/ding/mod.rs` for send and peek; `src/eval_run.rs` for the
# full plain peek.
#
# Run it against the Node tool first. It passes there, so a failure against
# another binary is a real difference and not a fault in this script.
#
#   scripts/supervisor-surface-check.sh $(which pty)
#   scripts/supervisor-surface-check.sh target/release/pty
#
# Exits 0 when everything passed.

set -u
BIN="$1"
ROOT=$(mktemp -d /tmp/st2surface.XXXXXX)
WORK=$(mktemp -d /tmp/st2work.XXXXXX)
export PTY_ROOT="$ROOT"
PASS=0; FAIL=0
ID="example.demo.agent"

say() { printf '%-58s %s\n' "$1" "$2"; }
ok()   { PASS=$((PASS+1)); say "$1" "ok"; }
bad()  { FAIL=$((FAIL+1)); say "$1" "FAIL: $2"; }
check() { if [ "$2" = 0 ]; then ok "$1"; else bad "$1" "$3"; fi; }

echo "binary: $("$BIN" --version 2>&1 | head -1)"
echo "root:   $ROOT"
echo

# 1. pty --help  (ding probes the tool this way)
out=$("$BIN" --help 2>&1); rc=$?
[ $rc -eq 0 ] && echo "$out" | grep -q "run" && ok "pty --help" || bad "pty --help" "rc=$rc"

# 2. pty run, st2's exact agent-task flags
out=$("$BIN" run -d --force --id "$ID" --name "example.demo" --cwd "$WORK" \
  --tag role=agent --tag keep=true \
  --unset-env NO_COLOR \
  --env ST_AGENT=example.demo --env CATALOG="$WORK" --env ST_ROOT="$WORK" --env PTY_ROOT="$ROOT" \
  -- sh -c 'printf "READY\n"; exec sh' 2>&1); rc=$?
check "pty run -d --force --id --name --cwd --tag --env" $rc "rc=$rc out=$out"

# 3. the session is really there and running
sleep 0.4
json=$("$BIN" list --json 2>&1); rc=$?
check "pty list --json" $rc "rc=$rc"
for field in '"name"' '"status"' '"pid"' '"createdAt"' '"command"' '"cwd"'; do
  echo "$json" | grep -q "$field" && ok "list --json carries $field" || bad "list --json carries $field" "absent"
done
echo "$json" | grep -q '"status":"running"' && ok "list --json says running" || bad "list --json says running" "$(echo "$json" | head -c 200)"
echo "$json" | grep -q '"role":"agent"' && ok "run persisted --tag role=agent" || bad "run persisted --tag role=agent" "tags missing"
echo "$json" | grep -q '"keep":"true"' && ok "run persisted --tag keep=true" || bad "run persisted --tag keep=true" "tag missing"
echo "$json" | grep -q '"displayName":"example.demo"' && ok "run persisted --name" || bad "run persisted --name" "name missing"

# 4. the child really inherited the --env values
sleep 0.2
"$BIN" send "$ID" --seq 'printf "ENVCHECK=%s\n" "$ST_AGENT"' --seq key:return >/dev/null 2>&1
sleep 0.5
seen=$("$BIN" peek --full --plain "$ID" 2>&1)
echo "$seen" | grep -q "ENVCHECK=example.demo" && ok "--env reached the child" || bad "--env reached the child" "no ENVCHECK line"
echo "$seen" | grep -q "READY" && ok "peek --full --plain shows output" || bad "peek --full --plain shows output" "no READY"

# 5. ding's three send shapes
ESC=$(printf '\033')
PASTE="${ESC}[200~a notice${ESC}[201~"
"$BIN" send "$ID" --seq "$PASTE" >/dev/null 2>&1
check "pty send --seq <bracketed paste>  (ding stage)" $? "rc=$?"
"$BIN" send "$ID" --seq key:return >/dev/null 2>&1
check "pty send --seq key:return  (ding submit)" $? "rc=$?"
"$BIN" send "$ID" --with-delay 0.5 --seq "$PASTE" --seq key:return >/dev/null 2>&1
check "pty send --with-delay 0.5 --seq .. --seq ..  (ding recovery)" $? "rc=$?"

# 6. ding's plain peek
"$BIN" peek "$ID" >/dev/null 2>&1
check "pty peek <id>  (ding inspection)" $? "rc=$?"

# 7. metadata patch on stdin, st2's presentation write
printf '{"displayName":"Build owner","tags":{"role":"agent","lane":"one"}}' | \
  "$BIN" metadata patch --id "$ID" >/dev/null 2>&1
check "pty metadata patch --id <id>  (stdin JSON)" $? "rc=$?"
"$BIN" list --json 2>&1 | grep -q '"displayName":"Build owner"' \
  && ok "metadata patch changed displayName" || bad "metadata patch changed displayName" "not applied"
"$BIN" list --json 2>&1 | grep -q '"lane":"one"' \
  && ok "metadata patch changed tags" || bad "metadata patch changed tags" "not applied"

# 8. a second run at a used id must be refused, and say so the way st2 matches on
out=$("$BIN" run -d --force --id "$ID" --no-display-name --cwd "$WORK" -- sh -c 'exec cat' 2>&1); rc=$?
if [ $rc -ne 0 ]; then ok "pty run refuses a live id"; else bad "pty run refuses a live id" "rc=0"; fi

# 9. kill, then the exit evidence keep=true is supposed to protect
"$BIN" kill "$ID" >/dev/null 2>&1
check "pty kill <id>" $? "rc=$?"
sleep 0.8
"$BIN" list --json 2>&1 | grep -q "$ID" \
  && ok "kill kept the record (tag keep=true)" || bad "kill kept the record (tag keep=true)" "record gone"

# 10. rm, and rm of something absent
"$BIN" rm "$ID" >/dev/null 2>&1
check "pty rm <id>" $? "rc=$?"
out=$("$BIN" rm "$ID" 2>&1); rc=$?
echo "$out" | grep -qi "not found" \
  && ok "pty rm of an absent id says 'not found'" || bad "pty rm of an absent id says 'not found'" "said: $out"

# 11. run with --no-display-name, st2's other presentation branch
"$BIN" run -d --force --id "example.demo.ding" --no-display-name --cwd "$WORK" -- sh -c 'exec cat' >/dev/null 2>&1
check "pty run --no-display-name" $? "rc=$?"
sleep 0.3
dn=$("$BIN" list --json 2>&1 | grep -o '"displayName":[^,}]*')
if [ -z "$dn" ] || [ "$dn" = '"displayName":null' ]; then
  ok "--no-display-name leaves no display name"
else
  bad "--no-display-name leaves no display name" "$dn"
fi
"$BIN" kill example.demo.ding >/dev/null 2>&1; "$BIN" rm example.demo.ding >/dev/null 2>&1

# 12. list --json on an empty root: st2 calls this constantly
EMPTY=$(mktemp -d /tmp/st2empty.XXXXXX)
out=$(PTY_ROOT="$EMPTY/sub" "$BIN" list --json 2>&1); rc=$?
[ $rc -eq 0 ] && [ "$(echo "$out" | tr -d ' \n')" = "[]" ] \
  && ok "list --json on a missing root is []" || bad "list --json on a missing root is []" "rc=$rc out=$out"
rm -rf "$EMPTY"

echo
echo "passed=$PASS failed=$FAIL"
rm -rf "$ROOT" "$WORK"
[ "$FAIL" -eq 0 ]
