#!/bin/sh
# local-rag Memory MCP launcher for the memory-cowork plugin.
#
# Started by Claude Cowork / Claude Code as `/bin/sh <this file>` — never rely on
# the executable bit, it does not survive a zip round-trip reliably.
#
# Why this is not plugin/bin/local-rag-mcp-launcher.js:
#   1. Cowork spawns local MCP servers with launchd's bare PATH
#      (/usr/bin:/bin:/usr/sbin:/sbin), so `"command": "node"` is not found at all.
#      This launcher needs no node: it finds the native proxy itself and execs it.
#   2. The proxy sends current_dir() to the daemon as worktree_root (the daemon
#      git-probes it). In Cowork that cwd is the app's, not a repository, so code
#      search has nothing to search and `remember` falls back to global scope.
#      This launcher resolves the repository and cd's into it before exec. When
#      nothing resolves, .mcp.json's LOCAL_RAG_PER_CALL_WORKTREE=1 lets each
#      tool call name its repository in a `worktree` argument instead (D-137);
#      a repository resolved here always wins over that argument.
#
# stdout carries the JSON-RPC stream. Every diagnostic MUST go to stderr.

set -eu

log() { printf 'memory-mcp: %s\n' "$*" >&2; }

PROXY=local-rag-proxy
DAEMON=local-rag
CONFIG_FILE="${XDG_CONFIG_HOME:-$HOME/.config}/local-rag/cowork-root"

# ---------------------------------------------------------------- worktree --
# A candidate is accepted when it is an existing directory inside a git
# checkout (a .git dir or file at it or above). The directory itself is what
# the proxy reports; the daemon does its own git probe from there.
in_git() {
  _p=${1:-}
  while [ -n "$_p" ] && [ "$_p" != "/" ] && [ "$_p" != "." ]; do
    [ -e "$_p/.git" ] && return 0
    _p=$(dirname "$_p")
  done
  return 1
}

accept() {
  _d=${1:-}
  [ -n "$_d" ] && [ -d "$_d" ] || return 1
  in_git "$_d" || return 1
  printf '%s\n' "$_d"
}

resolve_worktree() {
  # 1. explicit override (the plugin's .mcp.json env)
  if [ -n "${LOCAL_RAG_WORKTREE:-}" ]; then
    accept "$LOCAL_RAG_WORKTREE" && return 0
    log "LOCAL_RAG_WORKTREE=$LOCAL_RAG_WORKTREE is not a directory inside a git checkout, ignoring it"
  fi
  # 2. spawned from inside a repository (Claude Code, `claude --plugin-dir`)
  _cwd=$(pwd 2>/dev/null || printf '')
  if [ -n "$_cwd" ] && [ "$_cwd" != "/" ] && [ "$_cwd" != "$HOME" ]; then
    accept "$_cwd" && return 0
  fi
  # 3. one line, written once, outside git and outside the zip
  if [ -r "$CONFIG_FILE" ]; then
    _line=$(head -n 1 "$CONFIG_FILE" | tr -d '\r' | sed 's,/*$,,')
    # allow a leading ~ or $HOME; nothing is evaluated
    case "$_line" in
      "~"*) _line="$HOME${_line#\~}" ;;
      '$HOME'*) _line="$HOME${_line#\$HOME}" ;;
    esac
    accept "$_line" && return 0
    log "$CONFIG_FILE points at '$_line', which is not a directory inside a git checkout"
  fi
  return 1
}

WT=$(resolve_worktree || printf '')
if [ -n "$WT" ]; then
  cd "$WT"
  log "worktree: $WT"
else
  # Not fatal: a call that names its repository in the `worktree` argument
  # (LOCAL_RAG_PER_CALL_WORKTREE=1) is routed there; one that does not works
  # in global scope, and code tools report no worktree.
  cd "${HOME:-/}" 2>/dev/null || cd /
  log "no repository configured — each call must name its repository in the 'worktree' argument,"
  log "otherwise memory falls back to global scope and code search is unavailable."
  log "To pin one repository for every call instead:"
  log "  mkdir -p $(dirname "$CONFIG_FILE") && echo /path/to/repo > $CONFIG_FILE"
fi

# ------------------------------------------------------------------ binary --
# Same rungs as plugin/bin/local-rag-mcp-launcher.js (LOCAL_RAG_BIN_DIR, PATH,
# well-known global bins), plus the newest nvm/fnm node bin: `npm i -g` under a
# version manager puts local-rag-proxy beside node, and with a bare PATH the
# shell cannot derive "beside node" from `command -v node` the way the JS
# launcher derives it from process.execPath.
pick_latest() {
  [ -d "${1:-}" ] || return 1
  _v=$(ls -1 "$1" 2>/dev/null | sort -V 2>/dev/null | tail -n 1 || true)
  [ -n "${_v:-}" ] || _v=$(ls -1 "$1" 2>/dev/null | sort | tail -n 1 || true)
  [ -n "${_v:-}" ] || return 1
  printf '%s\n' "$1/$_v"
}

candidates() {
  [ -n "${LOCAL_RAG_BIN_DIR:-}" ] && printf '%s\n' "$LOCAL_RAG_BIN_DIR"
  printf '%s\n' "${PATH:-}" | tr ':' '\n'
  _node=$(command -v node 2>/dev/null || true)
  [ -n "$_node" ] && dirname "$_node"
  if [ -n "${PNPM_HOME:-}" ]; then
    printf '%s\n' "${PNPM_HOME%/}" "${PNPM_HOME%/}/bin"
  fi
  printf '%s\n' /opt/homebrew/bin /usr/local/bin
  if [ -n "${HOME:-}" ]; then
    printf '%s\n' \
      "$HOME/.local/bin" \
      "$HOME/.local/share/pnpm" \
      "$HOME/.local/share/pnpm/bin" \
      "$HOME/Library/pnpm" \
      "$HOME/.bun/bin" \
      "$HOME/.volta/bin" \
      "$HOME/.npm-global/bin"
    for _base in "$HOME/.nvm/versions/node" \
                 "$HOME/.local/share/fnm/node-versions" \
                 "$HOME/Library/Application Support/fnm/node-versions"; do
      _latest=$(pick_latest "$_base" || printf '')
      [ -n "$_latest" ] || continue
      printf '%s\n' "$_latest/bin" "$_latest/installation/bin"
    done
  fi
}

# The daemon must sit beside the proxy (local-rag-proxy connect.rs looks for it
# there), so a directory holding only the proxy is an incomplete install.
FOUND=""
_list=$(candidates)
_saved_ifs=$IFS
IFS='
'
set -f
for _dir in $_list; do
  [ -n "$_dir" ] || continue
  if [ -x "$_dir/$PROXY" ] && [ -x "$_dir/$DAEMON" ]; then
    FOUND="$_dir/$PROXY"
    break
  fi
done
set +f
IFS=$_saved_ifs

if [ -z "$FOUND" ]; then
  log "the memory server is not installed (no $PROXY with $DAEMON beside it)."
  log "Fix:  npm i -g @13w/memory"
  log "or set LOCAL_RAG_BIN_DIR in the plugin's .mcp.json to a directory of prebuilt binaries."
  if [ -n "${LOCAL_RAG_DEBUG:-}" ]; then
    log "looked in (in order):"
    printf '%s\n' "$_list" | sed 's/^/  /' >&2
  fi
  exit 1
fi

# The proxy also spawns the daemon when none is running; give it a PATH that
# contains its own directory, since the one Cowork provides may not.
PATH="$(dirname "$FOUND"):$PATH"
export PATH

exec "$FOUND" "$@"
