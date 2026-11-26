# Shell integration snippet for bash/zsh supporting iTermMinimal + regular tmux over SSH.
# Source from ~/.bashrc or ~/.zshrc.

__iterm_minimal_osc_begin() {
  if [[ "${TERM:-}" == screen* || "${TERM:-}" == tmux* || -n "${TMUX:-}" ]]; then
    printf '\033Ptmux;\033\033]'
  else
    printf '\033]'
  fi
}

__iterm_minimal_osc_end() {
  if [[ "${TERM:-}" == screen* || "${TERM:-}" == tmux* || -n "${TMUX:-}" ]]; then
    printf '\007\033\\'
  else
    printf '\007'
  fi
}

__iterm_minimal_report_state() {
  local rc=$?
  local h="${iterm2_hostname:-$(hostname -f 2>/dev/null || hostname)}"
  __iterm_minimal_osc_begin; printf '133;D;%d' "$rc"; __iterm_minimal_osc_end
  __iterm_minimal_osc_begin; printf '1337;RemoteHost=%s@%s' "$USER" "$h"; __iterm_minimal_osc_end
  __iterm_minimal_osc_begin; printf '1337;CurrentDir=%s' "$PWD"; __iterm_minimal_osc_end
  __iterm_minimal_osc_begin; printf '133;A'; __iterm_minimal_osc_end
  return "$rc"
}

if [[ -n "${BASH_VERSION:-}" ]]; then
  PROMPT_COMMAND="__iterm_minimal_report_state${PROMPT_COMMAND:+; $PROMPT_COMMAND}"
elif [[ -n "${ZSH_VERSION:-}" ]]; then
  autoload -Uz add-zsh-hook
  add-zsh-hook precmd __iterm_minimal_report_state
fi
