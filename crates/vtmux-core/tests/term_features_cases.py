#!/usr/bin/env python3
"""Regenerates src/term_features/golden/cases.tsv from the reference tmux 3.8:
    python3 crates/vtmux-core/tests/term_features_cases.py
(the inputs are the first five columns of the existing file; pass another
file of such rows as the argument to regenerate from it, and a second path to
write the result there instead of over the existing file: merge by hand).

Each row is `TERM COLORTERM -T-args features overrides`, `-` for none:
  COLORTERM  the client's $COLORTERM
  -T-args    `-T` arguments joined with a space
  features   `terminal-features[10+i]` items joined with `~`
  overrides  `terminal-overrides[10+i]` items joined with `~`
Per row, isolated (env -u TMUX -u TMUX_PANE, -L, -f /dev/null, SHELL=/bin/sh):
  new-session -d -s keep cat; set -s terminal-features[10+i] ...;
  set -s terminal-overrides[10+i] ...; then attach under a PTY with
  `-T args`, and
  list-clients -F '#{client_termfeatures}|#{client_colours}|' followed by
  `#{?#{Ic:NAME},NAME ,}` for each of Tc RGB setrgbf setrgbb Sync Smulx Setulc
  Ss Se Ms tsl fsl Eneks Dseks (the capabilities the client's terminal ended up with).
The server is never left empty: the `keep` session lives until kill-server.
xterm-ghostty needs Ghostty's terminfo ($GHOSTTY_TERMINFO), and the rows for
terminals outside ncurses' base set (kitty, alacritty, ...) need ncurses-term and
kitty's terminfo there too. Some `xterm*:Ms=...` rows do not attach under a
reference linked to CentOS 9's ncurses 6.2 (`xterm*:Ms=%p3%s`): there, regenerate
other rows by passing a file of just those rows and merging the result.
A terminal tmux refuses gives `!` and the client's message (`!can't use
hardcopy terminal: tty33`, `!open terminal failed: terminal does not support
clear`) where the other columns would be. $VTMUX_CASE_JOBS rows run at once
(default 8), each on its own server."""
import atexit, fcntl, os, pty, select, shutil, struct, subprocess, sys, tempfile, termios, time
from concurrent.futures import ThreadPoolExecutor

HERE = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.join(HERE, '..', 'src', 'term_features', 'golden', 'cases.tsv')
REF = os.environ.get('VTMUX_REFERENCE_TMUX') or os.path.join(
    os.environ.get('XDG_CACHE_HOME', os.path.expanduser('~/.cache')), 'vtmux/tmux-3.8/bin/tmux')
# A private socket dir: a server leaves its socket file behind, and the default
# dir is the owner's /tmp/tmux-<uid>.
SOCKDIR = tempfile.mkdtemp(prefix='vtmux-sock.', dir='/tmp')
atexit.register(shutil.rmtree, SOCKDIR, ignore_errors=True)
TERMINFO = os.environ.get('GHOSTTY_TERMINFO',
                          os.path.expanduser('~/.local/opt/ghostty-1.4.0/share/terminfo'))
CODES = 'Tc RGB setrgbf setrgbb Sync Smulx Setulc Ss Se Ms tsl fsl Eneks Dseks'.split()
FMT = '#{client_termfeatures}|#{client_colours}|' + ''.join(
    '#{?#{Ic:%s},%s ,}' % (c, c) for c in CODES)


def run(term, colorterm, targs, feats, ovs, sock):
    env = {'TERMINFO_DIRS': TERMINFO, 'PATH': '/usr/bin:/bin', 'HOME': '/tmp',
           'SHELL': '/bin/sh', 'TERM': term, 'LC_ALL': 'C.UTF-8', 'TMUX_TMPDIR': SOCKDIR}
    if colorterm:
        env['COLORTERM'] = colorterm
    base = [REF, '-L', sock, '-f', '/dev/null']

    def t(*a):
        return subprocess.run(base + list(a), env=env, capture_output=True, text=True)
    t('kill-server')
    assert t('new-session', '-d', '-s', 'keep', 'cat').returncode == 0
    for name, items in (('terminal-features', feats), ('terminal-overrides', ovs)):
        for i, item in enumerate(items):
            assert t('set', '-s', '%s[%d]' % (name, 10 + i), item).returncode == 0
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 10, 40, 0, 0))
    ta = [x for a in targs for x in ('-T', a)]
    client = subprocess.Popen(base + ta + ['attach', '-t', 'keep'], env=env, stdin=slave,
                              stdout=slave, stderr=slave, close_fds=True, preexec_fn=os.setsid)
    for _ in range(100):
        time.sleep(0.05)
        if t('list-clients', '-F', 'x').stdout.strip():
            break
        if client.poll() is not None:
            # Refused, in the client or by the server: what it printed.
            said = b''
            while select.select([master], [], [], 0.2)[0]:
                try:
                    chunk = os.read(master, 4096)
                except OSError:
                    break
                if not chunk:
                    break
                said += chunk
            t('kill-server')
            os.close(master)
            os.close(slave)
            return '!' + said.decode().strip()
    else:
        t('kill-server')
        client.kill()
        client.wait()
        return 'ERR'
    out = t('list-clients', '-F', FMT).stdout.strip('\n')
    t('kill-server')
    client.kill()
    client.wait()
    os.close(master)
    os.close(slave)
    return out.strip()


def main():
    src = sys.argv[1] if len(sys.argv) > 1 else OUT
    rows = []
    for line in open(src):
        line = line.rstrip('\n')
        if not line or line.startswith('#'):
            continue
        term, ct, ta, fe, ov = line.split('\t')[:5] if '\t' in line else line.split('|')[:5]
        rows.append((term, ct, ta, fe, ov))
    dest = sys.argv[2] if len(sys.argv) > 2 else OUT
    jobs = int(os.environ.get('VTMUX_CASE_JOBS', '8'))

    def one(i):
        term, ct, ta, fe, ov = rows[i]
        out = run(term, None if ct == '-' else ct, [] if ta == '-' else ta.split(' '),
                  [] if fe == '-' else fe.split('~'), [] if ov == '-' else ov.split('~'),
                  'tfcases%d-%d' % (os.getpid(), i % jobs))
        assert out != 'ERR', rows[i]
        return '\t'.join(rows[i] + (out,)) + '\n'

    # One server label per worker: rows i, i + jobs, ... must not overlap.
    results = []
    with ThreadPoolExecutor(jobs) as pool:
        for lo in range(0, len(rows), jobs):
            results += list(pool.map(one, range(lo, min(lo + jobs, len(rows)))))
    with open(dest + '.new', 'w') as f:
        f.write('# Generated by tests/term_features_cases.py from the reference tmux 3.8.\n')
        f.writelines(results)
    os.replace(dest + '.new', dest)


main()
