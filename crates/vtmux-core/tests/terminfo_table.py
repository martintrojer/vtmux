#!/usr/bin/env python3
"""Regenerates crates/vtmux-core/src/term_features/terminfo.rs, the terminals
vtmux knows without reading terminfo itself (D8, D-007):
    python3 crates/vtmux-core/tests/terminfo_table.py

For each terminal in the database it reads the terminfo entry the way
tmux's client does (tty-term.c `tty_term_read_list`: `tigetstr`, `tigetnum`,
`tigetflag` for every name of `tty_term_codes`; a standard boolean is there
even when false, so `am` and `bce` always are) and records:
  names    the `tty_term_codes` names the entry has (`#{I/c:name}`)
  off      the flags among them that are false
  colours  `colors`
  strings  the values of STRINGS, the capabilities vtmux writes or decodes,
           with padding removed as `tty_term_strip` does
Each terminal is read in a child process (curses' setupterm runs once).

Needs a Python whose curses is ncurses 6.x (on Meta hosts /usr/bin/python3, not
the python3 on PATH), `infocmp`, a compiled copy of the ncurses terminfo database
in the one-letter layout ($VTMUX_TERMINFO), and the reference source
($VTMUX_REFERENCE_SRC, default ~/.cache/vtmux/tmux-3.8-src). Every name in
that database is a terminal: tmux accepts any it can read (`setupterm`).
Build the database from ncurses' terminfo.src (6.6 for the checked-in table):
    tic -x -o $VTMUX_TERMINFO terminfo.src
The entries of CARRIED replace or add to what the database has. Entries ncurses'
`setupterm` refuses (`gn`, `hc`) are generic (left out) or HARDCOPY. One with a byte
above 0x7f in a string vtmux keeps (the 8-bit C1 terminals: xterm-8bit,
vt220-8bit, ibmpc, qnx...) is left out: the table holds UTF-8 text."""
import json, os, re, subprocess, sys
from concurrent.futures import ThreadPoolExecutor

HERE = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.join(HERE, '..', 'src', 'term_features', 'terminfo.rs')
SRC = os.environ.get('VTMUX_REFERENCE_SRC') or os.path.join(
    os.environ.get('XDG_CACHE_HOME', os.path.expanduser('~/.cache')), 'vtmux/tmux-3.8-src')
DB = os.environ.get('VTMUX_TERMINFO')

# The 68 entries of the first generation, kept as it read them (CentOS 9:
# ncurses 6.2 with ncurses-term, kitty's and Ghostty's own terminfo files),
# in the form main() builds below. The goldens (term_features_cases.py) and
# the oracle scenarios were checked against those, ncurses 6.6 differs for
# some (`iterm2`, `kitty`, `vte-256color`: `Smulx`; Ghostty's `Setulc`), and
# ncurses 6.6 has neither urxvt's entries nor Ghostty's and kitty's own files.
# `foot` and `foot-direct` were stand-ins then; the database has them now.
CARRIED = os.path.join(HERE, 'terminfo_carried.json')

STRINGS = ['clear', 'smcup', 'rmcup', 'smkx', 'rmkx', 'cnorm', 'civis', 'E3', 'ind', 'tsl', 'fsl',
           'Ms', 'Eneks', 'Dseks'] + ['kf%d' % i for i in range(1, 13)] + \
    ['kich1', 'kdch1', 'khome', 'kend', 'knp', 'kpp', 'kcbt'] + \
    ['cvvis', 'smso', 'Ss', 'Se', 'Cs', 'Cr', 'Smulx', 'Setulc', 'Smol']


def database():
    """Every terminal in the database: its files and links."""
    found = set()
    for d in os.listdir(DB):
        if len(d) == 1 and os.path.isdir(os.path.join(DB, d)):
            found.update(os.listdir(os.path.join(DB, d)))
    return found


_CODES = []


