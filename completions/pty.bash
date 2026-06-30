# Bash completion for pty
# Source this file or copy to /etc/bash_completion.d/pty

_pty() {
  local cur prev commands
  COMPREPLY=()
  cur="${COMP_WORDS[COMP_CWORD]}"
  prev="${COMP_WORDS[COMP_CWORD-1]}"
  commands="run attach a exec peek send events list ls stats restart kill rm remove gc tag up down wrap unwrap test help"

  # Complete subcommand
  if [[ ${COMP_CWORD} -eq 1 ]]; then
    COMPREPLY=($(compgen -W "${commands}" -- "${cur}"))
    return
  fi

  local session_dir="${PTY_SESSION_DIR:-${HOME}/.local/state/pty}"
  local names=""
  if [[ -d "${session_dir}" ]]; then
    names=$(ls "${session_dir}"/*.json 2>/dev/null | xargs -I{} basename {} .json)
  fi

  case "${COMP_WORDS[1]}" in
    attach|a|peek|send|kill|restart|events|rm|remove|stats|tag)
      if [[ "${cur}" == -* ]]; then
        case "${COMP_WORDS[1]}" in
          attach|a) COMPREPLY=($(compgen -W "--auto-restart -r" -- "${cur}")) ;;
          peek) COMPREPLY=($(compgen -W "--follow -f --plain --full --wait --timeout -t" -- "${cur}")) ;;
          send) COMPREPLY=($(compgen -W "--seq --with-delay" -- "${cur}")) ;;
          restart) COMPREPLY=($(compgen -W "--yes -y" -- "${cur}")) ;;
          events) COMPREPLY=($(compgen -W "--all --recent --json --wait --timeout -t" -- "${cur}")) ;;
          stats) COMPREPLY=($(compgen -W "--json --all" -- "${cur}")) ;;
        esac
      else
        COMPREPLY=($(compgen -W "${names}" -- "${cur}"))
      fi
      ;;
    list|ls)
      if [[ "${cur}" == -* ]]; then
        COMPREPLY=($(compgen -W "--json --tags" -- "${cur}"))
      fi
      ;;
    gc)
      if [[ "${cur}" == -* ]]; then
        COMPREPLY=($(compgen -W "--dry-run -n --print-launchd-plist --interval" -- "${cur}"))
      fi
      ;;
    wrap)
      if [[ "${cur}" == -* ]]; then
        COMPREPLY=($(compgen -W "--list -l" -- "${cur}"))
      else
        COMPREPLY=($(compgen -c -- "${cur}"))
      fi
      ;;
    unwrap)
      # Complete from wrapped commands in ~/.local/pty/bin (or $PTY_BIN_PATH)
      local wrap_dir="${PTY_BIN_PATH:-${HOME}/.local/pty/bin}"
      if [[ -d "${wrap_dir}" ]]; then
        local wrapped
        wrapped=$(ls "${wrap_dir}" 2>/dev/null)
        COMPREPLY=($(compgen -W "${wrapped}" -- "${cur}"))
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
        COMPREPLY=($(compgen -W "--detach -d --attach -a --ephemeral -e --name --cwd --tag" -- "${cur}"))
      fi
      ;;
  esac
}

complete -F _pty pty
