#!/usr/bin/env bash
# Regenerates src/options/golden/* from the reference tmux 3.8. Usage:
#   crates/vtmux-core/tests/options_cases.sh
# (writes the files next to src/options/golden.rs, which include_str!s them)
#
# Every command runs against the reference, isolated:
#   env -u TMUX -u TMUX_PANE -u EDITOR -u VISUAL SHELL=/bin/sh \
#       HOME=/home/vtmux-test tmux -L <sock> -f /dev/null <command>
# on a server started with `new-session -d -s s` (session $0, window @0,
# pane %0). The Rust test uses the same pinned values (options::Defaults).
#
# Files:
#   defaults.txt     NAME<TAB>`show -g NAME` (one line per printed line,
#                    `\n`-joined as `\x1e`), for every option in the table
#   listings.txt     cases (format below) listing whole trees: show -s,
#                    -g, -gw, -sH, -gH, -gwH, show-hooks -g, ...
#   cases.txt        set/show command sequences and their combined output
#   env.txt          VISUAL<TAB>EDITOR<TAB>`show -gv editor`,
#                    `show -gv status-keys`, `show -gv mode-keys` for a
#                    server started with that environment (`-` = unset)
#   owner_tmux.conf, owner_mu-crew.conf
#                    the owner's two configs ($VTMUX_OWNER_CONFS, colon
#                    separated, to override) with every run/run-shell,
#                    source-file and (shell) if-shell line removed
#   owner_tmux.txt, owner_mu-crew.txt
#                    `show -sH`, `show -gH`, `show -gwH` after `source-file`
#                    of that copy, read from $OWNER_DIR (the copy's path is
#                    visible through #{current_file})
set -euo pipefail

REF="${VTMUX_REFERENCE_TMUX:-${XDG_CACHE_HOME:-$HOME/.cache}/vtmux/tmux-3.8/bin/tmux}"
HERE="$(cd "$(dirname "$0")" && pwd)"
OUT="$HERE/../src/options/golden"
SOCK="vtmux-options-cases-$$"
OWNER_CONFS="${VTMUX_OWNER_CONFS:-$HOME/dotfiles/tmux/.tmux.conf:$HOME/.local/share/mu-crew-dotfiles/tmux/mu-crew.conf}"
# Fixed so #{d:current_file} is the same on both sides.
OWNER_DIR=/tmp/vtmux-options-golden
export SHELL=/bin/sh HOME=/home/vtmux-test
unset EDITOR VISUAL
t() { env -u TMUX -u TMUX_PANE "$REF" -L "$SOCK" -f /dev/null "$@"; }
trap 't kill-server 2>/dev/null || true; rm -rf "$OWNER_DIR"' EXIT
fresh() { t kill-server 2>/dev/null || true; t new-session -d -s s; }

mkdir -p "$OUT"

# ---- every option's default ----------------------------------------------
# The names come from the generated table, so a missing option shows up as
# a missing line rather than going unnoticed.
fresh
: > "$OUT/defaults.txt"
grep -o '^        name: "[^"]*"' "$HERE/../src/options/table.in" | sed 's/.*"\(.*\)"/\1/' |
while read -r name; do
    shown=$(t show -g "$name" 2>&1; echo x)
    shown=${shown%x}; shown=${shown%$'\n'}
    printf '%s\t%s\n' "$name" "${shown//$'\n'/$'\x1e'}" >> "$OUT/defaults.txt"
done

# ---- command sequences -------------------------------------------------------
# A case: `### name`, one `$ args` line per command (arguments separated by
# the byte 0x1f, so none needs quoting and empty ones survive), then the
# combined stdout+stderr of all the commands, then `%%`.
case_() {
    local name=$1; shift
    fresh
    {
        echo "### $name"
        local cmd
        for cmd in "$@"; do
            printf '$ %s\n' "$cmd"
        done
        for cmd in "$@"; do
            local -a argv
            IFS=$'\x1f' read -r -a argv <<< "$cmd"$'\x1f.'
            unset 'argv[${#argv[@]}-1]'
            t "${argv[@]}" 2>&1 || true
        done
        echo '%%'
    } >> "$FILE"
}
T=$'\x1f'

