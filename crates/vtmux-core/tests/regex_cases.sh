#!/usr/bin/env bash
# Regenerate glibc regcomp/regexec goldens (REG_EXTENDED, REG_NEWLINE off):
# bash crates/vtmux-core/tests/regex_cases.sh > crates/vtmux-core/src/regex/golden.in
# Isolated process: tmux.c:403 sets LC_CTYPE to en_US.UTF-8 or C.UTF-8, leaving
# LC_COLLATE at C. ctypes owns the regex_t storage; no FFI enters the Rust core.
set -euo pipefail
REF="${VTMUX_REFERENCE_TMUX:-${XDG_CACHE_HOME:-$HOME/.cache}/vtmux/tmux-3.8/bin/tmux}"
env -i PATH=/usr/bin:/bin HOME=/ SHELL=/bin/sh LANG=C.UTF-8 REF="$REF" CASE_KIND="${1:-regex}" python3 - <<'PY'
import ctypes, json, os, subprocess
libc = ctypes.CDLL('libc.so.6')
libc.setlocale(0, b'en_US.UTF-8') or libc.setlocale(0, b'C.UTF-8')
class RM(ctypes.Structure):
    _fields_ = [('so', ctypes.c_int), ('eo', ctypes.c_int)]
def rust(s):
    return json.dumps(s, ensure_ascii=False)
def capture(p, t, icase):
    storage = ctypes.create_string_buffer(256)
    if libc.regcomp(storage, p.encode(), 1 | (2 if icase else 0)):
        return 'Err(Error::Syntax)'
    matches = (RM * 10)()
    rc = libc.regexec(storage, t.encode(), 10, matches, 0)
    libc.regfree(storage)
    if rc: return 'Ok(None)'
    entries = ['None' if m.so < 0 else f'Some({m.so}..{m.eo})' for m in matches]
    return 'Ok(Some(vec![' + ', '.join(entries) + ']))'