def codes():
    if _CODES:
        return _CODES
    text = open(os.path.join(SRC, 'tty-term.c')).read()
    body = text[text.index('tty_term_codes[] = '):]
    body = body[:body.index('\n};')]
    _CODES.extend(re.findall(r'\{ TTYCODE_(\w+), "([^"]+)" \}', body))
    return _CODES


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
    env = dict(os.environ, TERMINFO_DIRS=DB)
    res = subprocess.run([sys.executable, '-c', child, name, json.dumps(codes())],
                         capture_output=True, text=True, stdin=subprocess.DEVNULL, check=True, env=env)
    caps = json.loads(res.stdout)
    if caps is not None:
        return caps
    # `setupterm` refused it: ncurses does for a generic (`gn`) terminal and
    # for a hardcopy (`hc`) one (lib_setup.c), tmux says which
    # (tty_term_read_list).
    src = subprocess.run(['infocmp', '-x', '-1', name], capture_output=True, text=True, env=env,
                         stdin=subprocess.DEVNULL, check=True).stdout
    flags = {line.strip().rstrip(',') for line in src.splitlines() if line.startswith('\t')}
    for flag in ('gn', 'hc'):
        if flag in flags:
            return flag
    sys.exit('%s: refused by setupterm, and neither gn nor hc' % name)


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
    if not DB:
        sys.exit('set VTMUX_TERMINFO to a compiled ncurses terminfo database (see the docstring)')
    all_codes = codes()
    order = [cap for _, cap in all_codes]
    have = database()
    carried = json.load(open(CARRIED))
    terminals = sorted(have | set(carried))
    with ThreadPoolExecutor(16) as pool:
        read_all = dict(zip(sorted(have), pool.map(read, sorted(have))))
    entries, generic, hardcopy, eight_bit = [], [], [], []
    for name in terminals:
        note = None
        if name in carried:
            c = carried[name]
            names_, off, colours = c['names'], c['off'], c['colours']
            strings = [tuple(kv) for kv in c['strings']]
        else:
            caps = read_all[name]
            if caps in ('gn', 'hc'):
                (generic if caps == 'gn' else hardcopy).append(name)
                continue
            names_ = [c for c in order if c in caps]
            off = [c for c in names_ if caps[c][0] == 'f' and caps[c][1] == 0]
            colours = caps['colors'][1] if 'colors' in caps else None
            strings = [(c, strip(caps[c][1])) for c in STRINGS if c in caps]
        if any(ord(ch) >= 0x80 for _, v in strings for ch in v):
            eight_bit.append(name)
            continue
        entries.append((name, names_, off, colours, strings, note))

    consts = []
    # The table is big, so it is stored small: `names` is a bit set over ALL, and
    # a terminal's strings are indices into one pool of (capability, value),
    # in a list shared by every terminal with the same ones.
    index = {cap: i for i, (kind, cap) in enumerate(k for k in all_codes if k[0] != 'NONE')}
    words = (len(index) + 63) // 64
    pool, pooled = [], {}
    lists, listed = [], {}
    print('%d terminals; left out: %d with 8-bit bytes in a kept string, %d generic, %d hardcopy'
          % (len(entries), len(eight_bit), len(generic), len(hardcopy)), file=sys.stderr)
    rows = []
    for name, names_, off, colours, strings, note in entries:
        bits = [0] * words
        for c in names_:
            bits[index[c] // 64] |= 1 << (index[c] % 64)
        n = '[%s]' % ', '.join('0x' + '_'.join(re.findall('....', '%016x' % w)) for w in bits)
        ids = []
        for kv in strings:
            if kv not in pooled:
                pooled[kv] = len(pool)
                pool.append(kv)
            ids.append(pooled[kv])
        key = tuple(ids)
        if key not in listed:
            listed[key] = 'STRINGS_%s' % ident(name)
            while listed[key] in lists:
                listed[key] += '_'
            lists.append(listed[key])
            consts.append('#[rustfmt::skip]\nconst %s: &[u16] = &[%s];\n' % (
                listed[key], ', '.join(str(i) for i in ids)))
        s_ = listed[key]
        o = '&[%s]' % ', '.join('"%s"' % c for c in off)
        doc = '    // %s\n' % note if note else ''
        rows.append('%s    ("%s", Entry { names: %s, off: %s, colours: %s, strings: %s }),\n' % (
            doc, name, n, o, 'None' if colours is None else 'Some({:_})'.format(colours), s_))
    assert len(pool) < 1 << 16
    consts.append('#[rustfmt::skip]\npub(super) const POOL: &[(Cap, &str)] = &[%s];\n' % ', '.join(
        '("%s", %s)' % (k, rust_str(v)) for k, v in pool))
    with open(OUT, 'w') as f:
        f.write('''//! Generated by `tests/terminfo_table.py`; do not edit. The terminals vtmux
//! knows (D8, D-007): what `tty_term_read_list` (tty-term.c) reads from each
//! one's terminfo entry, as far as vtmux uses it.

/// A capability name.
type Cap = &'static str;

/// The `tty_term_codes` names an entry has: bit `i` is `ALL[i]`.
pub(super) type Names = [u64; %d];

/// An entry with no names.
pub(super) const NO_NAMES: Names = [0; %d];

/// One terminal's entry.
pub(super) struct Entry {
    /// The `tty_term_codes` names it has.
    pub(super) names: Names,
    /// The flags among them that are false.
    pub(super) off: &'static [Cap],
    /// `colors`.
    pub(super) colours: Option<u32>,
    /// The strings vtmux writes or decodes, padding removed (`tty_term_strip`):
    /// indices into `POOL`.
    pub(super) strings: &'static [u16],
}

impl Entry {
    /// Whether the entry has the capability.
    pub(super) fn has(&self, name: &str) -> bool {
        ALL.iter()
            .position(|(n, _)| *n == name)
            .is_some_and(|i| self.names[i / 64] >> (i %% 64) & 1 == 1)
    }

    /// The names it has, in `tty_term_codes` order.
    pub(super) fn names(&self) -> impl Iterator<Item = Cap> + '_ {
        ALL.iter()
            .enumerate()
            .filter(|(i, _)| self.names[i / 64] >> (i %% 64) & 1 == 1)
            .map(|(_, (n, _))| *n)
    }

    /// The strings it keeps, by capability name.
    pub(super) fn strings(&self) -> impl Iterator<Item = (Cap, &'static str)> + '_ {
        self.strings.iter().map(|&i| POOL[usize::from(i)])
    }
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

/// The terminals ncurses' `setupterm` refuses as hardcopy (`hc`): tmux's
/// client says `can't use hardcopy terminal` (`tty_term_read_list`). The
/// generic ones (`gn`: `unknown`, `ibm327x`) are left out of the table, so they
/// are `missing or unsuitable terminal` like any name without an entry.
pub(super) const HARDCOPY: &[&str] = &[%s];

''' % (words, words, ', '.join('("%s", Kind::%s)' % (c, k.capitalize()) for k, c in all_codes if k != 'NONE'),
       ''.join(rows),
       ', '.join('"%s"' % n for n in hardcopy)))
        f.write('\n'.join(consts))
    subprocess.run(['rustfmt', '--edition', '2024', OUT], check=True)


if __name__ == '__main__':
    main()