FILE="$OUT/listings.txt"
: > "$FILE"
case_ show-s "show${T}-s"
case_ show-g "show${T}-g"
case_ show-gw "show${T}-gw"
case_ show-sH "show${T}-sH"
case_ show-gH "show${T}-gH"
case_ show-gwH "show${T}-gwH"
case_ showw-g "showw${T}-g"
case_ show-session "show"
case_ show-window "show${T}-w"
case_ show-pane "show${T}-p"
case_ show-A "show${T}-A"
case_ show-wA "show${T}-wA"
case_ show-pA "show${T}-pA"
case_ show-hooks-g "show-hooks${T}-g"
case_ show-hooks-gw "show-hooks${T}-gw"
case_ show-gv "show${T}-gv"
case_ show-sv "show${T}-sv"
case_ show-F-all "show${T}-s${T}-F${T}#{option_name}|#{option_value}|#{option_is_array}|#{option_is_string}|#{option_has_value}|#{option_array_key}"
case_ show-F-hooks "show${T}-gH${T}-F${T}#{option_name}|#{option_is_hook}|#{option_has_value}"

FILE="$OUT/cases.txt"
: > "$FILE"
# Each type: set, show, and the error texts.
case_ number "set${T}-g${T}history-limit${T}5000" "show${T}-g${T}history-limit" "show${T}-gv${T}history-limit" "set${T}-g${T}history-limit${T} 5" "show${T}-gv${T}history-limit" "set${T}-g${T}history-limit${T}+7" "show${T}-gv${T}history-limit"
case_ number-errors "set${T}-g${T}history-limit${T}x" "set${T}-g${T}history-limit${T}-1" "set${T}-g${T}history-limit${T}99999999999" "set${T}-g${T}history-limit${T}5x" "set${T}-g${T}history-limit${T}" "set${T}-g${T}history-limit" "set${T}-g${T}buffer-limit${T}0" "set${T}-g${T}tiled-layout-max-columns${T}65536" "set${T}-g${T}input-buffer-size${T}4294967295" "show${T}-gv${T}input-buffer-size" "set${T}-g${T}input-buffer-size${T}4294967296" "set${T}-g${T}input-buffer-size${T}1048575"
case_ key "set${T}-g${T}prefix${T}^B" "show${T}-g${T}prefix" "set${T}-g${T}prefix${T}C-a" "show${T}-g${T}prefix" "set${T}-g${T}prefix2${T}None" "show${T}-g${T}prefix2" "set${T}-g${T}prefix2${T}Any" "show${T}-g${T}prefix2" "set${T}-g${T}backspace${T}C-?" "show${T}-gv${T}backspace" "set${T}-g${T}prefix${T}M-Up" "show${T}-g${T}prefix" "set${T}-g${T}prefix${T}é" "show${T}-g${T}prefix"
case_ key-errors "set${T}-g${T}prefix${T}Foo" "set${T}-g${T}prefix${T}" "set${T}-g${T}prefix"
case_ colour "set${T}-g${T}status-fg${T}red" "show${T}-g${T}status-fg" "set${T}-g${T}status-bg${T}colour17" "show${T}-g${T}status-bg" "set${T}-g${T}status-bg${T}#FF0000" "show${T}-g${T}status-bg" "set${T}-g${T}status-bg${T}themegreen" "show${T}-g${T}status-bg"
case_ colour-errors "set${T}-g${T}status-fg${T}x" "set${T}-g${T}status-fg${T}" "set${T}-g${T}status-fg"
case_ flag "set${T}-g${T}mouse${T}off" "show${T}-g${T}mouse" "set${T}-g${T}mouse${T}YES" "show${T}-g${T}mouse" "set${T}-g${T}mouse${T}0" "show${T}-g${T}mouse" "set${T}-g${T}mouse${T}On" "show${T}-g${T}mouse" "set${T}-g${T}mouse${T}no" "show${T}-g${T}mouse" "set${T}-g${T}mouse${T}1" "show${T}-g${T}mouse"
case_ flag-toggle "set${T}-g${T}mouse" "show${T}-g${T}mouse" "set${T}-g${T}mouse" "show${T}-g${T}mouse" "set${T}-g${T}mouse${T}" "show${T}-g${T}mouse"
case_ flag-errors "set${T}-g${T}mouse${T}maybe" "set${T}-g${T}mouse${T}2" "set${T}-g${T}mouse${T}true"
case_ choice "set${T}-g${T}status-justify${T}right" "show${T}-g${T}status-justify" "set${T}-g${T}status-position${T}top" "show${T}-g${T}status-position" "setw${T}-g${T}mode-keys${T}vi" "showw${T}-g${T}mode-keys" "set${T}-g${T}status${T}3" "show${T}-g${T}status"
case_ choice-toggle "set${T}-g${T}status-justify" "show${T}-g${T}status-justify" "set${T}-g${T}status-justify" "show${T}-g${T}status-justify" "set${T}-g${T}status-position" "show${T}-g${T}status-position" "set${T}-g${T}status" "show${T}-g${T}status" "set${T}-g${T}status" "show${T}-g${T}status" "set${T}-g${T}status${T}3" "set${T}-g${T}status" "show${T}-g${T}status" "set${T}-g${T}status-justify${T}absolute-centre" "set${T}-g${T}status-justify" "show${T}-g${T}status-justify"
case_ choice-errors "set${T}-g${T}status-justify${T}x" "set${T}-g${T}status-justify${T}" "set${T}-g${T}status${T}ON" "set${T}-g${T}status${T}1"
case_ string "set${T}-g${T}status-left${T}hello world" "show${T}-g${T}status-left" "set${T}-g${T}status-left${T}" "show${T}-g${T}status-left" "set${T}-g${T}status-left${T}#{session_name}" "show${T}-g${T}status-left" "set${T}-g${T}status-left${T}a\"b" "show${T}-g${T}status-left" "set${T}-g${T}status-left${T}\$HOME ~x 'q'" "show${T}-g${T}status-left" "show${T}-gv${T}status-left" "set${T}-g${T}status-left${T}é 日" "show${T}-g${T}status-left" "set${T}-g${T}status-left${T}~" "show${T}-g${T}status-left"
case_ string-errors "set${T}-g${T}status-left" "set${T}-g${T}default-size${T}abc" "set${T}-g${T}default-size${T}10x" "set${T}-g${T}default-size${T}x" "set${T}-g${T}default-size${T}100x50" "show${T}-g${T}default-size" "set${T}-g${T}default-shell${T}foo" "set${T}-g${T}default-shell${T}/nonexistent/sh" "set${T}-g${T}default-shell${T}/bin/sh" "show${T}-g${T}default-shell"
case_ string-append "set${T}-ag${T}status-left${T}xx" "show${T}-g${T}status-left" "set${T}-ag${T}status-style${T}bold" "show${T}-g${T}status-style" "set${T}-ag${T}status-style${T}fg=nope" "show${T}-g${T}status-style" "set${T}-a${T}status-left${T}yy" "show${T}status-left" "set${T}-a${T}status-style${T}italics" "show${T}status-style" "show${T}-g${T}status-style"
case_ style "set${T}-g${T}status-style${T}fg=red,bg=blue" "show${T}-g${T}status-style" "set${T}-g${T}status-style${T}fg=x" "show${T}-g${T}status-style" "set${T}-g${T}status-style${T}fg=#{@c}" "show${T}-g${T}status-style" "set${T}-g${T}status-style${T}" "show${T}-g${T}status-style" "set${T}-g${T}status-style${T}default" "show${T}-g${T}status-style"
case_ colour-string "set${T}-g${T}display-panes-colour${T}red" "show${T}-g${T}display-panes-colour" "set${T}-g${T}display-panes-colour${T}x" "set${T}-g${T}display-panes-colour${T}" "show${T}-g${T}display-panes-colour" "set${T}-g${T}display-panes-colour${T}#{@x}" "show${T}-g${T}display-panes-colour" "set${T}-g${T}cursor-colour${T}fg=red"
case_ old-names "set${T}-g${T}display-panes-color${T}red" "show${T}-g${T}display-panes-color" "show${T}-g${T}display-panes-colour" "set${T}-gw${T}pane-colors[3]${T}red" "show${T}-gw${T}pane-colours" "show${T}-g${T}pane-colors" "set${T}-g${T}cursor-color${T}blue" "show${T}-g${T}cursor-colour"
case_ names "show${T}-g${T}status-left-l" "set${T}-g${T}status-left-l${T}5" "show${T}-g${T}status-left-length" "show${T}-g${T}s" "show${T}-gq${T}s" "show${T}-g${T}display-panes" "set${T}-g${T}nope${T}1" "set${T}-g${T}s${T}1" "show${T}-g${T}nope" "set${T}-gq${T}nope${T}1" "set${T}-gq${T}s${T}1"
case_ names-malformed "set${T}-g${T}terminal-features[]${T}a" "set${T}-g${T}x[${T}a" "set${T}-g${T}${T}a" "set${T}-g${T}a[1]b${T}a" "set${T}-g${T}terminal-features[99999999999]${T}a" "show${T}-g${T}x[" "show${T}-g${T}" "show${T}-g${T}status-style[]" "show${T}-g${T}terminal-features[]" "set${T}-gq${T}x[${T}1" "show${T}-gq${T}x["
case_ command "set${T}-g${T}default-client-command${T}new;  neww -n \"x y\"" "show${T}-g${T}default-client-command" "show${T}-gv${T}default-client-command" "set${T}-g${T}default-client-command${T}display \"a b" "show${T}-g${T}default-client-command" "set${T}-g${T}default-client-command${T}" "show${T}-g${T}default-client-command" "set${T}-g${T}default-client-command${T}splitp -h ; info" "show${T}-g${T}default-client-command" "set${T}-g${T}default-client-command${T}{ display a ; display b }" "show${T}-g${T}default-client-command"
case_ command-errors "set${T}-g${T}default-client-command${T}nosuch" "set${T}-g${T}default-client-command${T}{" "set${T}-g${T}default-client-command${T}display -Z" "set${T}-g${T}default-client-command" "show${T}-g${T}default-client-command"
case_ user "set${T}-g${T}@u${T}a" "set${T}-ag${T}@u${T}b" "show${T}-g${T}@u" "show${T}-gv${T}@u" "show${T}-g${T}@u[1]" "set${T}-g${T}@u${T}" "show${T}-g${T}@u" "set${T}-g${T}@u" "set${T}-g${T}@sp${T}a b" "show${T}-g${T}@sp" "set${T}-g${T}@é${T}日" "show${T}-g${T}@é"
case_ user-missing "show${T}-g${T}@nope" "show${T}-gv${T}@nope" "show${T}-gq${T}@nope" "set${T}-gu${T}@nope" "show${T}-gA${T}@nope"
case_ user-scopes "set${T}-s${T}@ss${T}1" "set${T}-g${T}@gs${T}2" "set${T}-gw${T}@gw${T}3" "set${T}@s${T}4" "set${T}-w${T}@w${T}5" "set${T}-p${T}@p${T}6" "setw${T}@sw${T}7" "show${T}-s" "show${T}-g${T}@gs" "show${T}-gw${T}@gw" "show${T}@s" "show${T}-w${T}@w" "show${T}-p${T}@p" "showw${T}@sw" "show${T}-g${T}@ss" "show${T}-w${T}@s" "show${T}-pA${T}@w" "show${T}-A${T}@gs" "show${T}-wA${T}@gw" "show${T}-pA${T}@gw"
case_ user-order "set${T}-g${T}@b${T}1" "set${T}-g${T}@a${T}2" "set${T}-g${T}@B${T}3" "set${T}-g${T}@a2${T}4" "set${T}-g${T}@_${T}5" "show${T}-g" "show${T}-g${T}-v"
case_ user-array "set${T}-g${T}@a[1]${T}2" "set${T}-gq${T}@a[1]${T}2" "set${T}-g${T}@a${T}x" "set${T}-g${T}@a[0]${T}y"
case_ not-array "set${T}-g${T}base-index[1]${T}2" "set${T}-gq${T}base-index[1]${T}2" "show${T}-g${T}base-index[1]" "show${T}-gv${T}base-index[1]"
case_ array "show${T}-g${T}terminal-features" "show${T}-gv${T}terminal-features" "set${T}-g${T}terminal-features[10]${T}xterm*:RGB" "show${T}-g${T}terminal-features" "show${T}-g${T}terminal-features[10]" "show${T}-gv${T}terminal-features[10]" "show${T}-g${T}terminal-features[99]" "show${T}-gv${T}terminal-features[99]" "set${T}-g${T}terminal-features[x]${T}a" "set${T}-ag${T}terminal-features[x]${T}b" "set${T}-ag${T}terminal-features${T}c,,d" "show${T}-g${T}terminal-features" "set${T}-gu${T}terminal-features[1]" "show${T}-g${T}terminal-features" "set${T}-g${T}terminal-features[007]${T}seven" "show${T}-g${T}terminal-features[7]" "set${T}-gu${T}terminal-features" "show${T}-g${T}terminal-features"
case_ array-assign "set${T}-g${T}update-environment${T}A B,C" "show${T}-g${T}update-environment" "set${T}-ag${T}update-environment${T}D" "show${T}-g${T}update-environment" "set${T}-g${T}update-environment${T}" "show${T}-g${T}update-environment" "show${T}-gv${T}update-environment" "set${T}-g${T}command-alias${T}a=b c,d=e" "show${T}-g${T}command-alias" "set${T}-g${T}terminal-overrides${T}a b,c" "show${T}-g${T}terminal-overrides" "set${T}-g${T}user-keys[0]${T}\\e[1~" "show${T}-g${T}user-keys"
case_ array-empty "show${T}-g${T}codepoint-widths" "show${T}-gv${T}codepoint-widths" "show${T}-g${T}user-keys" "set${T}-g${T}user-keys[3]${T}x" "set${T}-gu${T}user-keys[3]" "show${T}-g${T}user-keys" "show${T}-gw${T}pane-colours" "show${T}-gA${T}pane-colours" "show${T}-wA${T}pane-colours" "show${T}-w${T}pane-colours"
case_ array-colour "set${T}-g${T}pane-colours${T}red,blue" "show${T}-g${T}pane-colours" "set${T}-ag${T}pane-colours[0]${T}blue" "show${T}-g${T}pane-colours" "set${T}-g${T}pane-colours[0]${T}nope" "set${T}-g${T}pane-colours${T}green,nope,red" "show${T}-g${T}pane-colours" "set${T}-gw${T}pane-colours[5]${T}#102030" "show${T}-gw${T}pane-colours" "set${T}-gF${T}@pc${T}#{pane-colours} #{pane-colours[5]}" "show${T}-gv${T}@pc"
case_ array-status-format "show${T}-gv${T}status-format[1]" "set${T}-g${T}status-format[1]${T}x" "show${T}-g${T}status-format[1]" "set${T}-gu${T}status-format[1]" "show${T}-g${T}status-format[1]" "set${T}-g${T}status-format${T}a b,c" "show${T}-g${T}status-format" "set${T}-gu${T}status-format" "show${T}-gv${T}status-format[2]"
case_ array-errors "set${T}-g${T}terminal-features${T}" "set${T}-g${T}terminal-features" "set${T}-g${T}terminal-features[1]" "set${T}-go${T}terminal-features[1]${T}x" "set${T}-go${T}terminal-features[5]${T}x" "show${T}-g${T}terminal-features[5]" "set${T}-goq${T}terminal-features[1]${T}x"
case_ hooks "set-hook${T}-g${T}after-select-pane${T}display a" "set-hook${T}-ag${T}after-select-pane${T}display b" "set-hook${T}-g${T}after-select-pane[5]${T}display c;display d" "show${T}-g${T}after-select-pane" "show${T}-gv${T}after-select-pane[5]" "show-hooks${T}-g" "show${T}-gH${T}after-select-pane" "set-hook${T}-gu${T}after-select-pane[0]" "show-hooks${T}-g${T}after-select-pane" "set-hook${T}-gu${T}after-select-pane" "show${T}-g${T}after-select-pane" "show${T}-g${T}after-select-pane[3]"
case_ hooks-window "set-hook${T}-g${T}pane-exited${T}display x" "show-hooks${T}-gw" "set-hook${T}-gw${T}window-renamed${T}display y" "show-hooks${T}-gw" "set-hook${T}-p${T}pane-focus-in${T}display p" "show-hooks${T}-p" "show-hooks${T}-pw" "set-hook${T}-w${T}window-zoomed${T}display z" "show-hooks${T}-w" "set-hook${T}session-closed${T}display s" "show-hooks"
case_ hooks-errors "set-hook${T}-g${T}after-select-pane${T}nosuch" "set-hook${T}-g${T}after-select-pane[1]${T}{" "set-hook" "set-hook${T}-g" "set-hook${T}-R${T}after-select-pane" "set-hook${T}-E${T}x" "set-hook${T}-E${T}@x${T}y" "set-hook${T}-E" "set-hook${T}-g${T}after-select-pane${T}" "show${T}-g${T}after-select-pane"
case_ hooks-user "set-hook${T}-g${T}@myhook${T}display x" "show${T}-g" "show${T}-gH" "show-hooks${T}-g" "show${T}-g${T}@myhook"
case_ set-o "set${T}-go${T}base-index${T}3" "set${T}-goq${T}base-index${T}3" "show${T}-g${T}base-index" "set${T}-o${T}base-index${T}3" "show${T}base-index" "set${T}-o${T}base-index${T}4" "show${T}base-index" "set${T}-go${T}@o${T}1" "set${T}-go${T}@o${T}2" "show${T}-g${T}@o" "set${T}-gou${T}base-index"
case_ set-u "set${T}-g${T}base-index${T}5" "set${T}-gu${T}base-index" "show${T}-g${T}base-index" "set${T}base-index${T}7" "show${T}base-index" "set${T}-u${T}base-index" "show${T}base-index" "show${T}-A${T}base-index" "set${T}-u${T}base-index" "set${T}-gu${T}default-terminal" "show${T}-g${T}default-terminal" "set${T}-g${T}@x${T}1" "set${T}-gu${T}@x" "show${T}-g${T}@x" "set${T}-u${T}status-left${T}ignored" "show${T}-g${T}status-left"
case_ set-U "set${T}-p${T}cursor-style${T}bar" "set${T}-w${T}cursor-style${T}block" "show${T}-p" "show${T}-w${T}cursor-style" "set${T}-wU${T}cursor-style" "show${T}-p" "show${T}-w${T}cursor-style" "show${T}-pA${T}cursor-style" "set${T}-p${T}cursor-style${T}bar" "set${T}-gU${T}cursor-style" "show${T}-p" "set${T}-p${T}pane-colours[1]${T}red" "set${T}-w${T}pane-colours[1]${T}blue" "set${T}-U${T}-w${T}pane-colours[1]" "show${T}-p" "show${T}-w"
case_ set-F "set${T}-g${T}@a${T}A" "set${T}-gF${T}@b${T}#{@a}-#{@a}" "show${T}-g${T}@b" "set${T}-g${T}@c${T}#{@a}" "show${T}-g${T}@c" "set${T}-gF${T}status-left${T}#{@a} x" "show${T}-g${T}status-left" "set${T}-gF${T}@f${T}#{mouse}#{status}#{prefix}#{terminal-features}#{status-style}#{after-select-pane}#{default-client-command}" "show${T}-gv${T}@f" "set${T}-gF${T}@g${T}#{terminal-features[1]}|#{base-index}|#{status-format[9]}|#{@a}" "show${T}-gv${T}@g" "set${T}-gF${T}history-limit${T}#{e|+:1,2}" "show${T}-gv${T}history-limit" "set${T}-gF${T}@#{@a}${T}x" "show${T}-gv${T}@A" "show${T}-g${T}#{@a}"
case_ scope-errors "set${T}-t${T}nope${T}status${T}on" "set${T}-t${T}nope${T}-g${T}status${T}on" "set${T}-p${T}-t${T}%9${T}cursor-style${T}bar" "set${T}-w${T}-t${T}nope${T}@x${T}1" "set${T}-q${T}-t${T}nope${T}status${T}on" "show${T}-t${T}nope${T}status" "show${T}-t${T}nope" "show${T}-q${T}-t${T}nope" "show${T}-p${T}-t${T}%9" "showw${T}-t${T}nope" "setw${T}-t${T}nope${T}mode-keys${T}vi" "set${T}-t${T}nope${T}-s${T}@x${T}1" "show${T}-s${T}-t${T}nope${T}@x"
case_ inherit-session "set${T}-t${T}s${T}status${T}off" "show${T}-t${T}s${T}status" "show${T}-g${T}status" "show${T}-t${T}s" "show${T}-t${T}s${T}-A${T}status" "show${T}-t${T}s${T}-A${T}base-index" "show${T}-t${T}s${T}base-index" "set${T}-t${T}s${T}status" "show${T}-t${T}s${T}status" "set${T}-t${T}s${T}mouse" "show${T}-t${T}s${T}mouse"
case_ inherit-window "setw${T}-t${T}s:0${T}mode-keys${T}vi" "showw${T}-t${T}s:0" "showw${T}-g${T}mode-keys" "show${T}-w${T}-t${T}s:0${T}-A${T}automatic-rename" "set${T}-w${T}aggressive-resize" "show${T}-w" "show${T}-wv${T}aggressive-resize" "set${T}-p${T}remain-on-exit${T}on" "show${T}-p" "show${T}-pv${T}remain-on-exit" "show${T}-pA${T}remain-on-exit" "show${T}-wA${T}remain-on-exit" "set${T}-p${T}mode-keys${T}vi" "set${T}-p${T}aggressive-resize${T}on" "show${T}-p"
case_ inherit-flag-toggle "set${T}-g${T}mouse${T}on" "set${T}mouse" "show${T}mouse" "show${T}-g${T}mouse" "setw${T}-g${T}aggressive-resize${T}on" "setw${T}aggressive-resize" "showw${T}aggressive-resize" "set${T}-p${T}synchronize-panes" "show${T}-p${T}synchronize-panes"
case_ inherit-append "set${T}-g${T}status-left${T}AA" "set${T}-a${T}status-left${T}BB" "show${T}status-left" "set${T}-g${T}@x${T}G" "set${T}-a${T}@x${T}L" "show${T}@x" "show${T}-g${T}@x" "set${T}-ag${T}terminal-overrides${T}x" "show${T}-g${T}terminal-overrides"
case_ inherit-array "set${T}-g${T}status-format[0]${T}g0" "set${T}status-format[1]${T}s1" "show${T}status-format" "show${T}-A${T}status-format" "show${T}-g${T}status-format[0]" "set${T}status-format${T}x" "show${T}status-format" "set${T}-u${T}status-format" "show${T}status-format" "show${T}-A${T}status-format[0]"
case_ show-F "show${T}-g${T}-F${T}#{option_name}=#{option_value}${T}status-left" "show${T}-g${T}-F${T}[#{option_is_string}#{option_is_array}#{option_has_array_key}#{option_is_user}#{option_is_hook}#{option_is_parent}]${T}terminal-features" "show${T}-A${T}-F${T}#{option_name}#{?option_is_parent,*,}${T}status" "show${T}-gv${T}-F${T}<#{option_value_only}:#{option_value}>${T}base-index" "show${T}-g${T}-F${T}#{option_name}:#{option_has_value}${T}codepoint-widths" "show${T}-gv${T}-F${T}#{option_name}:#{option_has_value}${T}codepoint-widths" "set${T}-g${T}@q${T}a b" "show${T}-g${T}-F${T}#{q/a:option_value}${T}@q" "show${T}-gH${T}-F${T}#{option_name}:#{option_is_hook}${T}after-select-pane" "show${T}-g${T}-F${T}${T}base-index"
case_ show-v "show${T}-gv${T}status-left" "show${T}-gv${T}status-justify" "show${T}-gv${T}mouse" "show${T}-gv${T}prefix" "show${T}-gv${T}status-fg" "show${T}-gv${T}after-select-pane" "show${T}-gv${T}update-environment" "show${T}-gqv${T}nope" "showw${T}-gv${T}mode-keys"
case_ show-errors "show${T}-g${T}nope" "show${T}-g${T}status${T}x" "show${T}-gwq${T}nope" "showw${T}-g${T}status" "show${T}-gw${T}status" "show${T}-s${T}status" "show${T}-p${T}status"
case_ aliases "set-option${T}-g${T}@a${T}1" "set-window-option${T}-g${T}@b${T}2" "show-options${T}-g${T}@a" "show-window-options${T}-g${T}@b" "show-window-options${T}-g${T}mode-keys" "setw${T}-g${T}status${T}off" "show${T}-g${T}status"

