# Fish completion for pty
# Persistent terminal sessions with detach/attach support.
# Install: `cp completions/pty.fish ~/.config/fish/completions/` or
#          run `scripts/install-completions.sh`.

function __pty_root
    if set -q PTY_ROOT
        echo $PTY_ROOT
    else if set -q PTY_SESSION_DIR
        echo $PTY_SESSION_DIR
    else
        echo "$HOME/.local/state/pty"
    end
end

function __pty_sessions
    set -l dir (__pty_root)
    if test -d "$dir"
        for f in $dir/*.json
            if test -f "$f"
                basename $f .json
            end
        end
    end
end

function __pty_needs_command
    set -l cmd (commandline -opc)
    test (count $cmd) -eq 1
end

function __pty_using_command
    set -l cmd (commandline -opc)
    test (count $cmd) -ge 2; and test "$cmd[2]" = "$argv[1]"
end

# Disable file completions by default
complete -c pty -f

# ── Global flags ───────────────────────────────────────────────────────
complete -c pty -n __pty_needs_command -l root -x -a '(__fish_complete_directories)' -d 'Pin PTY_ROOT for this call'
complete -c pty -n __pty_needs_command -l preselect-new -d 'TUI: pre-select "Create new session..."'
complete -c pty -n __pty_needs_command -l filter-tag -x -d 'TUI: filter to k=v (repeatable)'

# ── Subcommands ────────────────────────────────────────────────────────
complete -c pty -n __pty_needs_command -a run       -d 'Create a session and attach'
complete -c pty -n __pty_needs_command -a attach    -d 'Attach to an existing session'
complete -c pty -n __pty_needs_command -a a         -d 'Alias for attach'
complete -c pty -n __pty_needs_command -a exec      -d 'Replace the current session process'
complete -c pty -n __pty_needs_command -a peek      -d 'Print current screen (or follow / wait-for-text)'
complete -c pty -n __pty_needs_command -a send      -d 'Send text or key events'
complete -c pty -n __pty_needs_command -a events    -d 'Follow event log'
complete -c pty -n __pty_needs_command -a list      -d 'List sessions'
complete -c pty -n __pty_needs_command -a ls        -d 'Alias for list'
complete -c pty -n __pty_needs_command -a stats     -d 'Live CPU / memory / PIDs'
complete -c pty -n __pty_needs_command -a restart   -d 'SIGTERM + respawn'
complete -c pty -n __pty_needs_command -a kill      -d 'SIGTERM a running session'
complete -c pty -n __pty_needs_command -a rm        -d 'Remove exited metadata'
complete -c pty -n __pty_needs_command -a remove    -d 'Alias for rm'
complete -c pty -n __pty_needs_command -a gc        -d 'Reconciliation pass'
complete -c pty -n __pty_needs_command -a tag       -d 'Read / write tags on one session'
complete -c pty -n __pty_needs_command -a tag-multi -d 'Bulk tag ops across sessions'
complete -c pty -n __pty_needs_command -a emit      -d 'Publish a user.* event'
complete -c pty -n __pty_needs_command -a rename    -d 'Set / show / clear displayName'
complete -c pty -n __pty_needs_command -a up        -d 'Start sessions from pty.toml'
complete -c pty -n __pty_needs_command -a down      -d 'Stop sessions from pty.toml'
complete -c pty -n __pty_needs_command -a test      -d 'Run the pty test suite (vitest)'
complete -c pty -n __pty_needs_command -a help      -d 'Show usage'

# ── run ────────────────────────────────────────────────────────────────
complete -c pty -n '__pty_using_command run' -s d -d 'Create in background (detached)'
complete -c pty -n '__pty_using_command run' -s a -d 'Create OR attach if id already exists'
complete -c pty -n '__pty_using_command run' -s e -d 'Ephemeral: auto-remove metadata on clean exit'
complete -c pty -n '__pty_using_command run' -l id -x -d 'Pin on-disk id (charset-validated)'
complete -c pty -n '__pty_using_command run' -l name -x -d 'Display label (any printable, ≤ 500 chars)'
complete -c pty -n '__pty_using_command run' -l no-display-name -d 'Skip the auto-generated label'
complete -c pty -n '__pty_using_command run' -l tag -x -d 'Tag session (k=v, repeatable)'
complete -c pty -n '__pty_using_command run' -l cwd -x -a '(__fish_complete_directories)' -d 'Working directory'
complete -c pty -n '__pty_using_command run' -l isolate-env -d 'Scrub env to a safe allow-list'
complete -c pty -n '__pty_using_command run' -F

# ── attach / a ─────────────────────────────────────────────────────────
for verb in attach a
    complete -c pty -n "__pty_using_command $verb" -a '(__pty_sessions)' -d 'Session'
    complete -c pty -n "__pty_using_command $verb" -s r -d 'Auto-restart if the session is exited'
    complete -c pty -n "__pty_using_command $verb" -l force -d 'Attach even from inside another pty'
end

# ── exec ───────────────────────────────────────────────────────────────
complete -c pty -n '__pty_using_command exec' -F

# ── peek ───────────────────────────────────────────────────────────────
complete -c pty -n '__pty_using_command peek' -a '(__pty_sessions)' -d 'Session'
complete -c pty -n '__pty_using_command peek' -s f -d 'Follow output read-only'
complete -c pty -n '__pty_using_command peek' -l plain -d 'Plain text (no ANSI)'
complete -c pty -n '__pty_using_command peek' -l full -d 'Include full scrollback'
complete -c pty -n '__pty_using_command peek' -l wait -x -d 'Wait until text appears'
complete -c pty -n '__pty_using_command peek' -s t -x -d 'Timeout (seconds) for --wait'

# ── send ───────────────────────────────────────────────────────────────
complete -c pty -n '__pty_using_command send' -a '(__pty_sessions)' -d 'Session'
complete -c pty -n '__pty_using_command send' -l seq -x -d 'Ordered chunk / key event (repeatable)'
complete -c pty -n '__pty_using_command send' -l with-delay -x -d 'Delay between --seq items (sec)'
complete -c pty -n '__pty_using_command send' -l paste -x -d 'Wrap in bracketed-paste markers'

# ── events ─────────────────────────────────────────────────────────────
complete -c pty -n '__pty_using_command events' -a '(__pty_sessions)' -d 'Session'
complete -c pty -n '__pty_using_command events' -l all -d 'Follow every session, interleaved'
complete -c pty -n '__pty_using_command events' -l recent -d 'Print recent + exit'
complete -c pty -n '__pty_using_command events' -l json -d 'Emit raw JSONL'
complete -c pty -n '__pty_using_command events' -l wait -x -d 'Wait for a specific event type'
complete -c pty -n '__pty_using_command events' -s t -x -d 'Timeout (seconds) for --wait'

# ── list / ls ──────────────────────────────────────────────────────────
for verb in list ls
    complete -c pty -n "__pty_using_command $verb" -l json -d 'Emit JSON'
    complete -c pty -n "__pty_using_command $verb" -l tags -d 'Include internal bookkeeping tags'
    complete -c pty -n "__pty_using_command $verb" -l filter-tag -x -d 'Filter to k=v (repeatable, ALL match)'
    complete -c pty -n "__pty_using_command $verb" -l remote -d 'Include remote sessions via pty-relay'
end

# ── stats ──────────────────────────────────────────────────────────────
complete -c pty -n '__pty_using_command stats' -a '(__pty_sessions)' -d 'Session'
complete -c pty -n '__pty_using_command stats' -l json -d 'Emit JSON'

# ── restart / kill / rm ────────────────────────────────────────────────
complete -c pty -n '__pty_using_command restart' -a '(__pty_sessions)' -d 'Session'
complete -c pty -n '__pty_using_command restart' -s y -d 'Skip confirmation'
complete -c pty -n '__pty_using_command restart' -l force -d 'Attach after restart even from inside another pty'

complete -c pty -n '__pty_using_command kill' -a '(__pty_sessions)' -d 'Session'
complete -c pty -n '__pty_using_command rm'     -a '(__pty_sessions)' -d 'Session'
complete -c pty -n '__pty_using_command remove' -a '(__pty_sessions)' -d 'Session'

# ── gc ─────────────────────────────────────────────────────────────────
complete -c pty -n '__pty_using_command gc' -s n -l dry-run -d 'Preview without changing anything'
complete -c pty -n '__pty_using_command gc' -l idle-days -x -d 'Reap permanents with no attach in N days'
complete -c pty -n '__pty_using_command gc' -l fast-fail-window -x -d 'Fast-fail window (seconds; default 60)'
complete -c pty -n '__pty_using_command gc' -l fast-fail-limit -x -d 'Consecutive fast fails before flapping (default 3)'
complete -c pty -n '__pty_using_command gc' -l print-launchd-plist -d 'Emit a launchd plist that runs pty gc'
complete -c pty -n '__pty_using_command gc' -l interval -x -d 'Plist StartInterval seconds (default 30)'

# ── tag / tag-multi ────────────────────────────────────────────────────
complete -c pty -n '__pty_using_command tag' -a '(__pty_sessions)' -d 'Session'
complete -c pty -n '__pty_using_command tag' -l rm -x -d 'Remove tag key (repeatable)'

complete -c pty -n '__pty_using_command tag-multi' -a '(__pty_sessions)' -d 'Explicit session'
complete -c pty -n '__pty_using_command tag-multi' -l all -d 'Selector: every session'
complete -c pty -n '__pty_using_command tag-multi' -l filter-tag -x -d 'Selector: k=v (repeatable)'
complete -c pty -n '__pty_using_command tag-multi' -l rm -x -d 'Remove tag key (repeatable)'
complete -c pty -n '__pty_using_command tag-multi' -l json -d 'Read mode: emit JSON'
complete -c pty -n '__pty_using_command tag-multi' -s y -l yes -d 'Confirm --all + write'

# ── emit ───────────────────────────────────────────────────────────────
complete -c pty -n '__pty_using_command emit' -a '(__pty_sessions)' -d 'Session (omit inside a session)'
complete -c pty -n '__pty_using_command emit' -l json -x -d 'JSON payload'
complete -c pty -n '__pty_using_command emit' -l text -x -d 'Text payload'

# ── rename ─────────────────────────────────────────────────────────────
complete -c pty -n '__pty_using_command rename' -a '(__pty_sessions)' -d 'Session'
complete -c pty -n '__pty_using_command rename' -l show -d 'Print current displayName'
complete -c pty -n '__pty_using_command rename' -l clear -d 'Remove displayName'

# ── up / down ──────────────────────────────────────────────────────────
for verb in up down
    complete -c pty -n "__pty_using_command $verb" -a '(__fish_complete_directories)' -d 'Directory containing pty.toml'
end

# ── test ───────────────────────────────────────────────────────────────
complete -c pty -n '__pty_using_command test' -a 'watch' -d 'Watch mode'
complete -c pty -n '__pty_using_command test' -s t -x -d 'Run matching tests'
