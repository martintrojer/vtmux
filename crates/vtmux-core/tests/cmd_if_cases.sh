#!/usr/bin/env bash
# Regenerates src/cmd/golden/if.txt from the reference tmux 3.8. Usage:
#   crates/vtmux-core/tests/cmd_if_cases.sh
#
# Each case is a config file; on a fresh isolated server (session `s`):
#   env -u TMUX -u TMUX_PANE SHELL=/bin/sh HOME=/home/vtmux-test \
#       tmux -L <sock> -f /dev/null source-file -nv F
# Its output (stdout and stderr, the file's path written `F`) and exit
# status are recorded. Cases named `env-*` run `source-file F` instead
# (assignments applied, nothing printed) and record `show-environment -g`
# for the variables starting `T_`: which assignments a condition let
# through.
#
# Format: `### name`, the file's lines, `---`, output lines, `exit=N`, `%%`.
set -euo pipefail

REF="${VTMUX_REFERENCE_TMUX:-${XDG_CACHE_HOME:-$HOME/.cache}/vtmux/tmux-3.8/bin/tmux}"
HERE="$(cd "$(dirname "$0")" && pwd)"
OUT="$HERE/../src/cmd/golden/if.txt"
SOCK="vtmux-if-cases-$$"
WORK=$(mktemp -d)
export SHELL=/bin/sh HOME=/home/vtmux-test
unset EDITOR VISUAL
t() { env -u TMUX -u TMUX_PANE "$REF" -L "$SOCK" -f /dev/null "$@"; }
trap 't kill-server 2>/dev/null || true; rm -rf "$WORK"' EXIT
fresh() { t kill-server 2>/dev/null || true; t new-session -d -s s; }

: > "$OUT"
case_() {
    local name=$1 text=$2 out code
    fresh
    printf '%s\n' "$text" > "$WORK/F"
    if [[ $name == env-* ]]; then
        out=$(t source-file "$WORK/F" 2>&1) && code=0 || code=$?
        out+=$'\n'$(t show-environment -g | grep '^T_' || true)
    else
        out=$(t source-file -nv "$WORK/F" 2>&1) && code=0 || code=$?
    fi
    {
        echo "### $name"
        printf '%s\n' "$text"
        echo '---'
        printf '%s\n' "$out" | sed -e "s|$WORK/F|F|g" -e '/^$/d'
        echo "exit=$code"
        echo '%%'
    } >> "$OUT"
}

