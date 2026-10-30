#!/usr/bin/env bash
# Regenerates crates/vtmux-core/src/format/time_rows.in from the reference tmux 3.8:
#   crates/vtmux-core/tests/time_cases.sh > crates/vtmux-core/src/format/time_rows.in
# (included by the `golden` test module of format/time.rs; `cargo test -p vtmux-core time`).
#
# Each row is `(tz, now, ts, kind, template, expected)`: the reference ran with TZ=<tz>,
# `@ts` set to <ts>, and the wall clock reading <now> (the reference reads the real clock,
# so a row is generated inside one wall-clock second and retried if the second ticks;
# relative rows set @ts to now minus an age). `kind` is how it was expanded:
#   plain: `set -gF @out '<tpl>'; show -gv @out`    (format_expand, format.c:6925)
#   time:  `display-message -p -t s '<tpl>'`         (format_expand_time, format.c:6892)
# The Rust side expands plain rows with `expand_at` and time rows with `expand_time_at`,
# both over a FixedWallClock(now, tz). Other variables are the fixed table in `common()`
# below, mirrored by `common()` in time.rs.
# `set -gF` turns control characters into `_` when it stores an @option, so a row whose
# output has a newline or tab (`%n`, `%t`) is expanded with display-message instead
# (`tfrow`), the `%` doubled so the first strftime pass leaves it for `t/f`.
# Case strings must not contain a single quote or a newline. Variables: @ts, @fmt, @nested,
# @pct, @nopct, @zero and session_name=s, as `ctx()` in time.rs.
set -euo pipefail

REF="${VTMUX_REFERENCE_TMUX:-${XDG_CACHE_HOME:-$HOME/.cache}/vtmux/tmux-3.8/bin/tmux}"
SOCK="vtmux-time-cases-$$"
TZV=UTC
cd /
t() {
    env -i PATH=/usr/bin:/bin HOME=/ SHELL=/bin/sh LANG=C.UTF-8 TZ="$TZV" \
        "$REF" -L "$SOCK" -f /dev/null "$@"
}
trap 't kill-server 2>/dev/null || true' EXIT

