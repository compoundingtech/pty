#compdef pty
# Zsh completion for pty
# Place in your fpath or source directly

_pty() {
  local session_dir="${PTY_SESSION_DIR:-${HOME}/.local/state/pty}"
  local wrap_dir="${PTY_BIN_PATH:-${HOME}/.local/pty/bin}"

  _pty_sessions() {
    local -a sessions
    if [[ -d "${session_dir}" ]]; then
      sessions=(${session_dir}/*.json(N:t:r))
    fi
    _describe 'session' sessions
  }

  _pty_wrapped() {
    local -a wrapped
    if [[ -d "${wrap_dir}" ]]; then
      wrapped=(${wrap_dir}/*(N:t))
    fi
    _describe 'wrapped command' wrapped
  }

  local -a commands
  commands=(
    'run:Create a session and attach'
    'attach:Attach to an existing session'
    'a:Attach to an existing session'
    'peek:Print current screen or follow output'
    'send:Send text or keys to a session'
    'events:Follow terminal events from sessions'
    'list:List active sessions'
    'ls:List active sessions'
    'stats:Show live session metrics'
    'restart:Restart an exited session'
    'kill:Kill a running session'
    'rm:Remove an exited session'
    'remove:Remove an exited session'
    'gc:Remove all exited sessions'
    'wrap:Auto-wrap a command in pty sessions'
    'unwrap:Remove a wrapper'
    'test:Run tests (vitest)'
    'help:Show usage information'
  )

  _arguments -C \
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
            '(-r --auto-restart)'{-r,--auto-restart}'[Auto-restart if exited]' \
            '1:session:_pty_sessions'
          ;;
        peek)
          _arguments \
            '(-f --follow)'{-f,--follow}'[Follow output read-only]' \
            '--plain[Output plain text without ANSI codes]' \
            '1:session:_pty_sessions'
          ;;
        send)
          _arguments \
            '1:session:_pty_sessions' \
            '--with-delay[Delay between --seq items (seconds)]:seconds:' \
            '*--seq[Send a sequence item]:value:'
          ;;
        kill|rm|remove)
          _arguments '1:session:_pty_sessions'
          ;;
        restart)
          _arguments \
            '(-y --yes)'{-y,--yes}'[Skip confirmation for running sessions]' \
            '1:session:_pty_sessions'
          ;;
        events)
          _arguments \
            '--all[Follow events from all sessions]' \
            '--recent[Show recent events and exit]' \
            '--json[Output raw JSONL]' \
            '1:session:_pty_sessions'
          ;;
        stats)
          _arguments \
            '--json[Output as JSON]' \
            '--all[Include exited sessions]' \
            '1:session:_pty_sessions'
          ;;
        list|ls)
          _arguments \
            '--json[Output as JSON]'
          ;;
        gc)
          # No arguments
          ;;
        wrap)
          _arguments \
            '(-l --list)'{-l,--list}'[List all wrapped commands]' \
            '1:command:_command_names -e'
          ;;
        unwrap)
          _arguments '1:wrapped:_pty_wrapped'
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
            '(-d --detach)'{-d,--detach}'[Create in background]' \
            '(-a --attach)'{-a,--attach}'[Attach if already running]' \
            '(-e --ephemeral)'{-e,--ephemeral}'[Auto-remove on exit]' \
            '--name[Session name]:name:' \
            '--cwd[Working directory]:dir:_directories' \
            '*--tag[Tag session with key=value]:tag:'
          ;;
      esac
      ;;
  esac
}

_pty "$@"
