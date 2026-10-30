#!/usr/bin/env bash
# Regenerates src/keys/golden/*.txt from the reference tmux 3.8. Usage:
#   crates/vtmux-core/tests/keys_cases.sh
# (writes the files next to src/keys/golden.rs, which include_str!s them)
#
# Every command runs against the reference, isolated:
#   env -u TMUX -u TMUX_PANE SHELL=/bin/sh HOME=/home/vtmux-test \
#       tmux -L <sock> -f /dev/null <command>
# with EDITOR/VISUAL unset. HOME is pinned so `$HOME` in a binding expands to
# the same text on both sides (the Rust test uses the same value).
#
# Files:
#   names.txt        KEY<TAB>=name | !error     `bind -T t KEY display`, then
#                    `list-keys -T t -F '#{key_string}'` (the name tmux prints
#                    for the key it parsed), or bind's error
#   order.txt        names of one table's keys, in `list-keys` order
#   fresh.txt        `list-keys` on a fresh server (308 lines, 5 tables)
#   fresh_N.txt      `list-keys -N`
#   owner_prefix.txt `list-keys -T prefix` after sourcing the owner's bind lines
#   owner_root.txt   `list-keys -T root`  after sourcing the owner's bind lines
#   cases.txt        key command sequences with their combined output
#   config.txt       config text -> `list-keys -T t -F '#{key_string}|#{key_command}'`
set -euo pipefail

REF="${VTMUX_REFERENCE_TMUX:-${XDG_CACHE_HOME:-$HOME/.cache}/vtmux/tmux-3.8/bin/tmux}"
HERE="$(cd "$(dirname "$0")" && pwd)"
OUT="$HERE/../src/keys/golden"
SOCK="vtmux-keys-cases-$$"
WORK=$(mktemp -d)
export SHELL=/bin/sh HOME=/home/vtmux-test
unset EDITOR VISUAL
t() { env -u TMUX -u TMUX_PANE "$REF" -L "$SOCK" -f /dev/null "$@"; }
trap 't kill-server 2>/dev/null || true; rm -rf "$WORK"' EXIT
fresh() { t kill-server 2>/dev/null || true; t new-session -d -s s; }

mkdir -p "$OUT"