rust_str() { python3 -c '
import sys
out = []
for ch in sys.argv[1]:
    if ch == "\\": out.append("\\\\")
    elif ch == "\"": out.append("\\\"")
    elif ord(ch) < 32 or ord(ch) == 127 or ord(ch) > 126: out.append("\\u{%x}" % ord(ch))
    else: out.append(ch)
sys.stdout.write("".join(out))
' "$1"; }

start() {
    TZV=$1
    t start-server \; set -g exit-empty off \; set -g update-environment ''
    t new-session -d -s s
    t set -g @fmt '%Y|%%|#S'
    t set -g @nested '#{T:@fmt}#{E:@fmt}'
    t set -g @pct '%H'
    t set -g @nopct 'plain'
    t set -g @zero 0
}

# ref KIND 'tpl' -> the reference's output (trailing newline removed).
ref() {
    local out
    if [ "$1" = plain ]; then
        t set -gF @out "$2" && out=$(t show -gv @out; echo x)
    else
        out=$(t display-message -p -t s "$2"; echo x)
    fi
    out=${out%x}
    # The trailing `x` survives the caller's `$(...)`, which would eat a newline the
    # expansion itself ends with (`%n`); `row` removes it.
    printf '%sx' "${out%$'\n'}"
}

# row TS_OR_AGE KIND 'tpl': `now` is read before and after; the row is kept only when both
# readings agree. TS_OR_AGE is `@N` (an age in seconds) or an absolute timestamp.
row() {
    local ts=$1 kind=$2 tpl=$3 now now2 abs out
    while :; do
        now=$(date +%s)
        case "$ts" in @*) abs=$((now - ${ts#@})) ;; *) abs=$ts ;; esac
        t set -g @ts "$abs"
        out=$(ref "$kind" "$tpl")
        out=${out%x}
        now2=$(date +%s)
        [ "$now" = "$now2" ] && break
    done
    printf '        ("%s", %s, %s, %s, "%s", "%s"),\n' "$TZV" "$now" "$abs" \
        "Kind::$([ "$kind" = plain ] && echo Plain || echo Time)" "$(rust_str "$tpl")" "$(rust_str "$out")"
}

# Instants (UTC): Thu 2026-10-29 16:53:03 (London: GMT), Fri 2026-06-20 (BST), the last
# second of 2025, Mon 2024-01-01, Fri 2021-01-01 (ISO week 53 of 2020), 2024-02-29,
# just after midnight, noon, year 5138 (years past 9999 are out of range, D-004).
TSS=(1793292783 1782000000 1767225599 1704067200 1609459200 1709164800 1793232001 1793275200 99999999999 86400 1)

# `t/f/<fmt>` over one instant. A conversion that prints a control character goes through
# display-message (see the header).
tfrow() {
    local f=$2
    case "$f" in
        *n | *t) row "$1" time "#{t/f/${f//\%/%%}:@ts}" ;;
        *) row "$1" plain "#{t/f/$f:@ts}" ;;
    esac
}

# Every conversion glibc's strftime knows in the C locale, then flags, widths, modifiers,
# and what it does not know.
CONVS=(a A b B c C d D e F g G h H I j k l m M n p P r R s S t T u U V w W x X y Y z Z %
    Ea EC Ex EX Ey EY Od Oe OH OI Om OM OS Ou OU OV Ow OW Oy Ob OB Oz EZ Es Ec Oc Ed EH Ee OD Of
    Q q J L N f i o v E O 5 ' ' + : 3N s)
FLAGS=(%-d %_d %0e %^a %^b %^B %#a %#p %#Z %#c %^c %#r %#x %5d %_5d %-5d %05d %5e %-e %5a %-5a %05a
    %10c %5Y %_5Y %-Y %6Y %_6Y %5y %5C %3j %-j %_j %-m %-H %_H %-I %_I %-M %_M %-S %-y %-C
    %10H %-10H %5U %5V %5G %5g %5u %5w %5s %_12s %012s %-s %010s %5z %-z %_z %10z %10Z %5Z %05Z
    %5p %5P %05p %5n %05t %5% %_5% %5Q %05Q %-Q %5 %-5 %5Ed %5Od %5EY %_5EY %5Oy %E5Y %Ez %Oz
    %15D %5D %05D %10F %^F %^x %^D %Oa %EB %Ob %^Ob %#OB %OP %Op %0% %E% %O% %^%
    %^_5a %_^5a %-_5d %_-5d %0-9d %-09d %00d %__d %2147483648d %999999d)

echo "// Generated by crates/vtmux-core/tests/time_cases.sh from tmux 3.8. Do not edit."
echo "// Rows: (tz, now, ts, kind, template, expected). See tests/time_cases.sh."
echo "#[allow(clippy::too_many_lines, clippy::unreadable_literal)]"
echo "fn rows() -> Vec<Row> {"
echo "    vec!["

for TZ in UTC Europe/London; do
    start "$TZ"
    # ctime and the modifiers over every instant.
    for ts in "${TSS[@]}"; do
        row "$ts" plain '#{t:@ts}'
        row "$ts" plain '#{t/p:@ts}'
        row "$ts" plain '#{t/r:@ts}'
        row "$ts" plain '#{t/d:@ts}'
        row "$ts" plain '#{t/f/%H#:%M#:%S %Z:@ts}'
        row "$ts" plain '#{t/f:@ts}'
    done
    for ts in 1793292783 1782000000 1767225599 1704067200 1609459200 1709164800 1793232001 1793275200 99999999999; do
        for c in "${CONVS[@]}"; do
            case "$c" in ' ') f='% ' ;; *) f="%$c" ;; esac
            tfrow "$ts" "$f"
        done
    done
    for ts in 1793292783 1782000000 1767225599; do
        for f in "${FLAGS[@]}"; do tfrow "$ts" "${f}"; done
        row "$ts" plain '#{t|f|%m/%d/%y %D:@ts}'
        row "$ts" plain '#{t|f|%a, %d %b %Y %H#:%M#:%S %z:@ts}'
    done

    # Modifier arguments and combinations (format.c:6147-6172).
    for ts in 1793292783 1782000000; do
        for m in 't/pr' 't/rp' 't/rd' 't/dp' 't/pd' 't/p/f/%H' 't/f/%H;t/r' 't/r;t/f/%H' 't/p;t/f/%H' 't/f/%H;t/f/%M' 't/f/%H;t' 't/x' 't/' 't/f/' 't/f/%H/extra' 't/F' 't/P' 't/R' 't/D'; do
            row "$ts" plain "#{$m:@ts}"
        done
        row "$ts" plain '#{t/f/#{@pct}#:%M:@ts}'
        row "$ts" plain '#{t/f/#{@pct}:@ts}'
        row "$ts" plain '#{t/f/%H#,%M:@ts}'
        row "$ts" plain '#{t/f/##%H:@ts}'
        row "$ts" plain '#{t/f/%H#}:@ts}'
        row "$ts" plain '#{t/f/#{?1,%H,x}:@ts}'
        row "$ts" plain '#{t/f/%H:@ts}x#{t:@ts}'
        row "$ts" plain '#{t:#{@ts}}'
        row "$ts" plain '#{t/p:#{@ts}}'
        row "$ts" plain '#{?#{t:@ts},yes,no}'
        row "$ts" plain '#{?t:@ts,yes,no}'
        row "$ts" plain '#{t:@ts}|#{t:@zero}|#{t:@nopct}|#{t:@nosuch}'
        row "$ts" plain '#{=5:t:@ts}'
        row "$ts" plain '#{=5:#{t:@ts}}'
        row "$ts" plain '#{n:t:@ts}'
        row "$ts" plain '#{b:t:@ts}'
        row "$ts" plain '#{q:t:@ts}'
        row "$ts" plain '#{s/ /_/:t:@ts}'
        row "$ts" plain '#{p30:t:@ts}'
    done

    # Relative and pretty times by age (seconds).
    for age in 0 1 59 60 61 3599 3600 3601 7199 7260 86399 86400 86401 90000 172800 604800 2419199 2419200 2592000 5184000 15552000 31535999 31536000 63072000 315360000; do
        for m in 't/r' 't/p' 't/d' 't/f/%H#:%M' 't'; do row "@$age" plain "#{$m:@ts}"; done
    done
    row "@-60" plain '#{t/r:@ts}'
    row "@-60" plain '#{t/p:@ts}'
    row "@-60" plain '#{t/d:@ts}'

    # Time-aware expansion (display-message): strftime runs over the whole template,
    # then over every nested expansion; plain expansion leaves `%` alone.
    for ts in 1793292783 1782000000; do
        for f in '%H' '%%H' '%%%H' '%%%%H' '%Y-%m-%d %H:%M:%S' '%a %b %e %Z %z' '%j %U %W %V %G' '%s' '%5Y' '%Q %J' '100%' '%' 'x%%y%%z' '#S %H #{session_name} %M' '%H:%M #{?1,%H,x}' '#{?1,%%H,x}' '#{?1,%%%%H,x}' '#{?%H,a,b}' '#{@fmt}' '#{E:@fmt}' '#{T:@fmt}' '#{T:@nested}' '#{E:@nested}' '#{E:@pct}' '#{T:@pct}' '#{T:@nopct}' '#{E:@nopct}' '#{T:@zero}' '#{T;E:@fmt}' '#{E;T:@fmt}' '#{T;T:@fmt}' '#{T:nosuch}' '#{l:%H}' '#{l:%%H}' '#{==:%H,%H}' '#{m:%H,%H}' '#{s/%H/x/:@pct}' '#{=2:%Y}' '#{e|+:%H,%M}' '#{R:%H,3}' '#{?#{==:%H,%H},y,n}' '#{t/f/%H:@ts} %H' '#{t/f/%%H:@ts}' '#{t:@ts} %H' '#[fg=red]%H#[default]' '##%H' '#{a:%H}' '#{c:%H}' '#{q:%H}' '#{w:%H}' '#{n:%H}' '#{b:%H}'; do
            row "$ts" time "$f"
            row "$ts" plain "$f"
        done
    done
    # The 8192-byte buffer of format_expand1 (format.c:6702) and the 512-byte one of t/f.
    for n in 8185 8190 8191 8192 8193; do
        body=$(python3 -c "print('x'*$n,end='')")
        row 1793292783 time "%H$body"
    done
    # `s[512]`: output of 511 bytes fits. A longer one makes strftime fail and the reference
    # prints the half-written stack buffer, which is not reproducible (D-004).
    for n in 505 509; do
        body=$(python3 -c "print('y'*$n,end='')")
        row 1793292783 plain "#{t/f/%H$body:@ts}"
    done
    row 1793292783 plain "#{t/f/%511d:@ts}"
    row 1793292783 time "%8190d"
    row 1793292783 time "%8191d"
    t kill-server
done

# Zones with odd offsets.
for TZ in America/St_Johns Asia/Kolkata Australia/Lord_Howe; do
    start "$TZ"
    for ts in 1793292783 1782000000; do
        for f in '%z' '%Z' '%5z' '%10z' '%-z' '%_z' '%#Z' '%c' '%s' '%H:%M' '%j %a'; do
            row "$ts" plain "#{t/f/$f:@ts}"
        done
        row "$ts" plain '#{t:@ts}'
        row "$ts" plain '#{t/p:@ts}'
    done
    t kill-server
done

echo "    ]"
echo "}"
