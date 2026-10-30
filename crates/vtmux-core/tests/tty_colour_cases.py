#!/usr/bin/env python3
"""Regenerates src/colour/tty_cases.tsv from the reference tmux 3.8:
    python3 crates/vtmux-core/tests/tty_colour_cases.py
For TERM in xterm (8 colours), xterm-16color, xterm-88color a pane prints every
case as `ESC[<params>m X ESC[0m y`; a client attached under a PTY (200 columns,
status off) is the observer, and the SGR state in force at each `X` is what
tty_check_fg / tty_check_bg (tty.c:2842-2960) left of it.
Row: `colours<TAB>params (space separated)<TAB>fg=<sgr> bg=<sgr>[ bold]` where
`-` is the default colour. Cases the core cannot tell apart are not asked:
`38;5;0-15` (ghostty stores them as the aixterm colours)."""
import fcntl, os, pty, re, struct, subprocess, tempfile, termios, time

HERE = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.join(HERE, '..', 'src', 'colour', 'tty_cases.tsv')
REF = os.environ.get('VTMUX_REFERENCE_TMUX') or os.path.join(
    os.environ.get('XDG_CACHE_HOME', os.path.expanduser('~/.cache')), 'vtmux/tmux-3.8/bin/tmux')
TERMS = [('xterm', 8), ('xterm-16color', 16), ('xterm-88color', 88)]


def cases():
    rgbs = [(r, g, b) for r in (0, 40, 100, 200, 255) for g in (0, 90, 135, 255)
            for b in (0, 50, 175, 255)] + [(8, 8, 8), (128, 128, 128), (238, 238, 238), (250, 250, 250),
                                           (1, 2, 3), (18, 18, 18), (95, 95, 95)]
    fg = [str(n) for n in range(30, 38)] + [str(n) for n in range(90, 98)]
    fg += ['38;5;%d' % n for n in range(16, 256)] + ['38;2;%d;%d;%d' % c for c in rgbs]
    bg = [str(n) for n in range(40, 48)] + [str(n) for n in range(100, 108)]
    bg += ['48;5;%d' % n for n in range(16, 256)] + ['48;2;%d;%d;%d' % c for c in rgbs]
    out = [[f] for f in fg] + [[b] for b in bg]
    out += [[f, b] for f in fg if f.startswith('38;5;') for b in ('40', '47', '100', '107')]
    out += [[f, b] for f in ('30', '37', '90', '97') for b in bg if b.startswith('4') and len(b) == 2 or b in ('100', '107')]
    return out


def sgr_state(data):
    state = {'fg': '-', 'bg': '-', 'bold': False}
    rows = []
    for m in re.finditer(rb'\x1b\[([0-9;]*)m|\x1b\(B|X|y', data):
        if m.group(0) == b'X':
            rows.append('fg=%s bg=%s%s' % (state['fg'], state['bg'], ' bold' if state['bold'] else ''))
        elif m.group(0) == b'y':
            pass
        elif m.group(1) is None or m.group(1) in (b'', b'0'):
            state.update(fg='-', bg='-', bold=False)
        else:
            p = m.group(1).decode().split(';')
            i = 0
            while i < len(p):
                n = int(p[i])
                if n in (38, 48):
                    k = 'fg' if n == 38 else 'bg'
                    cnt = 3 if p[i + 1] == '5' else 5
                    state[k] = ';'.join(p[i:i + cnt])
                    i += cnt
                    continue
                if n == 1:
                    state['bold'] = True
                elif n == 22:
                    state['bold'] = False
                elif n == 39:
                    state['fg'] = '-'
                elif n == 49:
                    state['bg'] = '-'
                elif 30 <= n <= 37 or 90 <= n <= 97:
                    state['fg'] = str(n)
                elif 40 <= n <= 47 or 100 <= n <= 107:
                    state['bg'] = str(n)
                else:
                    raise SystemExit('unexpected SGR %r' % m.group(0))
                i += 1
    return rows


def run(term, rows):
    env = {'PATH': '/usr/bin:/bin', 'HOME': '/tmp', 'SHELL': '/bin/sh', 'TERM': term, 'LC_ALL': 'C.UTF-8'}
    sock = 'ttycol%d' % os.getpid()
    base = [REF, '-L', sock, '-f', '/dev/null']

    def t(*a):
        return subprocess.run(base + list(a), env=env, capture_output=True, text=True)
    body = b''.join(b''.join(b'\x1b[%sm' % p.encode() for p in c) + b'X\x1b[0my' for c in rows)
    with tempfile.NamedTemporaryFile(delete=False) as f:
        f.write(body)
    t('kill-server')
    assert t('new-session', '-d', '-s', 'a', '-x', '200', '-y', '40', 'cat %s; exec cat' % f.name).returncode == 0
    t('set', '-g', 'status', 'off')
    time.sleep(0.5)  # the pane has printed everything before the client draws
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 40, 200, 0, 0))
    client = subprocess.Popen(base + ['attach', '-t', 'a'], env=env, stdin=slave, stdout=slave,
                              stderr=slave, close_fds=True, preexec_fn=os.setsid)
    buf = b''
    last = time.time()
    end = time.time() + 30
    import select
    while time.time() < end and time.time() - last < 1.5:
        r, _, _ = select.select([master], [], [], 0.1)
        if r:
            buf += os.read(master, 65536)
            last = time.time()
    t('kill-server')
    client.kill()
    client.wait()
    os.unlink(f.name)
    got = sgr_state(buf)
    if len(got) == 2 * len(rows):  # the client drew the screen twice: keep the last
        got = got[len(rows):]
    assert len(got) == len(rows), (term, len(got), len(rows))
    return got


def main():
    rows = cases()
    with open(OUT + '.new', 'w') as f:
        f.write('# Generated by tests/tty_colour_cases.py from the reference tmux 3.8.\n')
        for term, colours in TERMS:
            # 150 cases fit the 200x40 pane (2 cells each) per attach
            for i in range(0, len(rows), 1500):
                chunk = rows[i:i + 1500]
                for c, s in zip(chunk, run(term, chunk)):
                    f.write('%d\t%s\t%s\n' % (colours, ' '.join(c), s))
    os.replace(OUT + '.new', OUT)


if __name__ == "__main__":
    main()
