# Bash completion for pty
# Source this file or copy to /etc/bash_completion.d/pty
# (or install via scripts/install-completions.sh).

_pty() {
  local cur prev commands
  COMPREPLY=()
  cur="${COMP_WORDS[COMP_CWORD]}"
  prev="${COMP_WORDS[COMP_CWORD-1]}"
  commands="run attach a exec peek send events list ls stats restart kill rm remove gc tag tag-multi emit rename up down test version help"

  # Complete subcommand (first positional).
  if [[ ${COMP_CWORD} -eq 1 ]]; then
    if [[ "${cur}" == -* ]]; then
      COMPREPLY=($(compgen -W "--root --preselect-new --filter-tag --help -h --version -v" -- "${cur}"))
    else
      COMPREPLY=($(compgen -W "${commands}" -- "${cur}"))
    fi
    return
  fi

  # Session-name provider — used by verbs that take a <ref>.
  local root="${PTY_ROOT:-${PTY_SESSION_DIR:-${HOME}/.local/state/pty}}"
  local names=""
  if [[ -d "${root}" ]]; then
    names=$(ls "${root}"/*.json 2>/dev/null | xargs -I{} basename {} .json)
  fi

  case "${COMP_WORDS[1]}" in
    attach|a)
      if [[ "${cur}" == -* ]]; then
        COMPREPLY=($(compgen -W "-r --force" -- "${cur}"))
      else
        COMPREPLY=($(compgen -W "${names}" -- "${cur}"))
      fi
      ;;
    peek)
      if [[ "${cur}" == -* ]]; then
        COMPREPLY=($(compgen -W "-f --plain --full --wait -t" -- "${cur}"))
      else
        COMPREPLY=($(compgen -W "${names}" -- "${cur}"))
      fi
      ;;
    send)
      if [[ "${cur}" == -* ]]; then
        COMPREPLY=($(compgen -W "--seq --with-delay --paste" -- "${cur}"))
      else
        COMPREPLY=($(compgen -W "${names}" -- "${cur}"))
      fi
      ;;
    events)
      if [[ "${cur}" == -* ]]; then
        COMPREPLY=($(compgen -W "--all --recent --json --wait -t" -- "${cur}"))
      else
        COMPREPLY=($(compgen -W "${names}" -- "${cur}"))
      fi
      ;;
    stats)
      if [[ "${cur}" == -* ]]; then
        COMPREPLY=($(compgen -W "--json" -- "${cur}"))
      else
        COMPREPLY=($(compgen -W "${names}" -- "${cur}"))
      fi
      ;;
    restart)
      if [[ "${cur}" == -* ]]; then
        COMPREPLY=($(compgen -W "-y --force" -- "${cur}"))
      else
        COMPREPLY=($(compgen -W "${names}" -- "${cur}"))
      fi
      ;;
    kill|rm|remove)
      COMPREPLY=($(compgen -W "${names}" -- "${cur}"))
      ;;
    rename)
      if [[ "${cur}" == -* ]]; then
        COMPREPLY=($(compgen -W "--show --clear" -- "${cur}"))
      else
        COMPREPLY=($(compgen -W "${names}" -- "${cur}"))
      fi
      ;;
    list|ls)
      if [[ "${cur}" == -* ]]; then
        COMPREPLY=($(compgen -W "--json --tags --filter-tag --remote" -- "${cur}"))
      fi
      ;;
    gc)
      if [[ "${cur}" == -* ]]; then
        COMPREPLY=($(compgen -W "-n --dry-run --idle-days --fast-fail-window --fast-fail-limit --print-launchd-plist --interval" -- "${cur}"))
      fi
      ;;
    tag)
      if [[ "${cur}" == -* ]]; then
        COMPREPLY=($(compgen -W "--rm" -- "${cur}"))
      else
        COMPREPLY=($(compgen -W "${names}" -- "${cur}"))
      fi
      ;;
    tag-multi)
      if [[ "${cur}" == -* ]]; then
        COMPREPLY=($(compgen -W "--all --filter-tag --rm --json -y --yes" -- "${cur}"))
      else
        COMPREPLY=($(compgen -W "${names}" -- "${cur}"))
      fi
      ;;
    emit)
      if [[ "${cur}" == -* ]]; then
        COMPREPLY=($(compgen -W "--json --text" -- "${cur}"))
      else
        COMPREPLY=($(compgen -W "${names}" -- "${cur}"))
      fi
      ;;
    up|down)
      # Complete directories (containing pty.toml) or session names from an
      # already-loaded toml. Directories are more common; keep it simple.
      COMPREPLY=($(compgen -o dirnames -- "${cur}"))
      ;;
    exec)
      # After --, fall through to default (command + args) completion.
      COMPREPLY=($(compgen -o default -- "${cur}"))
      ;;
    run)
      # After --, fall back to default file completion for the command.
      local i
      for (( i=2; i < COMP_CWORD; i++ )); do
        if [[ "${COMP_WORDS[i]}" == "--" ]]; then
          COMPREPLY=($(compgen -o default -- "${cur}"))
          return
        fi
      done
      # Before --, complete flags.
      if [[ "${cur}" == -* ]]; then
        COMPREPLY=($(compgen -W "-d -a -e --id --name --no-display-name --tag --cwd --isolate-env" -- "${cur}"))
      fi
      ;;
    test)
      if [[ "${cur}" == -* ]]; then
        COMPREPLY=($(compgen -W "-t" -- "${cur}"))
      else
        COMPREPLY=($(compgen -W "watch" -- "${cur}"))
      fi
      ;;
  esac
}

complete -F _pty pty