cases = [
('', ''), ('', 'abc'), ('(|x)(.*)', 'xy'), ('(|a)a*', 'aa'),
('(|[^a]+)\\w+', 'ba'), ('|()', ''), ('(()|a)*', 'a'),
('(a)|\\1', 'a'), ('(a)|b\\1', 'a'), ('|(){2}', ''), ('(|())b{0}|', 'ab'),
('(){1,2}|\\w{1,2}\\1{1,2}', 'abaa'), ('(|(b{0}){0,1}(|){1,2})*', ''), ('a', 'bba'), ('a', 'bbb'), ('a|ab', 'ab'),
('(a|ab)', 'ab'), ('ab|a', 'ab'), ('a|aa|aaa', 'aaa'),
('(a*)(a*)', 'aaa'), ('(ab|a)(bc|c)?', 'abc'),
('(a|ab)(c|bcd)(d*)', 'abcd'), ('(a|b)*', 'abab'),
('((a)|(b))*', 'ab'), ('(a*)*', 'aab'), ('(a*)+', 'b'),
('(a?)*', 'aa'), ('(a|ab)*c', 'abac'), ('(a*)*b', 'aab'),
('(a+)(b+)?', 'aab'), ('(a)|(b)', 'b'), ('()', 'x'),
('a{2}', 'baaa'), ('a{2,3}', 'aaaa'), ('a{2,}', 'aaaa'),
('a{,2}', 'aaa'), ('a{0}', 'a'), ('(a){0}', 'a'),
('a{1}{2}', 'aaa'), ('a**', 'aaa'), ('a+?', 'aaa'),
('a?', 'bbb'), ('a+', 'baa'), ('a*', 'baa'),
('^a', 'ab'), ('^a', 'ba'), ('a$', 'ba'), ('a$', 'ab'),
('^$', ''), ('^$', 'a'), ('a.b', 'a\nb'), ('[^a]', '\n'),
('^b', 'a\nb'), ('a$', 'a\nb'), ('.', 'é'), ('.*', '世界'),
('é+', 'ééa'), ('[a-z]+', '12abc9'), ('[^a-z]+', 'abc12!'),
('[]a]+', ']aa'), ('[a-]+', '-aa'), ('[-a]+', 'aa-'),
('[[:alpha:]]+', 'é中a1'), ('[[:digit:]]+', 'ab123x'),
('[[:alnum:]_]+', 'é_1!'), ('[[:upper:]]+', 'ABCé'),
('[[:lower:]]+', 'éabcZ'), ('[[:punct:]]+', 'a!?b'),
('[[:space:]]+', 'a \t\nb'), ('[[:blank:]]+', 'a\tb'),
('[[:graph:]]+', 'é a'), ('[[:print:]]+', 'é a'),
('[[:cntrl:]]', '\t'), ('[[:xdigit:]]+', 'fFg'),
('[[.a.]]', 'a'), ('[[=a=]]', 'a'), ('[é]', 'é'),
('\\w+', 'é_1-'), ('\\W+', 'a, b'), ('\\s+', 'a \t b'),
('\\S+', 'ab cd'), ('\\bfoo\\b', 'a foo b'),
('a\\B', 'aé'), ('\\<é', 'é'), ('é\\>', 'é'),
('\\B', '  '), ('\\`a', 'ab'), ("a\\'", 'ba'),
('(.)\\1', 'abbc'), ('(a*)\\1', 'aaaa'),
('(a|b)\\1', 'ab'), ('(a)(b)\\2\\1', 'abba'),
('()\\1', 'a'), ('((a)|b)*\\2', 'aba'),
('\\(a\\)', '(a)'), ('a\\+', 'a+'), ('a)', 'a)'),
('|a', 'a'), ('a||b', 'c'), ('a{0,0}b', 'b'),
('[', 'a'), ('[a', 'a'), ('[z-a]', 'a'), ('[[:foo:]]', 'a'),
('(', 'a'), ('*a', 'a'), ('a{2,1}', 'a'), ('a{32768}', 'a'),
('a{', 'a'), ('a{x}', 'a'), ('\\', 'a'), ('^*', 'a'),
('(a)\\2', 'aa'), ('\\1(a)', 'aa'), ('(a\\1)', 'aa'),
('[à-é]', 'é'), ('[[.space.]]', ' '), ('[a-c-e]', 'e'),
]
if os.environ['CASE_KIND'] == 'format':
    # bash tests/regex_cases.sh format > src/format/matching_golden.in
    sock = 'vtmux-regex-' + str(os.getpid())
    def t(*args):
        return subprocess.check_output([os.environ['REF'], '-L', sock, '-f', '/dev/null', *args], cwd='/').decode().removesuffix('\n')
    try:
        t('start-server', ';', 'set', '-g', 'exit-empty', 'off')
        t('set', '-g', '@v', 'ababa')
        t('set', '-g', '@inner', '#{@v}')
        formats = ['#{m:a*,abc}', '#{m:a*,ba}', '#{m/i:A*,abc}', '#{m:A*,abc}',
            '#{m/r:a|ab,ab}', '#{m/r:^.$,é}', '#{m/r:^b,a\\nb}',
            '#{m/ri:É,é}', '#{m/r:(,ab}', '#{m:??,é}', '#{m:*,a/b}',
            '#{m:[[:alpha:]],é}', '#{m/p:ab,xaxb}', '#{m/z:ab,xaxb}',
            '#{m/p:ab,xabxab}', '#{m/p:ab,aab}', '#{m/z:AB,ab}',
            '#{m/z:ab,AB}', '#{m/p:ab,AB}', '#{m/p:a b,axb}',
            '#{m/p:ab|x,axb}', '#{m/p:^ab,abx}', '#{m/p:ab$,xabab}',
            '#{m/z:!ab,axb}', '#{m/z:!ab,xab}', '#{m/p:é,xé}',
            '#{m/p:世,x世界}', '#{m/p:a,#[align=right]ab}',
            '#{m/p:b,#[align=centre]abc}', '#{m/p:a,#[fg=red]ab}',
            '#{m/p:,abc}', '#{m/z:,}', '#{m/p:!x,abc}',
            '#{s/a|ab/X/:@v}', '#{s/(a)(b)/[\\\\2\\\\1]/:@v}',
            '#{s/A/X/i:@v}', '#{s/A/X/r:@v}', '#{s/^a/X/:@v}',
            '#{s/a$/X/:@v}', '#{s/x*/-/ :@v}', '#{s/x*/-/:@v}',
            '#{s/a*/X/:@v}', '#{s/(a*)/\\\\1/:@v}', '#{s/(a*)/\\\\2/:@v}',
            '#{s/a/&/:@v}', '#{s/a/X/;s/X/Y/:@v}', '#{s/a/X/;=2:@v}',
            '#{s/a/X/;n:@v}', '#{s/a/X/;E:@inner}', '#{s//X/:@v}',
            '#{s/[/X/:@v}', '#{s/a/:@v}', '#{s/a/X/:@missing}',
            'x#{m:a}y', '#{m;==:a,a}', '#{==;m:a*,abc}',
            '#{s/a/X/;l:ababa}', '#{s/(a*)*/[\\\\1]/;l:aa}',
            '#{s/$/X/;l:abc}', '#{s/^/X/;l:abc}', '#{s/b*/X/;l:abc}',
            '#{s/a*/X/;l:abc}', '#{s/.*/X/;l:abc}', '#{s/(a)/\\\\0/;l:a}',
            '#{s/(a)/\\\\9/;l:a}', '#{s/(a)/\\\\1/;l:a}', '#{s/(a?)/\\\\1/;l:b}']
        print('// Generated by bash tests/regex_cases.sh format; isolated reference tmux 3.8.')
        print('#[allow(clippy::too_many_lines)]')
        print('fn rows() -> Vec<(&\'static str, &\'static str)> { vec![')
        for fmt in formats:
            t('set', '-gF', '@out', fmt)
            expected = t('show', '-gv', '@out')
            print(f'({rust(fmt)}, {rust(expected)}), // set -gF @out {fmt!r}; show -gv @out')
        print('] }')
    finally:
        t('kill-server')
    raise SystemExit
