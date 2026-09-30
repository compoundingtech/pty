import {
  defaultActionlintConfig,
  githubWorkflow,
  githubWorkflowEvent,
} from '../../repos/effect-utils/genie/external.ts'
import { linuxRunner, nixSetupSteps } from './shared.ts'

export default githubWorkflow({
  name: "Conformance",
  on: {
    pull_request: githubWorkflowEvent.all,
    push: {
      branches: [
        "main",
      ],
    },
  },
  env: {
    SHARDS: 4,
  },
  jobs: {
    // Each shard runs a slice of the conformance files against BOTH binaries.
    // Slicing by file rather than by binary keeps the two sides on the same
    // runner: the suite drives real processes through real PTYs and races under
    // load, so adding a second binary's work to a shard's CPU is exactly what we
    // do not want.
    //
    // Sharding is here because the work parallelises across machines and does not
    // compress on one. The long pole is COMPILING the seventy debug test
    // binaries, not running them: measured 2026-09-05 across all 140 invocations,
    // the reported test time totals 5.1 minutes and no single invocation exceeds
    // 12.5 seconds, while the compile before the first result took about 26.
    //
    // Do not target test runtime here. An earlier version of this comment said
    // the tests were the cost, from a five-file sample that happened to contain
    // the one seven-second outlier. That sample was not representative and the
    // claim was wrong. If someone wants a bigger win than sharding, it is in the
    // compile: a shared cargo cache, or building the test binaries once and
    // distributing them, rather than four runners each compiling the workspace.
    shard: {
      'runs-on': linuxRunner,
      "timeout-minutes": 45,
      strategy: {
        "fail-fast": false,
        matrix: {
          // Keep this list and SHARDS above in step. The combine job counts the
          // results it receives and refuses if the two disagree.
          index: [
            0,
            1,
            2,
            3,
          ],
        },
      },
      steps: [
        {
          uses: "actions/checkout@v4",
        },
        ...nixSetupSteps(),
        {
          uses: "actions/setup-node@v4",
          with: {
            // The runtime is a pinned input, not a detail. It can change results,
            // so pinning only the reference COMMIT is not enough to make a
            // comparison reproducible.
            //
            // 22 matches the major the Node pty's own CI uses. Changing it is a
            // deliberate act: expect the divergence ledger to change with it.
            //
            // This comment used to say the Node binary truncates
            // `pty completions fish` on a pipe under Node v24.18.0 and not under
            // Node 22, and gave that as the reason for the pin. That was wrong.
            // Measured on Linux x86_64 on 2026-09-08, at the same pinned commit
            // 86dcc5e, with both runtimes:
            //
            //   node v22.23.2   fish piped 8192 bytes, to a file 12138
            //   node v24.18.0   fish piped 8192 bytes, to a file 12138
            //
            // It truncates on both. The variable is the PIPE, not the major.
            // Node's stdout is asynchronous to a pipe and synchronous to a file,
            // and the old `completions` branch called `process.exit()` before the
            // buffer drained, so output stopped at one 8192-byte pipe buffer.
            // The three shells make that plain at the same commit: bash is 5275
            // bytes and survives, zsh is 8327 and fish is 12138 and both stop at
            // 8192. Node fixed it in pty#178 by setting `process.exitCode` and
            // returning; that commit is one past this pin.
            //
            // The pin is still right, for the reason above it. The reason given
            // for it was not.
            "node-version": 22,
          },
        },
        // The reference commit is pinned rather than tracked. A checkout seven
        // commits behind produced eight false divergences on 2026-09-05, and
        // every one was an artifact of the stale reference.
        {
          name: "Build the Node pty at the pinned commit",
          run: `ref=$(cat crates/pty-conformance/node-ref)
echo "Node reference: $ref"
git clone --filter=blob:none https://github.com/compoundingtech/pty-original-experiment /tmp/node-pty
git -C /tmp/node-pty checkout --detach "$ref"
(cd /tmp/node-pty && npm ci --silent && npm run build --silent)
echo "node pty: $(/tmp/node-pty/bin/pty --version)"
`,
        },
        {
          name: "Build",
          run: "nix develop --command cargo build --workspace --release",
        },
        {
          name: "Conformance, both binaries, shard ${{ matrix.index }}",
          env: {
            PTY_NODE_CHECKOUT: "/tmp/node-pty",
          },
          run: `files=$(ls crates/pty-conformance/tests/*.rs \\
  | xargs -n1 basename | sed 's/\\.rs$//' \\
  | awk "NR % $SHARDS == \${{ matrix.index }}" | tr '\\n' ' ')
echo "shard \${{ matrix.index }} of $SHARDS: $files"
# Write to a FILE, not through a pipe.
#
# A pipe makes this step's completion depend on end-of-file, which
# needs every write end closed — including any inherited by a process
# that outlives the run. This suite leaves daemons behind, and on
# 2026-09-05 shard 2 finished its work in 2m44s and then sat for 38
# more minutes until the job timeout, with the runner reporting
# "Terminate orphan process: pid (23143) (pty-daemon)" at cleanup.
# Three runs, three times, each stopping at exactly the timeout.
#
# A file has no such dependency, and it keeps the partial output when
# a step IS killed, which a pipe into \`tail\` did not.
mkdir -p target/conformance
nix develop --command ./scripts/conformance-both.sh \\
  --node /tmp/node-pty/bin/pty \\
  --rust "$PWD/target/release/pty" \\
  --out "$PWD/target/conformance" $files \\
  > target/conformance/shard.log 2>&1 &
script=$!
# A heartbeat naming the most recent finished file, so a hang is
# visible while it happens rather than only in the post mortem.
while kill -0 "$script" 2>/dev/null; do
  sleep 30
  last=$(ls -t target/conformance/*/*.log 2>/dev/null | head -1)
  echo "... still running; newest result: \${last:-none yet}"
done
wait "$script" || rc=$?
echo "===== conformance output ====="
cat target/conformance/shard.log

# A difference has to survive a second look before it counts.
#
# This suite drives real processes through real PTYs and races under
# load. Both binaries can lose. Measured 2026-09-06: a run reported
# nesting_prevention::restart_force_restores_the_attach and
# up_down::down_stops_only_named_sessions as Node-side differences,
# and both files then passed 3 of 3 locally against the same Node
# binary. They were lost races, not divergences.
#
# The retry runs only the differing files, so contention is far lower
# than the full shard, and it rewrites red.txt. A file that was clean
# contributes nothing to red.txt either way, so re-running just the
# differing ones is a complete result for this shard. A real
# difference is reproducible and survives; a race has to lose twice.
if [ -s target/conformance/red.txt ]; then
  again=$(cut -d: -f1 target/conformance/red.txt | sort -u | tr '\\n' ' ')
  echo "===== differences seen; re-running only: $again ====="
  nix develop --command ./scripts/conformance-both.sh \\
    --node /tmp/node-pty/bin/pty \\
    --rust "$PWD/target/release/pty" \\
    --out "$PWD/target/conformance" $again \\
    > target/conformance/retry.log 2>&1 || rc=$?
  cat target/conformance/retry.log
  echo "===== differences that survived the retry ====="
  cat target/conformance/red.txt
fi
exit "\${rc:-0}"
`,
        },
        {
          name: "Keep this shard's differences",
          if: "always()",
          uses: "actions/upload-artifact@v4",
          with: {
            name: "red-${{ matrix.index }}",
            path: `target/conformance/red.txt
target/conformance/shard.log
target/conformance/retry.log
`,
            "if-no-files-found": "error",
            "retention-days": 14,
          },
        },
      ],
    },
    gate: {
      needs: "shard",
      if: "always()",
      'runs-on': linuxRunner,
      "timeout-minutes": 10,
      steps: [
        {
          uses: "actions/checkout@v4",
        },
        {
          uses: "actions/download-artifact@v4",
          with: {
            pattern: "red-*",
            path: "/tmp/red",
          },
        },
        // THE CHECK RUNS ONCE, OVER THE COMBINED SET. Do not move it into the
        // shards. `check-divergences.py` fails in two directions, and the second
        // one — a recorded divergence that no longer happens — can only be
        // decided by knowing the divergence appeared in NO shard. A per-shard
        // check sees a quarter of the tests, so every entry belonging to another
        // shard looks stale to it, and the gate would fail every run for a reason
        // that is not true.
        {
          name: "Combine and gate",
          run: `# A missing shard is not a pass. If one shard died, its slice of the
# tests was never compared, and both halves of the check would then
# be wrong: an unrecorded divergence in that slice goes unseen, and
# every ledger entry belonging to it looks stale. Refuse instead.
# Count red.txt files, not directories. A shard that hangs still
# uploads its streamed log, so the directory exists while the result
# does not, and counting directories would call that a complete set.
n=$(ls /tmp/red/red-*/red.txt 2>/dev/null | wc -l)
if [ "$n" -ne "$SHARDS" ]; then
  echo "expected $SHARDS red.txt results, found $n — a shard failed or hung."
  echo "The gate needs every shard's result to judge either direction."
  ls -R /tmp/red || true
  exit 1
fi
cat /tmp/red/red-*/red.txt > /tmp/red/all.txt
echo "combined divergences from all $SHARDS shards:"
cat /tmp/red/all.txt
python3 scripts/check-divergences.py /tmp/red/all.txt
`,
        },
      ],
      name: "Conformance gate",
    },
  },
  actionlint: defaultActionlintConfig,
})
