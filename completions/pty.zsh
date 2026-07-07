#compdef pty
# Zsh completion for pty
# Place in your fpath (e.g. ~/.zsh/completions/_pty) or install via
# scripts/install-completions.sh.

_pty() {
  local root="${PTY_ROOT:-${PTY_SESSION_DIR:-${HOME}/.local/state/pty}}"

  _pty_sessions() {
    local -a sessions
    if [[ -d "${root}" ]]; then
      sessions=(${root}/*.json(N:t:r))
    fi
    _describe 'session' sessions
  }

  local -a commands
  commands=(
    'run:Create a session and attach'
    'attach:Attach to an existing session'
    'a:Alias for attach'
    'exec:Replace the current session process'
    'peek:Print current screen (or follow / wait-for-text)'
    'send:Send text or key events'
    'events:Follow event log'
    'list:List sessions'
    'ls:Alias for list'
    'stats:Live CPU / memory / PIDs'
    'restart:SIGTERM + respawn'
    'kill:SIGTERM a running session'
    'rm:Remove exited metadata'
    'remove:Alias for rm'
    'gc:Reconciliation pass'
    'tag:Read / write tags on one session'
    'tag-multi:Bulk tag ops across sessions'
    'emit:Publish a user.* event'
    'rename:Set / show / clear displayName'
    'up:Start sessions from pty.toml'
    'down:Stop sessions from pty.toml'
    'test:Run the pty test suite (vitest)'
    'help:Show usage'
  )

  _arguments -C \
    '(--root)--root[Pin PTY_ROOT for this call]:path:_directories' \
    '(--preselect-new)--preselect-new[TUI: pre-select "Create new session..."]' \
    '(--filter-tag)*--filter-tag[TUI: filter to k=v (repeatable)]:tag:' \
    '1:command:->command' \
    '*::arg:->args'

  case $state in
    command)
      _describe 'command' commands
      ;;
    args)
      case ${words[1]} in
        attach|a)
          _arguments \
            '-r[Auto-restart if the session is exited]' \
            '--force[Attach even from inside another pty]' \
            '1:session:_pty_sessions'
          ;;
        exec)
          _normal
          ;;
        peek)
          _arguments \
            '-f[Follow output read-only]' \
            '--plain[Plain text (no ANSI)]' \
            '--full[Include full scrollback]' \
            '--wait[Wait until text appears]:pattern:' \
            '-t[Timeout in seconds for --wait]:seconds:' \
            '1:session:_pty_sessions'
          ;;
        send)
          _arguments \
            '1:session:_pty_sessions' \
            '*--seq[Ordered chunk / key event]:value:' \
            '--with-delay[Delay between --seq items (sec)]:seconds:' \
            '--paste[Wrap in bracketed-paste markers]:text:'
          ;;
        events)
          _arguments \
            '--all[Follow every session, interleaved]' \
            '--recent[Print recent + exit]' \
            '--json[Emit raw JSONL]' \
            '--wait[Wait for a specific event type]:type:' \
            '-t[Timeout in seconds for --wait]:seconds:' \
            '1:session:_pty_sessions'
          ;;
        list|ls)
          _arguments \
            '--json[Emit JSON]' \
            '--tags[Include internal bookkeeping tags]' \
            '*--filter-tag[Filter to k=v (ALL must match)]:tag:' \
            '--remote[Include remote sessions via pty-relay]'
          ;;
        stats)
          _arguments \
            '--json[Emit JSON]' \
            '1:session:_pty_sessions'
          ;;
        restart)
          _arguments \
            '-y[Skip confirmation]' \
            '--force[Attach after restart even from inside another pty]' \
            '1:session:_pty_sessions'
          ;;
        kill|rm|remove)
          _arguments '1:session:_pty_sessions'
          ;;
        gc)
          _arguments \
            '(-n --dry-run)'{-n,--dry-run}'[Preview without changing anything]' \
            '--idle-days[Reap permanents with no attach in N days]:days:' \
            '--fast-fail-window[Fast-fail window in seconds (default 60)]:seconds:' \
            '--fast-fail-limit[Consecutive fast fails before flapping (default 3)]:count:' \
            '--print-launchd-plist[Emit a launchd plist that runs pty gc]' \
            '--interval[Plist StartInterval seconds (default 30)]:seconds:'
          ;;
        tag)
          _arguments \
            '1:session:_pty_sessions' \
            '*--rm[Remove tag key]:key:'
          ;;
        tag-multi)
          _arguments \
            '--all[Selector: every session]' \
            '*--filter-tag[Selector: k=v (repeatable)]:tag:' \
            '*--rm[Remove tag key]:key:' \
            '--json[Read mode: emit JSON]' \
            '(-y --yes)'{-y,--yes}'[Confirm --all + write]'
          ;;
        emit)
          _arguments \
            '--json[JSON payload]:payload:' \
            '--text[Text payload]:text:' \
            '1:session:_pty_sessions'
          ;;
        rename)
          _arguments \
            '--show[Print current displayName]' \
            '--clear[Remove displayName]' \
            '1:session:_pty_sessions'
          ;;
        up|down)
          _arguments '1:directory:_directories'
          ;;
        run)
          # After --, fall back to normal (command + file) completion
          local -i i
          for (( i=1; i <= $#words; i++ )); do
            if [[ "${words[$i]}" == "--" ]]; then
              shift $i words
              (( CURRENT -= i ))
              _normal
              return
            fi
          done
          _arguments \
            '-d[Create in background (detached)]' \
            '-a[Create OR attach if id already exists]' \
            '-e[Ephemeral: auto-remove on clean exit]' \
            '--id[Pin on-disk id]:id:' \
            '--name[Display label]:label:' \
            '--no-display-name[Skip the auto-generated label]' \
            '*--tag[Tag session with k=v]:tag:' \
            '--cwd[Working directory]:dir:_directories' \
            '--isolate-env[Scrub env to a safe allow-list]'
          ;;
        test)
          _arguments \
            '-t[Run matching tests]:pattern:' \
            '1:mode:(watch)'
          ;;
      esac
      ;;
  esac
}

_pty "$@"
