# Bash completion for pty
# Source this file or copy to /etc/bash_completion.d/pty

_pty() {
  local cur prev commands
  COMPREPLY=()
  cur="${COMP_WORDS[COMP_CWORD]}"
  prev="${COMP_WORDS[COMP_CWORD-1]}"
  commands="run attach peek send kill list restart help"

  # Complete subcommand
  if [[ ${COMP_CWORD} -eq 1 ]]; then
    COMPREPLY=($(compgen -W "${commands}" -- "${cur}"))
    return
  fi

  # Complete session names for commands that take them
  case "${COMP_WORDS[1]}" in
    attach|a|peek|send|kill|restart)
      local session_dir="${PTY_SESSION_DIR:-${HOME}/.local/state/pty}"
      if [[ -d "${session_dir}" ]]; then
        local names
        names=$(ls "${session_dir}"/*.json 2>/dev/null | xargs -I{} basename {} .json)
        COMPREPLY=($(compgen -W "${names}" -- "${cur}"))
      fi
      # Flags per subcommand
      if [[ "${cur}" == -* ]]; then
        case "${COMP_WORDS[1]}" in
          attach|a) COMPREPLY=($(compgen -W "--auto-restart -r" -- "${cur}")) ;;
          peek) COMPREPLY=($(compgen -W "--follow -f --plain" -- "${cur}")) ;;
          send) COMPREPLY=($(compgen -W "--seq --with-delay" -- "${cur}")) ;;
          restart) COMPREPLY=($(compgen -W "--yes -y" -- "${cur}")) ;;
        esac
      fi
      ;;
    list|ls)
      if [[ "${cur}" == -* ]]; then
        COMPREPLY=($(compgen -W "--json" -- "${cur}"))
      fi
      ;;
    run)
      # After --, fall back to default file completion
      local i
      for (( i=2; i < COMP_CWORD; i++ )); do
        if [[ "${COMP_WORDS[i]}" == "--" ]]; then
          COMPREPLY=($(compgen -o default -- "${cur}"))
          return
        fi
      done
      # Before --, complete flags
      if [[ "${cur}" == -* ]]; then
        COMPREPLY=($(compgen -W "--detach -d --attach -a" -- "${cur}"))
      fi
      ;;
  esac
}

complete -F _pty pty
