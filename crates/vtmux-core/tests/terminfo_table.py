#!/usr/bin/env python3
"""Regenerates crates/vtmux-core/src/term_features/terminfo.rs, the terminals
vtmux knows without reading terminfo itself (D8, D-007):
    python3 crates/vtmux-core/tests/terminfo_table.py

For each terminal in TERMINALS it reads the host's terminfo entry the way
tmux's client does (tty-term.c `tty_term_read_list`: `tigetstr`, `tigetnum`,
`tigetflag` for every name of `tty_term_codes`; a standard boolean is there
even when false, so `am` and `bce` always are) and records:
  names    the `tty_term_codes` names the entry has (`#{I/c:name}`)
  off      the flags among them that are false
  colours  `colors`
  strings  the values of STRINGS, the capabilities vtmux writes or decodes,
           with padding removed as `tty_term_strip` does
Each terminal is read in a child process (curses' setupterm runs once).

Needs Python's curses on an ncurses with the terminals' entries
(TERMINFO_DIRS: ncurses-term for the less common ones, Ghostty's terminfo
for xterm-ghostty, kitty's for xterm-kitty) and the reference source
($VTMUX_REFERENCE_SRC, default ~/.cache/vtmux/tmux-3.8-src). A terminal the
database lacks takes the entry ALIASES names, and the table says so."""
import json, os, re, subprocess, sys

HERE = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.join(HERE, '..', 'src', 'term_features', 'terminfo.rs')
SRC = os.environ.get('VTMUX_REFERENCE_SRC') or os.path.join(
    os.environ.get('XDG_CACHE_HOME', os.path.expanduser('~/.cache')), 'vtmux/tmux-3.8-src')

# What a tmux user's TERM is likely to be: terminal emulators' own entries,
# the console, nested multiplexers, serial terminals.
TERMINALS = '''
alacritty alacritty-direct dumb foot foot-direct ghostty gnome gnome-256color
iterm2 kitty kitty-direct konsole konsole-256color konsole-direct linux
linux-16color mintty ms-terminal putty putty-256color rxvt rxvt-16color
rxvt-256color rxvt-88color rxvt-unicode rxvt-unicode-256color screen
screen-16color screen-256color screen.Eterm screen.gnome screen.konsole
screen.konsole-256color screen.linux screen.linux-s screen.mlterm
screen.mlterm-256color screen.mrxvt screen.putty screen.putty-256color
screen.rxvt screen.teraterm screen.vte screen.vte-256color
screen.xterm-256color screen.xterm-new screen.xterm-r6 screen.xterm-xfree86
st st-256color st-direct tmux tmux-256color tmux-direct vt100 vt102 vt220
vte vte-256color vte-direct xterm xterm-16color xterm-256color xterm-88color
xterm-color xterm-direct xterm-ghostty xterm-kitty xterm-mono xterm-new
'''.split()

# Stand-ins for terminals a host's database may lack (foot's entry came to
# ncurses in 6.3; its terminfo is xterm-derived).
ALIASES = {'foot': 'xterm-256color', 'foot-direct': 'xterm-direct'}

STRINGS = ['clear', 'smcup', 'rmcup', 'smkx', 'rmkx', 'cnorm', 'civis', 'E3', 'ind', 'tsl', 'fsl',
           'Ms', 'Eneks', 'Dseks'] + ['kf%d' % i for i in range(1, 13)] + \
    ['kich1', 'kdch1', 'khome', 'kend', 'knp', 'kpp', 'kcbt'] + \
    ['cvvis', 'smso', 'Ss', 'Se', 'Cs', 'Cr', 'Smulx', 'Setulc', 'Smol']


def codes():
    text = open(os.path.join(SRC, 'tty-term.c')).read()
    body = text[text.index('tty_term_codes[] = '):]
    body = body[:body.index('\n};')]
    return re.findall(r'\{ TTYCODE_(\w+), "([^"]+)" \}', body)


def strip(s):
    """tty_term_strip."""
    if '$' not in s:
        return s
    out, i = [], 0
    while i < len(s):
        if s[i] == '$' and s[i + 1:i + 2] == '<':
            while i < len(s) and s[i] != '>':
                i += 1
            if i < len(s) and s[i] == '>':
                i += 1
            if i >= len(s):
                break
        out.append(s[i])
        i += 1
    return ''.join(out)


