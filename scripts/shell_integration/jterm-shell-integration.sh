# Shell integration snippet for bash/zsh supporting jterm + regular tmux over SSH.
# Source from ~/.bashrc or ~/.zshrc.

__jterm_osc_begin() {
  if [[ "${TERM:-}" == screen* || "${TERM:-}" == tmux* || -n "${TMUX:-}" ]]; then
    printf '\033Ptmux;\033\033]'
  else
    printf '\033]'
  fi
}

__jterm_osc_end() {
  if [[ "${TERM:-}" == screen* || "${TERM:-}" == tmux* || -n "${TMUX:-}" ]]; then
    printf '\007\033\\'
  else
    printf '\007'
  fi
}

__jterm_report_state() {
  local rc=$?
  local h="${iterm2_hostname:-$(hostname -f 2>/dev/null || hostname)}"
  __jterm_osc_begin; printf '133;D;%d' "$rc"; __jterm_osc_end
  __jterm_osc_begin; printf '1337;RemoteHost=%s@%s' "$USER" "$h"; __jterm_osc_end
  __jterm_osc_begin; printf '1337;CurrentDir=%s' "$PWD"; __jterm_osc_end
  __jterm_osc_begin; printf '133;A'; __jterm_osc_end
  return "$rc"
}

if [[ -n "${BASH_VERSION:-}" ]]; then
  PROMPT_COMMAND="__jterm_report_state${PROMPT_COMMAND:+; $PROMPT_COMMAND}"
elif [[ -n "${ZSH_VERSION:-}" ]]; then
  autoload -Uz add-zsh-hook
  add-zsh-hook precmd __jterm_report_state
fi