# ---- key names ---------------------------------------------------------
# (0x110000-0x13ffff bind in tmux but print as bytes that are not UTF-8, so
# they are not in this text file.) The first group is the FACTS list in the brief; the rest walks every branch
# of key_string_lookup_string.
NAMES=(
  # named keys, either case; Tab is C0_HT and C-i is not Tab
  Tab Enter Escape Space BSpace BTab tab ENTER escape space
  C-i C-I C-m C-j 'C-[' 'M-C-[' 'C-]' 'C-\' 'C-^' 'C-_' 'C-@' 'C-?' 'C-Space' 'C-Tab' 'C-Escape'
  '^[' '^a' '^A' '^?' '^1' '^@' '^ab' '^' '^Tab' '^C-a' '^M-a'
  C-a C-A C-z C-Z c-a m-a s-a M-a M-A S-a S-A C-M-a M-C-a C-M-S-a S-C-M-a C-C-a C-S-C-a
  'C-' 'M-' 'S-' 'C--' 'C-M--' 'X-a' 'C-X-a' 'CM-a' 'C-ab' 'C- ' 'C-  '
  # printable ASCII, one character
  a A z Z 0 9 '!' '"' '#' '$' '%' '&' "'" '(' ')' '*' '+' ',' '-' '.' '/' ':' ';' '<' '=' '>' '?' '@' '[' '\' ']' '^' '_' '`' '{' '|' '}' '~' ' '
  'a ' ' a' 'ab' 'abc'
  # function and editing keys, with and without M- (IMPLIED_META)
  F1 F2 F9 F10 F11 F12 F13 f1 M-F1 C-F1 S-F1 C-S-M-F1 M-f12
  IC Insert DC Delete Home End NPage PageDown PgDn PPage PageUp PgUp
  ic insert M-IC M-Insert M-PgUp S-Home C-End
  Up Down Left Right up M-Up C-Up S-Up C-S-Up M-C-Up C-M-Up M-S-Down
  KP/ 'KP*' KP- KP7 KP8 KP9 KP+ KP4 KP5 KP6 KP1 KP2 KP3 KPEnter KP0 'KP.' kp5 M-KP5 KPenter
  # C0 names, which print as brackets except Tab Enter Escape
  '[NUL]' '[SOH]' '[nul]' '[LF]' '[ESC]' '[US]' '[BS]' '[ASC]' '[SO]'
  # None / Any / unknown names
  None NONE Any any ANY C-Any Unknown FocusIn PasteStart Mouse Dragging x y
  # mouse keys
  MouseDown1Pane MouseDown2Pane MouseDown3Pane MouseDown4Pane MouseDown5Pane MouseDown6Pane
  MouseDown11Pane MouseDown12Pane MouseUp1Status MouseUp3StatusLeft MouseDrag1Border
  MouseDragEnd1Pane WheelUpPane WheelDownPane WheelUpStatus WheelDownStatusDefault
  WheelUp1Pane SecondClick1Pane DoubleClick1Pane TripleClick1Pane DoubleClick1Status
  MouseDown1ScrollbarUp MouseDrag1ScrollbarSlider MouseDown1ScrollbarDown MouseDown1Empty
  MouseDown1Control0 MouseDown1Control9 MouseDown1Control10 mousedown1pane MOUSEDOWN1PANE
  C-MouseDrag1Pane M-MouseDrag1Pane S-WheelUpPane C-M-WheelDownPane
  MouseMovePane MouseMove1Pane
  # UserN
  User0 User1 User5 User999 User1000 User1001 user5 USER5 'User 5' User+5 User-1 User-4294967295
  User0x5 User5abc User User- 'User5 ' ' User5' C-User5 M-User5
  # hexadecimal
  0x0 0x00 0x1 0x1f 0x20 0x41 0x61 0x7e 0x7f 0x80 0x9f 0xa0 0xe9 0xff 0x100 0x300 0x7ff 0x800
  0xfff 0x2200 0x200b 0x65e5 0xd7ff 0xd800 0xdfff 0xe000 0xfffe 0xffff 0x10000 0x1f600 0x1f4a9
  0xfffff 0x10ffff 0x140000 0x1fffff 0x200000 0x7fffffff 0xffffffff
  0x100000041 0xFFFFFFFFFF 0xFFFFFFFFFFFFFFFFFF 0x0x41 0x0x 0x0xg 0xg 0x 0X41 '0x 41' '0x  41'
  0x41zz 0x00041 0x-41 0x+41 0x41,5 C-0x41 M-0x41 0xz
  # UTF-8
  é É ñ 日 🙂 ∀ ␀ '‌' é́ 'C-é' 'M-é' 'S-é' 'C-M-é' 'C-日' 'M-🙂' 'ab日' 'é ' '^é'
)
: > "$OUT/names.txt"
for k in "${NAMES[@]}"; do
    fresh
    err=$(t bind -T t -- "$k" display 2>&1) || true
    if [ -n "$err" ]; then
        printf '%s\t!%s\n' "$k" "$err" >> "$OUT/names.txt"
    else
        name=$(t list-keys -T t -F '#{key_string}'; echo x)
        name=${name%x}; name=${name%$'\n'}
        printf '%s\t=%s\n' "$k" "$name" >> "$OUT/names.txt"
    fi
done

# ---- order ---------------------------------------------------------------
# One table holding keys from every part of the code space, bound in a
# shuffled order; list-keys prints them in key-code order. Four-byte
# characters are bound in code-point order, which is the order tmux first
# meets them (see the module docs of vtmux-core::keys on that divergence).
ORDER=(
  'M-Up' 'S-a' 'C-a' 'a' 'A' 'M-a' 'C-A' 'Tab' 'C-i' 'Enter' 'Escape' 'C-[' 'Space' 'C-Space'
  'C-?' 'C-@' 'C-Tab' 'F1' 'F12' 'M-F1' 'C-F1' 'IC' 'DC' 'Home' 'End' 'NPage' 'PPage' 'BTab' 'S-BTab'
  'Up' 'Down' 'Left' 'Right' 'C-Up' 'S-Up' 'C-S-Up' 'C-M-S-Up' 'KP5' 'KPEnter' 'M-KP5' 'BSpace'
  'MouseDown1Pane' 'MouseDown3Status' 'C-MouseDown1Pane' 'WheelUpPane' 'WheelDownStatus'
  'DoubleClick1Pane' 'TripleClick1Pane' 'SecondClick1Pane' 'MouseDrag1Border' 'MouseDragEnd1Pane'
  'MouseUp1Pane' 'MouseDown1Control9' 'User0' 'User5' 'User1000' 'Any' '0x1' '0x1f' '0x7f'
  'é' '日' '0x80' '0x300' '0xa0' 'C-é' '0x2200' 'M-日' '0x1f4a9' '0x1f600'
  '!' '~' '0' '9' 'z' 'C-z' 'M-z' 'C-M-z' 'S-C-z' 'F5' 'F10' 'M-C-Up'
)
fresh
for k in "${ORDER[@]}"; do t bind -T o -- "$k" display; done
t list-keys -T o -F '#{key_string}' > "$OUT/order.txt"

# ---- fresh server ----------------------------------------------------------
fresh
t list-keys > "$OUT/fresh.txt"
t list-keys -N > "$OUT/fresh_N.txt"

# ---- the owner's binds ---------------------------------------------------------
fresh
t source-file "$HERE/../src/keys/golden/owner_binds.conf"
t list-keys -T prefix > "$OUT/owner_prefix.txt"
t list-keys -T root > "$OUT/owner_root.txt"

# ---- command sequences -----------------------------------------------------------
# A case: `### name`, one `$ args` line per command (arguments separated by
# the byte 0x1f, so none needs quoting and empty ones survive), then the
# combined stdout+stderr of all the commands, then `%%`.
: > "$OUT/cases.txt"
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
            # A sentinel field keeps a trailing empty argument.
            IFS=$'\x1f' read -r -a argv <<< "$cmd"$'\x1f.'
            unset 'argv[${#argv[@]}-1]'
            t "${argv[@]}" 2>&1 || true
        done
        echo '%%'
    } >> "$OUT/cases.txt"
}
T=$'\x1f'
case_ list-default "list-keys"
case_ list-N "list-keys${T}-N"
case_ list-Na "list-keys${T}-N${T}-a"
case_ list-N-table "list-keys${T}-N${T}-T${T}prefix"
case_ list-N-table-a "list-keys${T}-N${T}-a${T}-T${T}root"
case_ list-T-prefix "list-keys${T}-T${T}prefix"
case_ list-T-root "list-keys${T}-T${T}root"
case_ list-T-move "list-keys${T}-T${T}move"
case_ list-T-copy-mode "list-keys${T}-T${T}copy-mode"
case_ list-T-copy-mode-vi "list-keys${T}-T${T}copy-mode-vi"
case_ list-single "list-keys${T}-1"
case_ list-single-T "list-keys${T}-1${T}-T${T}root"
case_ list-single-N "list-keys${T}-1${T}-N"
case_ list-P "list-keys${T}-N${T}-P${T}PFX"
case_ list-P-empty "list-keys${T}-N${T}-P${T}"
case_ list-P-nonN "list-keys${T}-P${T}PFX${T}-T${T}move"
case_ list-key "list-keys${T}C-b"
case_ list-key-T "list-keys${T}-T${T}prefix${T}C-b"
case_ list-key-N "list-keys${T}-T${T}prefix${T}-N${T}C-b"
case_ list-key-N-nonote "list-keys${T}-T${T}prefix${T}-N${T}C-c"
case_ list-key-mouse "list-keys${T}MouseDown1Pane"
case_ list-key-mod "list-keys${T}M-Up"
case_ list-key-tab "list-keys${T}Tab"
case_ list-key-ctab "list-keys${T}C-i"
case_ list-key-none "list-keys${T}None"
case_ list-key-f13 "list-keys${T}F13"
case_ list-key-bad "list-keys${T}nosuchkey"
case_ list-key-notbound "list-keys${T}-T${T}move${T}C-b"
case_ list-O-bad "list-keys${T}-O${T}bad"
case_ list-T-bad "list-keys${T}-T${T}nope"
case_ list-O-key "list-keys${T}-T${T}move${T}-O${T}key"
case_ list-O-index-r "list-keys${T}-T${T}move${T}-O${T}index${T}-r"
case_ list-O-modifier "list-keys${T}-T${T}move${T}-O${T}modifier"
case_ list-O-modifier-r "list-keys${T}-T${T}move${T}-O${T}modifier${T}-r"
case_ list-O-name "list-keys${T}-O${T}name"
case_ list-O-name-r "list-keys${T}-O${T}name${T}-r"
case_ list-O-title "list-keys${T}-O${T}TITLE${T}-T${T}root"
case_ list-O-order "list-keys${T}-T${T}move${T}-O${T}order"
case_ list-O-order-r "list-keys${T}-T${T}move${T}-O${T}order${T}-r"
case_ list-O-size "list-keys${T}-T${T}move${T}-O${T}size"
case_ list-O-activity-r "list-keys${T}-T${T}move${T}-O${T}activity${T}-r"
case_ list-r-alone "list-keys${T}-T${T}move${T}-r"
case_ list-F "list-keys${T}-T${T}move${T}-F${T}#{key_table}:#{key_string}:#{key_repeat}:#{key_string_width}:#{key_table_width}:#{key_has_repeat}:#{notes_only}"
case_ list-F-prefix "list-keys${T}-N${T}-F${T}[#{key_prefix}][#{key_note}]${T}-T${T}move"
case_ list-F-empty "list-keys${T}-T${T}move${T}-F${T}"
case_ list-F-1 "list-keys${T}-1${T}-F${T}#{key_string}"
case_ list-repeat "bind${T}-r${T}-T${T}zz${T}x${T}display" "bind${T}-T${T}zz${T}y${T}display" "list-keys${T}-T${T}zz"
case_ list-repeat-only "bind${T}-r${T}-T${T}zz${T}x${T}display" "list-keys${T}-T${T}zz"
case_ list-table-width "bind${T}-T${T}a-very-long-table${T}x${T}display" "bind${T}-T${T}b${T}yy${T}display" "list-keys${T}-T${T}b" "list-keys${T}-T${T}a-very-long-table" "list-keys${T}-a" 
case_ list-note-width "bind${T}-N${T}the note${T}-T${T}nn${T}C-Space${T}display" "bind${T}-N${T}n2${T}-T${T}nn${T}x${T}display" "list-keys${T}-N${T}-T${T}nn" "list-keys${T}-T${T}nn"
case_ list-utf8-width "bind${T}-T${T}uu${T}日${T}display" "bind${T}-T${T}uu${T}x${T}display" "bind${T}-T${T}uu${T}é${T}display" "list-keys${T}-T${T}uu" "list-keys${T}-N${T}-a${T}-T${T}uu"
case_ list-bind-escape "bind${T}-T${T}ee${T}\\;${T}display" "bind${T}-T${T}ee${T}\"${T}display" "bind${T}-T${T}ee${T}'${T}display" "bind${T}-T${T}ee${T}#${T}display" "bind${T}-T${T}ee${T}\$${T}display" "bind${T}-T${T}ee${T}~${T}display" "bind${T}-T${T}ee${T}%${T}display" "bind${T}-T${T}ee${T}{${T}display" "bind${T}-T${T}ee${T}}${T}display" "bind${T}-T${T}ee${T}\\${T}display" "bind${T}-T${T}ee${T} ${T}display" "bind${T}-T${T}ee${T}0x7f${T}display" "bind${T}-T${T}ee${T}0x1${T}display" "list-keys${T}-T${T}ee"
case_ bind-new "bind${T}-T${T}zz${T}x${T}display" "list-keys${T}-T${T}zz"
case_ bind-n "bind${T}-n${T}x${T}display" "list-keys${T}-T${T}root${T}x"
case_ bind-default-table "bind${T}x${T}display" "list-keys${T}-T${T}prefix${T}x"
case_ bind-T-beats-n "bind${T}-n${T}-T${T}zz${T}x${T}display" "list-keys${T}-T${T}zz" "list-keys${T}-T${T}root${T}x"
case_ bind-rebind "bind${T}-T${T}zz${T}x${T}display${T}a" "bind${T}-T${T}zz${T}x${T}display${T}b" "list-keys${T}-T${T}zz"
case_ bind-note "bind${T}-N${T}my note${T}-T${T}zz${T}x${T}display" "list-keys${T}-N${T}-T${T}zz"
case_ bind-note-existing "bind${T}-T${T}zz${T}x${T}display" "bind${T}-N${T}later${T}-T${T}zz${T}x" "list-keys${T}-N${T}-T${T}zz"
case_ bind-repeat-existing "bind${T}-T${T}zz${T}x${T}display" "bind${T}-r${T}-T${T}zz${T}x" "list-keys${T}-T${T}zz"
case_ bind-nocmd-new "bind${T}-T${T}zz${T}x" "list-keys${T}-T${T}zz"
case_ bind-nocmd-empty-table "bind${T}-T${T}zz${T}x" "unbind${T}-T${T}zz${T}x"
case_ bind-rebind-resets-note "bind${T}-N${T}n1${T}-r${T}-T${T}zz${T}x${T}display" "bind${T}-T${T}zz${T}x${T}display" "list-keys${T}-N${T}-a${T}-T${T}zz" "list-keys${T}-T${T}zz"
case_ bind-argv-semicolons "bind${T}-T${T}zz${T}x${T}display${T}a${T};${T}display${T}b" "bind${T}-T${T}zz${T}y${T}display${T}a;" "bind${T}-T${T}zz${T}z${T}display${T}a\\;" "list-keys${T}-T${T}zz"
case_ bind-argv-string "bind${T}-T${T}zz${T}x${T}display a ; display b" "bind${T}-T${T}zz${T}y${T}display -p 'a b'" "bind${T}-T${T}zz${T}z${T}no-such-command" "bind${T}-T${T}zz${T}w${T}display -Z" "list-keys${T}-T${T}zz"
case_ bind-argv-string-env "bind${T}-T${T}zz${T}x${T}display \$HOME ~" "list-keys${T}-T${T}zz"
case_ bind-argv-multi "bind${T}-T${T}zz${T}x${T}display${T}-p${T}a b${T}c" "bind${T}-T${T}zz${T}y${T}display${T}-Z" "bind${T}-T${T}zz${T}z${T}nosuch${T}x" "list-keys${T}-T${T}zz"
case_ bind-argv-alias "bind${T}-T${T}zz${T}x${T}splitp${T}-h" "bind${T}-T${T}zz${T}y${T}send${T}-X${T}cancel" "bind${T}-T${T}zz${T}z${T}info" "list-keys${T}-T${T}zz"
case_ bind-unknown-key "bind${T}nosuchkey${T}display"
case_ bind-none "bind${T}None${T}display"
case_ bind-f13 "bind${T}F13${T}display"
case_ bind-missing "bind"
case_ bind-any "bind${T}-T${T}zz${T}Any${T}display" "list-keys${T}-T${T}zz"
case_ bind-flags "bind${T}-X${T}x${T}display"
case_ bind-N-needs-arg "bind${T}-N"
case_ bind-T-needs-arg "bind${T}-T"
case_ unbind-missing "unbind"
case_ unbind-missing-q "unbind${T}-q"
case_ unbind-key-a "unbind${T}-a${T}x"
case_ unbind-key-a-q "unbind${T}-aq${T}x"
case_ unbind-a-nonexistent "unbind${T}-a${T}-T${T}nope"
case_ unbind-a-nonexistent-q "unbind${T}-aq${T}-T${T}nope"
# (After `unbind -a` removes `prefix`, a later `list-keys -N` crashes the
# reference server: cmd_list_keys_get_root_and_prefix hands a NULL table to
# sort_get_key_bindings_table. vtmux does not copy that; see keys.rs.)
case_ unbind-a-prefix "unbind${T}-a" "list-keys${T}-T${T}prefix" "list-keys${T}-T${T}root${T}-1"
case_ unbind-a-n "unbind${T}-an" "list-keys${T}-T${T}root" "list-keys${T}-T${T}prefix${T}C-b"
case_ unbind-a-T "unbind${T}-a${T}-T${T}copy-mode" "list-keys${T}-T${T}copy-mode"
case_ unbind-T-nonexistent "unbind${T}-T${T}nope${T}x"
case_ unbind-T-nonexistent-q "unbind${T}-qT${T}nope${T}x"
case_ unbind-none "unbind${T}None"
case_ unbind-none-q "unbind${T}-q${T}None"
case_ unbind-unknown "unbind${T}nosuchkey"
case_ unbind-unbound "unbind${T}x" "list-keys${T}-T${T}prefix${T}x"
case_ unbind-default "unbind${T}C-b" "list-keys${T}-N${T}-T${T}prefix${T}C-b" "list-keys${T}-T${T}prefix${T}-1"
case_ unbind-n "unbind${T}-n${T}MouseDown1Pane" "list-keys${T}-T${T}root${T}MouseDown1Pane"
case_ unbind-last-removes-table "bind${T}-T${T}zz${T}x${T}display" "unbind${T}-T${T}zz${T}x" "list-keys${T}-T${T}zz"
case_ unbind-default-table-stays "unbind${T}-T${T}move${T}1" "unbind${T}-T${T}move${T}2" "list-keys${T}-T${T}move${T}-N"
case_ unbind-table-then-listing "unbind${T}-a${T}-T${T}move" "list-keys${T}-T${T}move" "list-keys${T}-N"