if os.environ['CASE_KIND'] == 'fnmatch':
    # bash crates/vtmux-core/tests/regex_cases.sh fnmatch > src/regex/fnmatch_golden.in
    print('// Generated by tests/regex_cases.sh fnmatch (glibc fnmatch, flags 0/FNM_CASEFOLD).')
    print('#[allow(clippy::unicode_not_nfc)]')
    print('fn rows() -> Vec<(&\'static str, &\'static str, bool, bool)> { vec![')
    pairs = [('a*','abc'), ('*',''), ('?','é'), ('??','é'), ('???','é'),
        ('?','世'), ('??','世'), ('???','世'), ('*','a/b'), ('*','.a'),
        ('a?b','a\nb'), ('a*','ba'), ('[a-z]','b'), ('[z-a]','m'),
        ('[!a]','b'), ('[^a]','a'), ('[]a]',']'), ('[a-]','-'),
        ('[a','[a'), ('[a','a'), ('[','['), ('\\\\','\\\\'),
        ('\\\\*','*'), ('[\\\\]]',']'), ('[[:alpha:]]','é'),
        ('[[:digit:]]','٣'), ('[[:upper:]]','a'), ('[[:lower:]]','A'),
        ('[[:foo:]]','a'), ('[[.a.]]','a'), ('[[=a=]]','a'),
        ('[à-é]','è'), ('[α-ω]','β'), ('[é]','É'), ('[é][é]','é'),
        ('A','a'), ('É','é'), ('Σ','ς'), ('k','K'), ('İ','i'),
        ('[A-z]','_'), ('[a-Z]','B'), ('[a-c-e]','-'), ('?*?','é'),
        ('[!a][!a]','é'), ('[[:alpha:]][[:alpha:]]','é')]
    for p,t in pairs:
        for ic in [False, True]:
            result = libc.fnmatch(p.encode(), t.encode(), 16 if ic else 0) == 0
            print(f'({rust(p)}, {rust(t)}, {str(ic).lower()}, {str(result).lower()}),')
    print('] }')
    raise SystemExit
print('// Generated by bash crates/vtmux-core/tests/regex_cases.sh (glibc regcomp/regexec).')
print('#[allow(clippy::too_many_lines, clippy::unicode_not_nfc)]')
print('fn rows() -> Vec<Row> { vec![')
for p,t in cases:
    for flag in [False, True]:
        print(f'({rust(p)}, {rust(t)}, {str(flag).lower()}, {capture(p,t,flag)}),')
for p,t in [('É','é'), ('Σ','ς'), ('K','K'), ('İ','i'), ('ı','I'),
            ('[[:upper:]]','a'), ('[^a]','A'), ('(a)\\1','aA'), ('[a-Z]','B')]:
    print(f'({rust(p)}, {rust(t)}, true, {capture(p,t,True)}),')
print('] }')
PY