def read(name):
    """One terminal's entry, in a child: setupterm runs once per process."""
    child = r'''
import curses, json, sys
name, codes = sys.argv[1], json.loads(sys.argv[2])
try:
    curses.setupterm(name, 1)
except curses.error:
    print('null'); sys.exit()
out = {}
for kind, cap in codes:
    if kind == 'STRING':
        v = curses.tigetstr(cap)
        if v is not None:
            out[cap] = ['s', v.decode('latin-1')]
    elif kind == 'NUMBER':
        n = curses.tigetnum(cap)
        if n not in (-1, -2):
            out[cap] = ['n', n]
    elif kind == 'FLAG':
        n = curses.tigetflag(cap)
        if n != -1:
            out[cap] = ['f', n]
print(json.dumps(out))
'''
    res = subprocess.run([sys.executable, '-c', child, name, json.dumps(codes())],
                         capture_output=True, text=True, stdin=subprocess.DEVNULL, check=True)
    return json.loads(res.stdout)


def rust_str(s):
    out = []
    for c in s:
        o = ord(c)
        if c in '"\\':
            out.append('\\' + c)
        elif 0x20 <= o < 0x7f:
            out.append(c)
        elif o < 0x80:
            out.append('\\x%02x' % o)
        else:
            sys.exit('non-ASCII byte in a kept string: %r' % s)
    return '"' + ''.join(out) + '"'


def ident(name):
    return re.sub(r'[^A-Z0-9]', '_', name.upper())


def main():
    all_codes = codes()
    order = [cap for _, cap in all_codes]
    entries = []
    for name in TERMINALS:
        caps, note = read(name), None
        if caps is None:
            if name not in ALIASES:
                sys.exit('%s: not in this terminfo database' % name)
            caps = read(ALIASES[name])
            note = 'not in the generating host\'s terminfo: %s\'s entry' % ALIASES[name]
            assert caps is not None, ALIASES[name]
        names = [c for c in order if c in caps]
        off = [c for c in names if caps[c][0] == 'f' and caps[c][1] == 0]
        colours = caps['colors'][1] if 'colors' in caps else None
        strings = [(c, strip(caps[c][1])) for c in STRINGS if c in caps]
        entries.append((name, names, off, colours, strings, note, ALIASES.get(name, name)))

    # Shared lists get one constant, named after the first terminal with it.
    shared = {}
    consts = []
    def const(kind, items, owner, render):
        key = (kind, tuple(items))
        if key not in shared:
            shared[key] = '%s_%s' % (kind, ident(owner))
            consts.append('const %s: &[%s] = &[%s];\n' % (
                shared[key], 'Cap' if kind == 'NAMES' else '(&str, &str)',
                ', '.join(render(i) for i in items)))
        return shared[key]

    rows = []
    # Real entries name the constants, not the stand-ins.
    for name, names, off, colours, strings, note, owner in sorted(entries, key=lambda e: (e[5] is not None, e[0] not in ALIASES.values())):
        const('NAMES', names, owner, lambda c: '"%s"' % c)
        const('STRINGS', strings, owner, lambda kv: '("%s", %s)' % (kv[0], rust_str(kv[1])))
    for name, names, off, colours, strings, note, owner in entries:
        n = shared[('NAMES', tuple(names))]
        s = shared[('STRINGS', tuple(strings))]
        o = '&[%s]' % ', '.join('"%s"' % c for c in off)
        doc = '    // %s\n' % note if note else ''
        rows.append('%s    ("%s", Entry { names: %s, off: %s, colours: %s, strings: %s }),\n' % (
            doc, name, n, o, 'None' if colours is None else 'Some({:_})'.format(colours), s))

    with open(OUT, 'w') as f:
        f.write('''//! Generated by `tests/terminfo_table.py`; do not edit. The terminals vtmux
//! knows (D8, D-007): what `tty_term_read_list` (tty-term.c) reads from each
//! one's terminfo entry, as far as vtmux uses it.

/// A capability name.
type Cap = &'static str;

/// One terminal's entry.
pub(super) struct Entry {
    /// The `tty_term_codes` names it has.
    pub(super) names: &'static [Cap],
    /// The flags among them that are false.
    pub(super) off: &'static [Cap],
    /// `colors`.
    pub(super) colours: Option<u32>,
    /// The strings vtmux writes or decodes, padding removed (`tty_term_strip`).
    pub(super) strings: &'static [(Cap, &'static str)],
}

/// Every name in `tty_term_codes` with its type: an override naming
/// anything else changes nothing (`tty_term_apply`).
pub(super) const ALL: &[(Cap, Kind)] = &[%s];

/// A `tty_term_codes` type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    String,
    Number,
    Flag,
}

/// The terminals, by name.
#[rustfmt::skip]
pub(super) const TERMINALS: &[(&str, Entry)] = &[
%s];

''' % (', '.join('("%s", Kind::%s)' % (c, k.capitalize()) for k, c in all_codes if k != 'NONE'),
       ''.join(rows)))
        f.write('\n'.join(consts))
    subprocess.run(['rustfmt', '--edition', '2024', OUT], check=True)


if __name__ == '__main__':
    main()