# ---- multi-line blocks ---------------------------------------------------
case_ true $'%if 1\ndisplay a\n%endif'
case_ false $'%if 0\ndisplay a\n%endif\ndisplay b'
case_ else $'%if 0\ndisplay a\n%else\ndisplay b\n%endif'
case_ elif-chain $'%if 0\ndisplay a\n%elif 0\ndisplay b\n%elif 1\ndisplay c\n%elif 1\ndisplay d\n%else\ndisplay e\n%endif'
case_ elif-none $'%if 0\ndisplay a\n%elif 0\ndisplay b\n%endif\ndisplay z'
case_ elif-first-wins $'%if 1\ndisplay a\n%elif 1\ndisplay b\n%else\ndisplay c\n%endif'
case_ nested $'%if 1\n%if 0\ndisplay a\n%elif 1\ndisplay b\n%else\ndisplay c\n%endif\ndisplay d\n%endif'
case_ nested-in-false $'%if 0\n%if 1\ndisplay a\n%endif\n%else\ndisplay b\n%endif'
case_ two-blocks $'%if 1\ndisplay a\n%endif\n%if 1\ndisplay b\n%endif\ndisplay c'
case_ empty-line-branch $'%if 1\n\n%endif'
case_ comments $'%if 1 # c\ndisplay a\n%endif # c'
case_ quoted-newline $'%if 1\ndisplay "a\nb"\n%endif'
# ---- conditions are formats --------------------------------------------
case_ format-unquoted $'%if #{==:a,a}\ndisplay yes\n%endif'
case_ format-unquoted-space $'%if #{==:a, b}\ndisplay yes\n%else\ndisplay no\n%endif'
case_ format-quoted $'%if "#{!=:a,b}"\ndisplay yes\n%endif'
case_ format-arith $'%if #{e|+:1,1}\ndisplay yes\n%endif'
case_ format-zero $'%if "#{e|-:1,1}"\ndisplay yes\n%else\ndisplay no\n%endif'
case_ quoted-words $'%if "a b"\ndisplay yes\n%endif'
case_ vtmux-guard $'%if "#{vtmux_version}"\ndisplay vtmux\n%else\ndisplay tmux\n%endif'
case_ vtmux-guard-unquoted $'%if #{vtmux_version}\ndisplay vtmux\n%endif'
case_ no-jobs $'%if "#(echo 1)"\ndisplay job\n%else\ndisplay nojob\n%endif'
case_ env-var-in-condition $'T_X=1\n%if "$T_X"\nT_A=1\n%endif'
case_ assignment-seen-by-format $'X1=1\n%if "#{X1}"\ndisplay seen\n%endif'
case_ hidden-seen-by-format $'%hidden H1=1\n%if "#{H1}"\ndisplay seen\n%else\ndisplay unseen\n%endif'
# `set` runs after the whole file is parsed, so the condition reads the
# option before it is set.
case_ set-then-if $'set -g @q 1\n%if "#{@q}"\ndisplay set\n%else\ndisplay unset\n%endif'
# ---- false branches are lexed, not built --------------------------------
case_ false-unknown-command $'%if 0\nnosuchcommand -x\ndisplay -p a b c\n%else\ndisplay b\n%endif'
case_ false-bad-quote $'%if 0\ndisplay "abc\n%endif'
case_ false-bad-tilde $'%if 0\ndisplay ~nosuchuser\n%endif'
# A word that is not UTF-8 is only a problem where it is built.
case_ false-non-utf8 $'%if 0\nset -g @a "\\377"\n%endif\nset -g @b 1'
case_ false-non-utf8-big $'%if 0\ndisplay \\U00110000\n%else\ndisplay b\n%endif'
case_ false-non-utf8-command $'%if 0\n\\377 x\n%endif\ndisplay b'
case_ false-non-utf8-assign $'%if 0\nA=\\377\n%endif\ndisplay b'
case_ true-unknown-command $'%if 1\nnosuchcommand\n%endif'
# ---- one-line form ------------------------------------------------------
case_ inline $'display x ; %if 1 display inl %endif'
case_ inline-first $'%if 1 display a ; display b %else display c %endif ; display d'
case_ inline-elif $'%if 0 display a %elif 1 display e %endif'
case_ inline-two $'%if 1 display a %endif ; %if 0 display b %else display c %endif'
case_ inline-nested $'%if 1 %if 0 nosuch %else display nested %endif %endif'
case_ inline-trailing-semi $'%if 1 display a ; %endif'
case_ inline-middle $'display a ; %if 1 display b %endif ; display c'
case_ inline-false-unknown $'display a ; %if 0 nosuch %endif'
# ---- inside { } ---------------------------------------------------------
case_ block $'bind x { %if 1\ndisplay a\n%else\ndisplay b\n%endif\n}'
case_ block-close-after-endif $'bind x { %if 0\ndisplay a\n%endif }'
case_ block-inline $'bind x { display a ; %if 0 display b %else display c %endif }'
case_ block-in-branch $'%if 1\nbind x { display a\ndisplay b }\n%endif\ndisplay c'
# ---- assignments under conditions (ps->scope, ps->stack) ----------------
case_ env-scope $'%if 1\nT_A1=1\n%elif 1\nT_A2=1\n%else\nT_A3=1\n%endif\n%if 1\nT_B1=1\n%elif 0\nT_B2=1\n%else\nT_B3=1\n%endif\n%if 0\nT_C1=1\n%else\nT_C2=1\n%endif'
case_ env-nested $'%if 0\n%if 1\nT_N1=1\n%endif\n%endif\n%if 1\n%if 1\nT_N2=1\n%endif\nT_N3=1\n%endif'
case_ env-inline $'%if 1 display a ; T_I1=1 %endif\n%if 0 T_I2=1 %else T_I3=1 %endif'
# ---- syntax errors ------------------------------------------------------
case_ err-empty-branch $'%if 1\n%endif'
case_ err-empty-else $'%if 0\ndisplay a\n%else\n%endif'
case_ err-unterminated $'%if 1\ndisplay a'
case_ err-stray-else $'%else'
case_ err-stray-endif $'display a\n%endif'
case_ err-no-condition $'%if\ndisplay a\n%endif'
case_ err-assignment-condition $'%if A=1\ndisplay a\n%endif'
case_ err-two-words $'%if #{x} y\ndisplay a\n%endif'
case_ err-elif-two-words $'%if 1\ndisplay a\n%elif 1 2\n%endif'
case_ err-unclosed-format $'%if #{x\ndisplay a\n%endif'
case_ err-else-same-line $'%if 1\ndisplay a\n%else display b\n%endif'
case_ err-after-endif $'%if 1\ndisplay a\n%endif display b'
case_ err-inline-trailing $'%if 1 display a %endif display b'
case_ err-inline-semi-first $'%if 1 ; display a %endif'
case_ err-inline-empty-else $'%if 1 display a %else %endif'
case_ err-inline-unclosed $'%if 1 display a\ndisplay b'
case_ err-chain-then-block $'display x ; %if 1\ndisplay y\n%endif'
case_ err-endif-brace $'%if 1\ndisplay a\n%endif }'
case_ err-later-unknown $'%if 1\ndisplay a\n%endif\nnosuch'
# ---- lines and groups around blocks (no %if) ----------------------------
case_ lines-plain $'display a # c\ndisplay b \\\n c\ndisplay "x\ny" ; nosuch'
case_ lines-block-group $'bind x { display a\ndisplay b\n}\ndisplay c'
case_ alias $'info ; splitp -h'
