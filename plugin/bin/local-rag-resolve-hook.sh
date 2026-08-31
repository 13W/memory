#!/bin/sh
# Find `local-rag-hook` and exec it. The first shell script in this
# repository, and it exists because the hook path must not cost a Node
# start: `13 §1` gives it `<50 ms cold`, and `hooks.json` runs it seven
# times per session.
#
# THE CONTRACT THIS FILE UPHOLDS is `11 §3.1` `[FIXED]` — seven events,
# always exit 0 — and that contract covers the whole command a `hooks.json`
# entry invokes, not just the binary once it is running. This script does
# NOT enforce it: it exits 127 when it finds nothing, and the `|| true` at
# the end of every `hooks.json` line is what makes the contract hold. That
# split is deliberate. A script that swallowed its own failure could not
# tell a caller "there is nothing here", and `SessionStart` needs exactly
# that distinction to decide whether to speak.
#
# BUILT-INS ONLY — `command -v`, `case`, `read`, `printf`, parameter
# expansion, `[ -x ]`, `exec`. No `dirname`, no `grep`, no `awk`, no `cat`.
# (`printf` is a builtin in every shell this can land on — dash, bash, zsh,
# busybox ash — which is what makes it admissible here.) Two reasons, and
# neither is asceticism. First, `T22-14` requires this script and
# `plugin/bin/local-rag-mcp-launcher.js`'s `candidateBinDirs()` to produce a
# byte-identical ordered list from the same environment, so every rule here
# has to be one a shell can state without help; `--print-candidates` is the
# seam that test uses. Second, an external command is a dependency on
# `PATH`, and a hook that must work when `PATH` is launchd's four entries
# cannot afford one.
#
# ONE HONEST DIVERGENCE from the JS list, and it is structural rather than
# an oversight. The launcher derives "the directory beside node" from
# `process.execPath`, which it always knows. A shell can only ask
# `command -v node`, so when node is not on `PATH` — the GUI-`PATH` case
# the well-known rung exists for — that entry is absent here and present
# there. Resolution does not break: the absolute well-known directories
# are still scanned, and the missing entry is precisely the one that would
# have been empty in that scenario anyway.
#
# CO-LOCATION IS NOT REQUIRED HERE, unlike the MCP launcher. That one
# insists the daemon sit beside the proxy, because `connect.rs:55` looks
# for it there. `local-rag-hook` writes to the spool and talks to the
# daemon over a socket; it needs nothing beside it, and demanding a full
# set would refuse an installation that is complete for this purpose.
#
# STDIN IS UNTOUCHED. The hook event arrives on stdin, and this script
# builds its list with a pipeline and a command substitution before
# `exec`. Neither reads the script's own stdin; measured, not assumed.

set -u

BINARY="local-rag-hook"

