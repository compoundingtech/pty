# Fish completion for pty
# Persistent terminal sessions with detach/attach support
# Place in ~/.config/fish/completions/ or install via: npm run install-completions

function __pty_sessions
    set -l session_dir "$PTY_SESSION_DIR"
    if test -z "$session_dir"
        set session_dir "$HOME/.local/state/pty"
    end
    if test -d "$session_dir"
        for f in $session_dir/*.json
            if test -f "$f"
                basename $f .json
            end
        end
    end
end

function __pty_wrapped
    set -l wrap_dir "$PTY_BIN_PATH"
    if test -z "$wrap_dir"
        set wrap_dir "$HOME/.local/pty/bin"
    end
    if test -d "$wrap_dir"
        for f in $wrap_dir/*
            if test -f "$f"
                basename $f
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

# Subcommands
complete -c pty -n __pty_needs_command -a run -d 'Create a session and attach'
complete -c pty -n __pty_needs_command -a attach -d 'Attach to an existing session'
complete -c pty -n __pty_needs_command -a a -d 'Attach to an existing session'
complete -c pty -n __pty_needs_command -a peek -d 'Print current screen or follow output'
complete -c pty -n __pty_needs_command -a send -d 'Send text or keys to a session'
complete -c pty -n __pty_needs_command -a events -d 'Follow terminal events from sessions'
complete -c pty -n __pty_needs_command -a list -d 'List active sessions'
complete -c pty -n __pty_needs_command -a ls -d 'List active sessions'
complete -c pty -n __pty_needs_command -a stats -d 'Show live session metrics'
complete -c pty -n __pty_needs_command -a restart -d 'Restart an exited session'
complete -c pty -n __pty_needs_command -a kill -d 'Kill a running session'
complete -c pty -n __pty_needs_command -a rm -d 'Remove an exited session'
complete -c pty -n __pty_needs_command -a remove -d 'Remove an exited session'
complete -c pty -n __pty_needs_command -a gc -d 'Remove all exited sessions'
complete -c pty -n __pty_needs_command -a wrap -d 'Auto-wrap a command in pty sessions'
complete -c pty -n __pty_needs_command -a unwrap -d 'Remove a wrapper'
complete -c pty -n __pty_needs_command -a test -d 'Run tests (vitest)'
complete -c pty -n __pty_needs_command -a help -d 'Show usage information'

# run: flags and file completion for the command argument
complete -c pty -n '__pty_using_command run' -s d -l detach -d 'Create in background'
complete -c pty -n '__pty_using_command run' -s a -l attach -d 'Attach if already running'
complete -c pty -n '__pty_using_command run' -s e -l ephemeral -d 'Auto-remove on exit'
complete -c pty -n '__pty_using_command run' -l name -x -d 'Session name (auto-generated if omitted)'
complete -c pty -n '__pty_using_command run' -F

# attach: session names and flags
complete -c pty -n '__pty_using_command attach' -a '(__pty_sessions)' -d 'Session'
complete -c pty -n '__pty_using_command attach' -s r -l auto-restart -d 'Auto-restart if exited'
complete -c pty -n '__pty_using_command a' -a '(__pty_sessions)' -d 'Session'
complete -c pty -n '__pty_using_command a' -s r -l auto-restart -d 'Auto-restart if exited'

# peek: session names and flags
complete -c pty -n '__pty_using_command peek' -a '(__pty_sessions)' -d 'Session'
complete -c pty -n '__pty_using_command peek' -s f -l follow -d 'Follow output read-only'
complete -c pty -n '__pty_using_command peek' -l plain -d 'Output plain text without ANSI'

# send: session names and flags
complete -c pty -n '__pty_using_command send' -a '(__pty_sessions)' -d 'Session'
complete -c pty -n '__pty_using_command send' -l seq -d 'Send a sequence item' -r
complete -c pty -n '__pty_using_command send' -l with-delay -d 'Delay between --seq items (seconds)' -r

# events: session names and flags
complete -c pty -n '__pty_using_command events' -a '(__pty_sessions)' -d 'Session'
complete -c pty -n '__pty_using_command events' -l all -d 'Follow events from all sessions'
complete -c pty -n '__pty_using_command events' -l recent -d 'Show recent events and exit'
complete -c pty -n '__pty_using_command events' -l json -d 'Output raw JSONL'

# list: flags
complete -c pty -n '__pty_using_command list' -l json -d 'Output as JSON'
complete -c pty -n '__pty_using_command ls' -l json -d 'Output as JSON'

# stats: session names and flags
complete -c pty -n '__pty_using_command stats' -a '(__pty_sessions)' -d 'Session'
complete -c pty -n '__pty_using_command stats' -l json -d 'Output as JSON'
complete -c pty -n '__pty_using_command stats' -l all -d 'Include exited sessions'

# restart: session names and flags
complete -c pty -n '__pty_using_command restart' -a '(__pty_sessions)' -d 'Session'
complete -c pty -n '__pty_using_command restart' -s y -l yes -d 'Skip confirmation'

# kill: session names
complete -c pty -n '__pty_using_command kill' -a '(__pty_sessions)' -d 'Session'

# rm/remove: session names
complete -c pty -n '__pty_using_command rm' -a '(__pty_sessions)' -d 'Session'
complete -c pty -n '__pty_using_command remove' -a '(__pty_sessions)' -d 'Session'

# wrap: commands in PATH, and --list flag
complete -c pty -n '__pty_using_command wrap' -s l -l list -d 'List all wrapped commands'
complete -c pty -n '__pty_using_command wrap' -a '(__fish_complete_command)' -d 'Command'

# unwrap: previously wrapped commands
complete -c pty -n '__pty_using_command unwrap' -a '(__pty_wrapped)' -d 'Wrapped command'