# ---- config text -----------------------------------------------------------------
# Each case is config text; the reference sources it as a file, and the
# result is the error (without its `file:line: ` prefix) or the bindings in
# table t.
: > "$OUT/config.txt"
cfg() {
    local name=$1 text=$2
    fresh
    printf '%s\n' "$text" > "$WORK/c.conf"
    local err
    err=$(t source-file "$WORK/c.conf" 2>&1) || true
    err=$(printf '%s\n' "$err" | sed -e "s|^$WORK/c.conf:[0-9]*: ||")
    {
        echo "### $name"
        printf '%s\n' "$text"
        echo '---'
        if [ -n "$err" ]; then
            printf '%s\n' "$err" | sed -e 's/^/! /'
        fi
        t list-keys -T t -F '#{key_string}|#{key_command}' 2>&1 || true
        echo '%%'
    } >> "$OUT/config.txt"
}
cfg block-multiline $'bind -T t a {\n  display a\n  display b\n}'
cfg block-semicolons $'bind -T t a { display a ; display b\ndisplay c }'
cfg block-comment $'bind -T t a { display a # comment\ndisplay b }'
cfg block-trailing-space $'bind -T t a { display a ; display b } '
cfg block-if-two $'bind -T t a if -F 1 { display a\n display b } { display c }'
cfg block-if-lines $'bind -T t a if -F 1 {\ndisplay a\n\ndisplay b\n} {\ndisplay c\n}'
cfg block-if-semi $'bind -T t a if -F 1 { display a ; display b } { display c }'
cfg block-if-three $'bind -T t a if -F 1 { display a\n display b ; display c\n display d } { display e }'
cfg block-in-display $'bind -T t a display { a }'
cfg block-empty-command $'bind -T t a { display }'
cfg block-empty $'bind -T t a {}'
cfg block-empty-space $'bind -T t a { }'
cfg block-only-semi $'bind -T t a { ; }'
cfg block-assign $'bind -T t a { A=1 }'
cfg block-assign-use $'bind -T t a { A=1\ndisplay $A }'
cfg block-in-set $'bind -T t a set @x { a }'
cfg block-nested-bad $'bind -T t a { { display } }'
cfg block-nested-arg $'bind -T t a { if -F 1 { display a } { display b } }'
cfg block-nested-deep $'bind -T t a { if -F 1 { if -F 1 { display a\ndisplay b } } }'
cfg block-nested-unknown $'bind -T t a { display { x } }'
cfg block-tight $'bind -T t a {display}'
cfg block-tight-close $'bind -T t a { display a}'
cfg block-extra-close $'bind -T t a { display a }}'
cfg block-unterminated $'bind -T t a { display a '
cfg block-unterminated-lines $'bind -T t a { display a\ndisplay b'
cfg block-semi-forms $'bind -T t a { display a;display b;; display c}'
cfg block-bare-close $'bind -T t a display }'
cfg block-close-in-word $'bind -T t a display a}'
cfg block-close-quoted $'bind -T t a display "a}"'
cfg block-close-escaped $'bind -T t a display a\\}'
cfg block-open-in-word $'bind -T t a display a{b'
cfg block-open-quoted $'bind -T t a display \'{\''
cfg block-open-escaped $'bind -T t a display \\{'
cfg block-open-dangling $'bind -T t a { display { }'
cfg block-then-more-statement $'bind -T t a { display a } ; bind -T t b { display b }'
cfg block-then-word $'bind -T t a { display a } display b'
cfg block-two-blocks $'bind -T t a { display a } { display b }'
cfg block-as-command-name $'{ display a }'
cfg block-after-command-then-fail $'display { a }'
cfg block-bind-in-block $'bind -T t a { bind -T t b display }'
cfg block-run-C $'bind -T t a run -C { display a }'
cfg block-run-no-C $'bind -T t a run { display a }'
cfg block-if-shell-first $'bind -T t a if { display a } { display b }'
cfg block-if-shell-third $'bind -T t a if -F 1 { display a } { display b } { display c }'
cfg block-if-shell-string $'bind -T t a if -F 1 "display a" "display b"'
cfg block-flag-value $'bind -T t a display -t { a }'
cfg block-after-dashdash $'bind -T t a if -F 1 -- { display a }'
cfg block-set-value $'bind -T t a set -g @y { display a }'
cfg block-set-name $'bind -T t a set { display a } b'
cfg block-display-menu $'bind -T t a display-menu -T title "Name" n { display a } "" "Other" o { display b }'
cfg block-display-menu-sep $'bind -T t a display-menu { x }'
cfg block-display-menu-key-block $'bind -T t a display-menu "Name" { x }'
cfg block-display-menu-sep-then $'bind -T t a display-menu "" { x }'
cfg block-display-menu-sep-block $'bind -T t a display-menu "" "Name" k { display a }'
cfg block-command-prompt $'bind -T t a command-prompt -p x { rename-session -- "%%" }'
cfg block-command-prompt-string $'bind -T t a command-prompt -p x "rename-session -- %%"'
cfg block-confirm $'bind -T t a confirm-before -p "kill? (y/n)" { kill-window }'
cfg block-choose-tree $'bind -T t a choose-tree -Zs { switch-client -t "%%" }'
cfg block-line-groups $'bind -T t a {\ndisplay a\ndisplay b ; display c\ndisplay d\n} ; display e'
cfg block-line-groups-2 $'bind -T t a if -F 1 { display a ; display b\ndisplay c ; display d } { display e ; display f\ndisplay g }'
cfg block-line-groups-continuation $'bind -T t a if -F 1 { display a \\\n display b\ndisplay c } { }'
cfg block-line-groups-quotes $'bind -T t a if -F 1 { display "a\nb"\ndisplay c } { }'
cfg block-line-groups-comments $'bind -T t a if -F 1 {\n# c\ndisplay a\n# d\ndisplay b } { }'
cfg block-line-groups-comment-after $'bind -T t a if -F 1 { display a # x\ndisplay b } { }'
cfg block-line-groups-comment-after-2 $'bind -T t a if -F 1 { display a ; display b # x\ndisplay c } { }'
cfg block-line-groups-blank $'bind -T t a if -F 1 { display a\n\n\ndisplay b } { }'
cfg block-line-groups-crlf $'bind -T t a if -F 1 { display a\r\ndisplay b } { }'
cfg block-line-groups-semi-eol $'bind -T t a if -F 1 { display a ;\ndisplay b } { }'
cfg block-line-groups-assign $'bind -T t a if -F 1 { A=1\ndisplay a\nB=2 ; display b } { }'
cfg block-line-groups-hidden $'bind -T t a if -F 1 { %hidden A=1\ndisplay a } { }'
cfg block-alias $'bind -T t a { splitp -h\ninfo }'
cfg block-alias-in-argv $'bind -T t a if -F 1 { splitp } { choose-window }'
cfg print-flags-regroup $'bind -T t a choose-tree -sZ -O name'
cfg print-flags-regroup-2 $'bind -T t a choose-tree -Zs -O name -F x -f y'
cfg print-flags-repeat $'bind -T t a send -X -N 3 -l foo'
cfg print-flags-values-order $'bind -T t a new-window -n x -e A=1 -c y -e B=2 -d'
cfg print-flags-glued $'bind -T t a resize-pane -D5 -U3 -x 20% -y10'
cfg print-flags-optional $'bind -T t a resize-pane -D'
cfg print-flags-optional-then $'bind -T t a resize-pane -D -D5'
cfg print-flags-optional-after $'bind -T t a resize-pane -D5 -D'
cfg print-flags-optional-two $'bind -T t a resize-pane -D -U'
cfg print-flags-optional-dash $'bind -T t a resize-pane -D --'
cfg print-flags-optional-with-flag $'bind -T t a resize-pane -D -Z'
cfg print-flags-optional-with-flag-2 $'bind -T t a resize-pane -Z -D'
cfg print-flags-optional-neg $'bind -T t a resize-pane -D -5'
cfg print-flags-optional-neg-2 $'bind -T t a resize-pane -D-5'
cfg print-flags-optional-mixed $'bind -T t a move-pane -D -U5 -L'
cfg print-flags-dashdash $'bind -T t a display -p -- -x'
cfg print-flags-dash $'bind -T t a display -p -'
cfg print-flags-count $'bind -T t a send -XX -l -l'
cfg print-flags-glued-bool $'bind -T t a new-window -dPk'
cfg print-alias-send $'bind -T t a send -X cancel'
cfg print-alias-lots $'bind -T t a neww \\; splitw \\; killp \\; selectp -L \\; has \\; lsw'
cfg print-alias-prefix $'bind -T t a displayp'
cfg print-alias-abbrev $'bind -T t a new-w'
cfg print-alias-ambiguous $'bind -T t a new'
cfg print-alias-default-split $'bind -T t a split-pane -h'
cfg print-alias-default-server $'bind -T t a server-info'
cfg print-alias-default-choose $'bind -T t a choose-session'
cfg print-home $'bind -T t a display "$HOME"'
cfg print-home-var $'bind -T t a display $HOME/x'
cfg print-home-braces $'bind -T t a display ${HOME}x'
cfg print-home-tilde $'bind -T t a display ~/x'
cfg print-home-tilde-alone $'bind -T t a display ~'
cfg print-home-tilde-quoted $'bind -T t a display "~/x"'
cfg print-home-tilde-single $'bind -T t a display \'~/x\''
cfg print-home-tilde-user $'bind -T t a display ~nosuchuserxyz/x'
cfg print-var-unset $'bind -T t a display "[$NOSUCH_VAR_XYZ]"'
cfg print-var-dollar-end $'bind -T t a display "a$"'
cfg print-var-dollar-digit $'bind -T t a display "$1" \'$1\' $2'
cfg print-var-dollar-sq $'bind -T t a display \'$HOME\''
cfg print-var-dollar-escaped $'bind -T t a display \\$HOME'
cfg print-quote-dollar-in-dq $'bind -T t a display "pw=$r"'
cfg print-quote-dollar-sq $'bind -T t a display \'pw=#{pane_width}; r=$((pw * 100 / ww)); x\''
cfg print-quote-dollar-sq-2 $'bind -T t a run-shell \'pw=#{pane_width}; ww=#{window_width}; r=$((pw * 100 / ww)); if [ $r -lt 40 ]; then tmux resize-pane -x $((ww / 2)); fi\''
cfg print-quote-dollar-letter $'bind -T t a display \'a $b c\' \'a $_ c\' \'a ${b} c\' \'a $1 c\' \'a $ c\' \'a $\' \'$a b\''
cfg print-quote-dollar-trailing $'bind -T t a display \'a b $\' \'a b$\' \'a b$$\''
cfg print-quote-space $'bind -T t a display "a b" "a  b" " a" "a " " "'
cfg print-quote-single-char $'bind -T t a display ";" "#" "\'" "\\"" "%" "{" "}" "~" "$" " " "\\\\" "a" "-"'
cfg print-quote-single-char-2 $'bind -T t a display \\; \\# \\\' \\" \\% \\{ \\} \\~ \\$ \\\\'
cfg print-quote-special $'bind -T t a display "a;b" "a#b" "a\'b" "a%b" "a{b" "a}b" "a~b" "~a" "~a b" "a\\"b" "a\\"b c" "a\\\\b" "a\\\\b c"'
cfg print-quote-special-2 $'bind -T t a display \'a"b\' \'a b"c\' \'"\' \'""\' \'" "\' "\'" "\'\'" "a\'" "\'a"'
cfg print-quote-empty $'bind -T t a display "" \'\' ""'
cfg print-quote-empty-flag $'bind -T t a display -F "" x'
cfg print-quote-empty-flag-2 $'bind -T t a display -F \'\''
cfg print-quote-tilde $'bind -T t a display \\~ \\~a \'~\' \'x~\' "a ~b"'
cfg print-quote-tilde-dq $'bind -T t a display "\\~a b" "\\~ b" "~"'
cfg print-quote-control $'bind -T t a display "a\\tb" "a\\nb" "a\\rb" "a\\eb" "a\\ab" "a\\bb" "a\\fb" "a\\vb" "a\\001b" "a\\177b" "a\\037b"'
cfg print-quote-control-quoted $'bind -T t a display "a b\\tc" "a b\\nc" "a b\\033c" "a b\\001c"'
cfg print-quote-nul $'bind -T t a display "a\\000b" "a\\0001b"'
cfg print-quote-utf8 $'bind -T t a display é "é é" "日本語" "🙂" "a é" \'é é\''
cfg print-quote-backslash $'bind -T t a display "a\\\\b" "\\\\" \'a\\b\' \'a\\\\b\' "\\\\\\\\"'
cfg print-quote-backslash-spaced $'bind -T t a display "a \\\\ b" \'a \\ b\' "a\\sb"'
cfg print-quote-hash-formats $'bind -T t a display "#{pane_id}" "#[bold]x" \'#S\' "#{?#{==:#{a},b},x,y}"'
cfg print-quote-format-flags $'bind -T t a display -F "#{pane_id}" -t "{last}" x'
cfg print-quote-percent $'bind -T t a command-prompt -I "%1" "%%" %% %1'
cfg print-quote-equals $'bind -T t a display A=1 "A=1" -p a=b'
cfg print-quote-dash $'bind -T t a display -- -- - -a'
cfg print-semicolon-word $'bind -T t a display \\;'
cfg print-semicolon-escaped-in-quote $'bind -T t a display "a;" ";b" "\\;"'
cfg print-numbers $'bind -T t a send -N3 -t:.+ x'
cfg print-targets $'bind -T t a select-pane -t:.+ \\; select-window -t:=0 \\; switch-client -t= \\; swap-window -t@'
cfg print-long $'bind -T t a display-menu -T "#[align=centre]#{pane_index} (#{pane_id})" -x P -y P "Go" g { send -X history-top } "" "Other" o { send -X history-bottom }'
cfg print-empty-after-flag $'bind -T t a new-window -n ""'
cfg print-spaces-in-flag-value $'bind -T t a new-window -n "a b" -c "/x y"'
cfg print-flag-value-escape $'bind -T t a display -F "#{a};b" -c "$HOME"'
cfg print-flag-value-tilde $'bind -T t a new-window -c "~/x" -n ~'
cfg bind-note-quote $'bind -T t -N "a note; with \\"stuff\\"" a display'
cfg bind-note-utf8 $'bind -T t -N "é 日" a display'
cfg bind-r $'bind -T t -r a display'
cfg bind-r-after $'bind -T t a display\nbind -T t -r b display'
cfg bind-n-flag $'bind -n -T t a display'
cfg bind-note-repeat-nocmd $'bind -T t a display\nbind -T t -N later -r a'
cfg bind-error-in-middle $'bind -T t a display\nbind -T t b nosuchcommand\nbind -T t c display'
cfg bind-error-key $'bind -T t a display\nbind -T t nosuchkey display'
cfg bind-error-flag $'bind -T t a display -Z'
cfg bind-error-args $'bind -T t a display a b'
cfg bind-error-too-few $'bind'
cfg bind-error-syntax $'bind -T t a display }'
cfg bind-error-late-syntax $'bind -T t a display\nbind -T t b }'
cfg bind-error-unknown-cmd-in-block $'bind -T t a { nosuch }'
cfg bind-error-flag-in-block $'bind -T t a { display -Z }'
cfg bind-error-usage $'bind -T t a display -?'
cfg bind-error-arg-block-too-many $'bind -T t a display a { b }'
cfg bind-key-string-error $'bind -T t a "display \\" "'
cfg bind-key-command-alias $'bind -T t a info'
cfg unbind-in-config $'bind -T t a display\nbind -T t b display\nunbind -T t a'
cfg unbind-in-config-all $'bind -T t a display\nunbind -a -T t'
cfg continuation-in-bind $'bind -T t a display \\\n  -p x'
cfg line-comment-after-bind $'bind -T t a choose-tree -s  # comment'
cfg two-binds-semicolon $'bind -T t a display a ; bind -T t b display b'
cfg bind-owner-reload $'bind -T t R source-file "$HOME/.tmux.conf" \\; display-message "Sourced $HOME/.tmux.conf"'
cfg bind-owner-picker $'bind -T t M run-shell "$HOME/.config/tmux/scripts/move-pane-picker \'#{pane_id}\'"'
cfg bind-owner-pw $'bind -T t -r r run-shell \'pw=#{pane_width}; ww=#{window_width}; r=$((pw * 100 / ww)); if [ $r -lt 40 ]; then tmux resize-pane -x $((ww / 2)); elif [ $r -lt 60 ]; then tmux resize-pane -x $((ww * 2 / 3)); else tmux resize-pane -x $((ww / 3)); fi\''
cfg bind-owner-swap $'bind -T t -r S-Left select-pane -m \\; select-pane -L \\; swap-pane \\; select-pane -M'
cfg bind-owner-split $'bind -T t \'"\' split-window -v -c "#{pane_current_path}"'
cfg bind-owner-popup $'bind -T t C-g display-popup -E -w 60% -h 80% "$HOME/.config/tmux/scripts/cheatsheet"'
cfg bind-owner-clipboard $'bind -T t v run-shell -b "$HOME/.tmux/plugins/tmux-fzf/scripts/clipboard.sh"'

echo "wrote $(wc -l < "$OUT/names.txt") names, $(grep -c '^###' "$OUT/cases.txt") cases, $(grep -c '^###' "$OUT/config.txt") config cases"