# Every directory to look in, in order — the same rules, in the same
# sequence, as `candidateBinDirs()`.
_emit_candidates() {
    # `LOCAL_RAG_TEST_BIN_DIRS` replaces the whole list rather than
    # extending it, so a test can neutralise the real environment instead
    # of competing with it.
    if [ -n "${LOCAL_RAG_TEST_BIN_DIRS:-}" ]; then
        _rest="$LOCAL_RAG_TEST_BIN_DIRS"
        while [ -n "$_rest" ]; do
            case "$_rest" in
                *:*) _dir="${_rest%%:*}"; _rest="${_rest#*:}" ;;
                *)   _dir="$_rest";       _rest="" ;;
            esac
            [ -n "$_dir" ] && printf '%s\n' "$_dir"
        done
        return 0
    fi

    [ -n "${LOCAL_RAG_BIN_DIR:-}" ] && printf '%s\n' "$LOCAL_RAG_BIN_DIR"

    _rest="${PATH:-}"
    while [ -n "$_rest" ]; do
        case "$_rest" in
            *:*) _dir="${_rest%%:*}"; _rest="${_rest#*:}" ;;
            *)   _dir="$_rest";       _rest="" ;;
        esac
        [ -n "$_dir" ] && printf '%s\n' "$_dir"
    done

    # Derived, not guessed — see the header for why this one can be absent
    # here and present in the JS list.
    #
    # The trailing slashes are not paranoia. `command -v` concatenates the
    # PATH entry with the name, so a `PATH` entry written `/usr/bin/` yields
    # `/usr/bin//node` — dash does this, bash does not — and `${_node%/*}`
    # would then hand back `/usr/bin/`, which dedupes against the PATH entry
    # and leaves the list one shorter than `path.dirname()`'s. Found by
    # measuring dash against the JS list, not by reading the standard.
    _node="$(command -v node 2>/dev/null)" || _node=""
    if [ -n "$_node" ]; then
        _nodedir="${_node%/*}"
        while [ -n "$_nodedir" ] && [ "${_nodedir%/}" != "$_nodedir" ]; do
            _nodedir="${_nodedir%/}"
        done
        printf '%s\n' "${_nodedir:-/}"
    fi

    # `PNPM_HOME` plus its `bin` child — the JS `withBinChild` rung (D-124).
    # The stripping is the same one the node directory needs, for the same
    # measured reason: `"$_pnpm/bin"` on a value ending in `/` would emit
    # `//bin` where `path.join` emits `/bin`, and the parity test compares
    # bytes, not intent.
    if [ -n "${PNPM_HOME:-}" ]; then
        _pnpm="$PNPM_HOME"
        while [ -n "$_pnpm" ] && [ "${_pnpm%/}" != "$_pnpm" ]; do
            _pnpm="${_pnpm%/}"
        done
        _pnpm="${_pnpm:-/}"
        printf '%s\n' "$_pnpm" "${_pnpm%/}/bin"
    fi

    printf '%s\n' /opt/homebrew/bin /usr/local/bin
    if [ -n "${HOME:-}" ]; then
        printf '%s\n' \
            "$HOME/.local/bin" \
            "$HOME/.local/share/pnpm" \
            "$HOME/.local/share/pnpm/bin" \
            "$HOME/.bun/bin" \
            "$HOME/.volta/bin" \
            "$HOME/.npm-global/bin"
    fi
    return 0
}

# First occurrence wins, order preserved, exact string equality — no
# realpath, no trailing-slash normalisation. The JS side does the same, and
# any cleverness here would break the parity test rather than help.
_candidates() {
    _seen=":"
    _emit_candidates | while IFS= read -r _cand; do
        case "$_seen" in
            *":$_cand:"*) continue ;;
        esac
        _seen="$_seen$_cand:"
        printf '%s\n' "$_cand"
    done
}

if [ "${1:-}" = "--print-candidates" ]; then
    _candidates
    exit 0
fi

# `IFS`-newline plus `set -f` rather than an unquoted `$(_candidates)`: word
# splitting would break the first directory with a space in it, and globbing
# would expand one with a `*`. Both are real on the paths this list contains
# (`$HOME` alone can hold either), and the JS side has no such hazard, so the
# parity test would not have caught it.
_list="$(_candidates)"
_saved_ifs="$IFS"
IFS='
'
set -f
_found=""
for _cand in $_list; do
    if [ -x "$_cand/$BINARY" ]; then
        _found="$_cand/$BINARY"
        break
    fi
done
set +f
IFS="$_saved_ifs"

if [ -z "$_found" ]; then
    # `SessionStart` — and only `SessionStart` — points this at the golden
    # file, so "six stay silent, the seventh speaks" is one environment
    # variable rather than seven different command lines. The decision is
    # made here because this is the only place that knows whether the
    # binary was found; and when it was not, nothing else has written to
    # stdout, so this notice cannot collide with a real recall envelope.
    if [ -n "${LOCAL_RAG_NOT_INSTALLED_JSON:-}" ] && [ -r "${LOCAL_RAG_NOT_INSTALLED_JSON:-}" ]; then
        # `read` rather than `cat`, and not for tidiness: `cat` is an external
        # command, so the notice would vanish silently on any `PATH` that does
        # not carry coreutils — exactly the minimal-`PATH` situation this
        # script exists to survive. Caught by a test whose `PATH` held only the
        # fixture directory. The `|| [ -n "$_line" ]` tail emits a final line
        # that has no newline of its own; the golden always has one, and a test
        # pins that, so this is belt-and-braces rather than a shape decision.
        while IFS= read -r _line || [ -n "$_line" ]; do
            printf '%s\n' "$_line"
        done < "$LOCAL_RAG_NOT_INSTALLED_JSON"
    fi
    exit 127
fi

exec "$_found" "$@"