# ---- environment-derived defaults ------------------------------------------
: > "$OUT/env.txt"
for pair in "-:-" "-:/usr/bin/nvim" "-:emacs" "vim:emacs" "/usr/local/bin/hx:-" "-:VI" "-:/opt/vi/bin/nano" "-:" ; do
    visual=${pair%%:*}; editor=${pair#*:}
    t kill-server 2>/dev/null || true
    envs=()
    [ "$visual" != - ] && envs+=("VISUAL=$visual")
    [ "$editor" != - ] && envs+=("EDITOR=$editor")
    env "${envs[@]}" "$REF" -L "$SOCK" -f /dev/null new-session -d -s s
    printf '%s\t%s\t%s|%s|%s\n' "$visual" "$editor" \
        "$(t show -gv editor)" "$(t show -gv status-keys)" "$(t show -gv mode-keys)" >> "$OUT/env.txt"
done

# ---- the owner's configs -------------------------------------------------------
mkdir -p "$OWNER_DIR"
IFS=: read -r -a confs <<< "$OWNER_CONFS"
for i in 0 1; do
    name=$([ "$i" = 0 ] && echo tmux || echo mu-crew)
    # Skipped on both sides: shell commands and other files.
    grep -Ev '^[[:space:]]*(run|run-shell|source-file|if-shell)([[:space:]]|$)' "${confs[$i]}" \
        > "$OUT/owner_$name.conf"
    cp "$OUT/owner_$name.conf" "$OWNER_DIR/$name.conf"
    fresh
    err=$(t source-file "$OWNER_DIR/$name.conf" 2>&1) || true
    {
        [ -n "$err" ] && printf '! %s\n' "$err"
        t show -sH
        t show -gH
        t show -gwH
    } > "$OUT/owner_$name.txt"
done

echo "wrote $(wc -l < "$OUT/defaults.txt") defaults, $(grep -c '^###' "$OUT/cases.txt") cases, $(grep -c '^###' "$OUT/listings.txt") listings"
