//! Command-line and config parsing.
//!
//! Two inputs, two behaviours (cmd-parse.y):
//!
//! - [`parse_argv`] is `cmd_parse_from_arguments` (1065-1136). The shell has
//!   already split the words, so nothing is quoted, escaped or commented: a
//!   word ending in `;` ends the command, a word ending in `\;` becomes a
//!   word ending in a literal `;`, `;` elsewhere in a word is literal.
//! - [`parse_config_with`] is the lexer and grammar for config files and
//!   command strings (`yylex`, 1262-1480, and the `%%` rules above it):
//!   quotes, backslash escapes, `$VAR` and `~` expansion, `#` comments, `;`
//!   separators and newlines.
//!
//! Both then run `cmd_parse_build_command` (886-940): expand a default
//! `command-alias`, find the command, parse its arguments. Everything is
//! parsed before anything runs: a bad command anywhere in the text rejects
//! the whole text, but an execution-time failure (`new -d -x abc`) never
//! shows up here.
//!
//! `NAME=value` and `%hidden NAME=value` are applied while parsing, as the
//! `assignment` rules do (cmd-parse.y:224-270): [`ConfigEnv::set`] is called
//! the moment the assignment is read, so a later `$NAME` in the same text,
//! even in the same statement, sees it, and assignments made before a later
//! error stay applied. [`ParseOpts::parse_only`] (`source-file -n`,
//! `CMD_PARSE_PARSEONLY`) skips the call.
//!
//! `{ ... }` is an argument (`argument: '{' argument_statements`): the
//! commands inside are parsed and built like the rest, into a nested
//! [`CommandList`] ([`ArgValue::Commands`]). Which positionals accept one is
//! per command (`Args::parse_values`). Commands also remember their source
//! line, so the list can be grouped the way `cmd_parse_build_commands` does
//! (a new group per line unless [`ParseOpts::one_group`]), which `;` versus
//! `;;` in printed blocks shows. The line a command gets is the line after
//! the lexer read the token that ended it, as bison's lookahead makes it:
//! a trailing `# comment` has already moved to the next line by then.
//!
//! Not handled yet and reported as [`CmdError::Unsupported`]: `%if` and
//! friends, and arguments that are not valid UTF-8 (`\377`, `\U00110000`).
//! Parse errors carry no `file:line:` prefix; the caller that knows the file
//! adds it.

use super::table::{DEFAULT_COMMAND_ALIASES, find};
use super::{ArgValue, Args, CmdError, CommandCall, CommandList};

/// What the lexer needs from the host. vtmux-core does no I/O: the server
/// implements this over its global environment and the system user database
/// (`getpwnam`, which goes through NSS and is not a scan of /etc/passwd).
pub trait ConfigEnv {
    /// `$VAR`: `environ_find(global_environ, name)` (cmd-parse.y:1584).
    /// Hidden variables count; an unset one expands to nothing.
    fn var(&self, name: &str) -> Option<String>;
    /// `~` and `~user` (`yylex_token_tilde`, cmd-parse.y:1640-1690). `None`
    /// is a bare `~`: a non-empty `HOME` in the global environment, else the
    /// home of the current uid (`getpwuid`). `Some(user)` is `getpwnam`.
    /// `None` back is a syntax error.
    fn home(&self, user: Option<&str>) -> Option<String>;
    /// `environ_put(global_environ, "NAME=value", hidden)`, called from the
    /// assignment rule unless the parse is parse-only (cmd-parse.y:224-270).
    fn set(&mut self, name: &str, value: &str, hidden: bool);
}

/// Options for [`parse_config_with`] (the `CMD_PARSE_*` flags that matter).
#[derive(Clone, Copy, Debug, Default)]
pub struct ParseOpts {
    /// `CMD_PARSE_PARSEONLY` (`source-file -n`): assignments are checked but
    /// not applied.
    pub parse_only: bool,
    /// `CMD_PARSE_ONEGROUP`: every command is in one group, whatever its
    /// line (`cmd_parse_from_string` sets it).
    pub one_group: bool,
}

/// The environment for text that cannot contain `$` or `~` (the default
/// command aliases, the default key bindings): every variable is unset.
pub struct NoEnv;

impl ConfigEnv for NoEnv {
    fn var(&self, _: &str) -> Option<String> {
        None
    }

    fn home(&self, _: Option<&str>) -> Option<String> {
        None
    }

    fn set(&mut self, _: &str, _: &str, _: bool) {}
}

/// One command as the grammar reads it, before it is looked up (`struct
/// cmd_parse_command`).
struct RawCommand {
    /// The lexer's line when the command was reduced.
    line: u32,
    args: Vec<RawArg>,
}

enum RawArg {
    Str(String),
    /// `{ ... }`, not built yet.
    Block(Vec<RawCommand>),
    /// An already parsed list (`cmd_parse_from_arguments` with a block).
    Built(CommandList),
}

/// Parse an argument vector as `tmux a b ; c d` receives it.
pub fn parse_argv(words: &[String]) -> Result<Vec<CommandCall>, CmdError> {
    let values: Vec<ArgValue> = words.iter().cloned().map(ArgValue::Str).collect();
    parse_argv_values(&values)
}

/// `cmd_parse_from_arguments` (cmd-parse.y:1065-1136): the same for values
/// that may already be `{ }` blocks (what `bind-key` passes on).
pub fn parse_argv_values(values: &[ArgValue]) -> Result<Vec<CommandCall>, CmdError> {
    let mut commands: Vec<RawCommand> = Vec::new();
    let mut current: Vec<RawArg> = Vec::new();
    let new = |args| RawCommand { line: 0, args };
    for value in values {
        let mut end = false;
        match value {
            ArgValue::Str(word) => {
                let mut word = word.clone();
                if word.ends_with(';') {
                    word.pop();
                    if word.ends_with('\\') {
                        // `\;` is a literal `;` (cmd-parse.y:1108-1110).
                        word.pop();
                        word.push(';');
                    } else {
                        end = true;
                    }
                }
                if !end || !word.is_empty() {
                    current.push(RawArg::Str(word));
                }
            }
            ArgValue::Commands(list) => current.push(RawArg::Built(list.clone())),
        }
        if end {
            commands.push(new(std::mem::take(&mut current)));
        }
    }
    if !current.is_empty() {
        commands.push(new(current));
    }
    build_commands(&commands, false, true)
}

/// Parse config text: one line, or several (a newline ends a statement
/// exactly as `;` does). `NAME=value` and `%hidden NAME=value` are applied to
/// `env` as they are read (see the module docs) and produce no command.
///
/// # Errors
/// The first syntax, lexer or command error in the text. Assignments read
/// before it stay applied.
pub fn parse_config_with(
    text: &str,
    env: &mut dyn ConfigEnv,
    opts: ParseOpts,
) -> Result<Vec<CommandCall>, CmdError> {
    let raw = lex_commands(text, env, opts)?;
    build_commands(&raw, opts.one_group, true)
}

/// `cmd_parse_from_string` (cmd-parse.y:1010-1027): text as a command
/// string, every command in one group (`{ a \n b }` is `a ; b`).
pub fn parse_string(text: &str, env: &mut dyn ConfigEnv) -> Result<CommandList, CmdError> {
    let opts = ParseOpts {
        parse_only: false,
        one_group: true,
    };
    Ok(CommandList {
        calls: parse_config_with(text, env, opts)?,
    })
}

/// `cmd_parse_build_commands` (cmd-parse.y:937-1005): build each command,
/// giving a new group to each run of commands from one line.
fn build_commands(
    cmds: &[RawCommand],
    one_group: bool,
    expand_alias: bool,
) -> Result<Vec<CommandCall>, CmdError> {
    let mut out = Vec::new();
    let mut group = 0;
    let mut line = None;
    for cmd in cmds {
        if line.is_none() || (!one_group && line != Some(cmd.line)) {
            group += 1;
        }
        line = Some(cmd.line);
        for mut call in build_command(cmd, one_group, expand_alias)? {
            call.group = group;
            out.push(call);
        }
    }
    Ok(out)
}

fn build_command(
    cmd: &RawCommand,
    one_group: bool,
    expand_alias: bool,
) -> Result<Vec<CommandCall>, CmdError> {
    if expand_alias {
        // cmd_parse_expand_alias (cmd-parse.y:820-880): no first word, or a
        // block as first word, is an empty list.
        let Some(RawArg::Str(first)) = cmd.args.first() else {
            return Ok(Vec::new());
        };
        if let Some((_, text)) = DEFAULT_COMMAND_ALIASES.iter().find(|(a, _)| a == first) {
            // Parse the alias text, append the remaining words to its last
            // command, and build without expanding again.
            let opts = ParseOpts {
                parse_only: true,
                one_group,
            };
            let mut expanded = lex_commands(text, &mut NoEnv, opts)?;
            let Some(last) = expanded.last_mut() else {
                return Ok(Vec::new());
            };
            last.args.extend(cmd.args[1..].iter().map(clone_raw));
            return build_commands(&expanded, one_group, false);
        }
    }
    // cmd_parse_build_command (cmd-parse.y:886-935).
    let mut values = Vec::with_capacity(cmd.args.len());
    for arg in &cmd.args {
        values.push(match arg {
            RawArg::Str(s) => ArgValue::Str(s.clone()),
            RawArg::Block(block) => ArgValue::Commands(CommandList {
                calls: build_commands(block, one_group, expand_alias)?,
            }),
            RawArg::Built(list) => ArgValue::Commands(list.clone()),
        });
    }
    // cmd_parse (cmd.c:508-540).
    let Some(ArgValue::Str(first)) = values.first() else {
        return Err(CmdError::Lex("no command"));
    };
    let entry = find(first)?;
    let args = Args::parse_values(entry, &values)?;
    Ok(vec![CommandCall {
        entry,
        args,
        group: 0,
    }])
}

fn clone_raw(arg: &RawArg) -> RawArg {
    match arg {
        RawArg::Str(s) => RawArg::Str(s.clone()),
        RawArg::Block(b) => RawArg::Block(
            b.iter()
                .map(|c| RawCommand {
                    line: c.line,
                    args: c.args.iter().map(clone_raw).collect(),
                })
                .collect(),
        ),
        RawArg::Built(l) => RawArg::Built(l.clone()),
    }
}

// ---------------------------------------------------------------------
// Lexer: a port of yylex and its helpers (cmd-parse.y:1180-1790).

enum Token {
    /// `\n`, or the newline tmux appends at the end of input.
    Eol,
    Semi,
    /// `{`: opens a block argument.
    LBrace,
    /// `}`: closes it.
    RBrace,
    /// A word (`TOKEN`) or `name=value` (`EQUALS`).
    Word {
        text: String,
        equals: bool,
    },
    /// `%hidden`: valid only at the start of a statement, before `NAME=value`.
    Hidden,
    /// `%if`, `%elif`, `%else`, `%endif`: a statement-level directive.
    /// Anywhere else tmux's grammar rejects it.
    Directive,
    End,
}

struct Lexer<'a> {
    input: &'a [u8],
    off: usize,
    /// Backslashes read but not yet returned (`ps->escapes`).
    escapes: u32,
    /// The synthetic final newline has been returned (`ps->eof`).
    eof: bool,
    env: &'a mut dyn ConfigEnv,
    opts: ParseOpts,
    /// `input->line`, as the lexer moves it (see the module docs).
    line: u32,
    /// A real newline was returned; the line moves on at the next token
    /// (`ps->eol`).
    eol: bool,
}

const MAX_NAME: usize = 1024 - 2;

fn is_var(ch: u8, first: bool) -> bool {
    if ch == b'=' || (first && ch.is_ascii_digit()) {
        return false;
    }
    ch.is_ascii_alphanumeric() || ch == b'_'
}

type Ch = Option<u8>;

impl Lexer<'_> {
    fn getc1(&mut self) -> Ch {
        let ch = self.input.get(self.off).copied();
        if ch.is_some() {
            self.off += 1;
        }
        ch
    }

    fn ungetc(&mut self, ch: Ch) {
        if ch.is_some() && self.off > 0 {
            self.off -= 1;
        }
    }

    /// `yylex_getc`: backslash-newline is a line continuation; every other
    /// backslash is handed back one at a time for the escape handler.
    fn getc(&mut self) -> Ch {
        if self.escapes != 0 {
            self.escapes -= 1;
            return Some(b'\\');
        }
        loop {
            let ch = self.getc1();
            if ch == Some(b'\\') {
                self.escapes += 1;
                continue;
            }
            if ch == Some(b'\n') && self.escapes % 2 == 1 {
                self.line += 1;
                self.escapes -= 1;
                continue;
            }
            if self.escapes != 0 {
                self.ungetc(ch);
                self.escapes -= 1;
                return Some(b'\\');
            }
            return ch;
        }
    }

    fn next_token(&mut self) -> Result<Token, CmdError> {
        if self.eol {
            self.line += 1;
            self.eol = false;
        }
        loop {
            let Some(mut ch) = self.getc() else {
                // Every input ends with a newline (cmd-parse.y:1292-1300).
                if self.eof {
                    return Ok(Token::End);
                }
                self.eof = true;
                return Ok(Token::Eol);
            };
            if ch == b' ' || ch == b'\t' {
                continue;
            }
            if ch == b'\r' {
                // \r\n is \n.
                let next = self.getc();
                if next == Some(b'\n') {
                    ch = b'\n';
                } else {
                    self.ungetc(next);
                }
            }
            match ch {
                b'\n' => {
                    self.eol = true;
                    return Ok(Token::Eol);
                }
                b';' => return Ok(Token::Semi),
                b'{' => return Ok(Token::LBrace),
                b'}' => return Ok(Token::RBrace),
                b'#' => {
                    // A comment to the end of the line (continuations
                    // included: it reads through getc).
                    let mut next = self.getc();
                    while next.is_some() && next != Some(b'\n') {
                        next = self.getc();
                    }
                    if next == Some(b'\n') {
                        // Unlike a bare newline, this one moves the line
                        // at once (cmd-parse.y:1330-1338).
                        self.line += 1;
                        return Ok(Token::Eol);
                    }
                }
                b'%' => {
                    // All `%` or digits is a word; else a directive
                    // (cmd-parse.y:1350-1390).
                    let word = self.get_word(b'%');
                    if word.iter().all(|c| *c == b'%' || c.is_ascii_digit()) {
                        return Ok(Token::Word {
                            text: bytes_to_string(word)?,
                            equals: false,
                        });
                    }
                    return match word.as_slice() {
                        b"%hidden" => Ok(Token::Hidden),
                        b"%if" | b"%else" | b"%elif" | b"%endif" => Ok(Token::Directive),
                        _ => Err(CmdError::Syntax),
                    };
                }
                _ => {
                    let token = self.token(ch)?;
                    // cmd-parse.y:1392-1403: NAME=... with a valid NAME.
                    let equals = token.contains(&b'=') && is_var(token[0], true) && {
                        let name_end = token[1..].iter().position(|c| !is_var(*c, false));
                        name_end.is_some_and(|i| token[1 + i] == b'=')
                    };
                    return Ok(Token::Word {
                        text: bytes_to_string(token)?,
                        equals,
                    });
                }
            }
        }
    }

    fn get_word(&mut self, first: u8) -> Vec<u8> {
        let mut buf = vec![first];
        let mut ch;
        loop {
            ch = self.getc();
            match ch {
                Some(c) if !matches!(c, b' ' | b'\t' | b'\n') => buf.push(c),
                _ => break,
            }
        }
        self.ungetc(ch);
        buf
    }

    /// `yylex_token`: one word with quotes, escapes, `$` and `~`.
    fn token(&mut self, first: u8) -> Result<Vec<u8>, CmdError> {
        #[derive(Clone, Copy, PartialEq)]
        enum State {
            Start,
            None,
            Double,
            Single,
        }
        let mut buf: Vec<u8> = Vec::new();
        let mut state = State::None;
        let mut last = State::Start;
        let mut ch: Ch = Some(first);
        while let Some(mut c) = ch {
            if state == State::None && c == b'\r' {
                let next = self.getc();
                if next == Some(b'\n') {
                    c = b'\n';
                } else {
                    self.ungetc(next);
                }
            }
            if c == b'\n' && state == State::None {
                break;
            }
            // Whitespace, `;` and `}` end a word outside quotes.
            if state == State::None && matches!(c, b' ' | b'\t' | b';' | b'}') {
                break;
            }
            // Space and comments after a newline inside quotes are removed;
            // the newline stays (cmd-parse.y:1676-1699).
            if c == b'\n' {
                self.line += 1;
                buf.push(b'\n');
                loop {
                    ch = self.getc();
                    if ch != Some(b' ') && ch != Some(b'\t') {
                        break;
                    }
                }
                if ch != Some(b'#') {
                    continue;
                }
                ch = self.getc();
                if ch.is_some_and(|n| b",#{}:".contains(&n)) {
                    self.ungetc(ch);
                    ch = Some(b'#');
                } else {
                    loop {
                        ch = self.getc();
                        if ch == Some(b'\n') || ch.is_none() {
                            break;
                        }
                    }
                }
                continue;
            }
            // `\`, `~` and `$` are expanded except in single quotes.
            let mut handled = true;
            if c == b'\\' && state != State::Single {
                self.escape(&mut buf)?;
            } else if c == b'~' && last != state && state != State::Single {
                self.tilde(&mut buf)?;
            } else if c == b'$' && state != State::Single {
                self.variable(&mut buf)?;
            } else {
                handled = false;
            }
            if handled {
                last = state;
                ch = self.getc();
                continue;
            }
            let quote = match c {
                b'\'' => Some(State::Single),
                b'"' => Some(State::Double),
                _ => None,
            };
            if let Some(q) = quote {
                if state == State::None {
                    state = q;
                    ch = self.getc();
                    continue;
                }
                if state == q {
                    state = State::None;
                    ch = self.getc();
                    continue;
                }
            }
            buf.push(c);
            last = state;
            ch = self.getc();
        }
        self.ungetc(ch);
        // C strings: a NUL (from `\000`) ends the word.
        if let Some(nul) = buf.iter().position(|b| *b == 0) {
            buf.truncate(nul);
        }
        Ok(buf)
    }

    /// `yylex_token_escape` (cmd-parse.y:1490-1600).
    fn escape(&mut self, buf: &mut Vec<u8>) -> Result<(), CmdError> {
        let Some(ch) = self.getc() else {
            return Err(CmdError::Syntax);
        };
        if (b'4'..=b'7').contains(&ch) {
            return Err(CmdError::Lex("invalid octal escape"));
        }
        if (b'0'..=b'3').contains(&ch) {
            let o2 = self.getc();
            if let Some(o2) = o2.filter(|c| (b'0'..=b'7').contains(c)) {
                let o3 = self.getc();
                if let Some(o3) = o3.filter(|c| (b'0'..=b'7').contains(c)) {
                    buf.push(64 * (ch - b'0') + 8 * (o2 - b'0') + (o3 - b'0'));
                    return Ok(());
                }
            }
            return Err(CmdError::Lex("invalid octal escape"));
        }
        let mapped = match ch {
            b'a' => 0x07,
            b'b' => 0x08,
            b'e' => 0x1b,
            b'f' => 0x0c,
            b's' => b' ',
            b'v' => 0x0b,
            b'r' => b'\r',
            b'n' => b'\n',
            b't' => b'\t',
            b'u' => return self.unicode(buf, 4, "invalid \\u argument"),
            b'U' => return self.unicode(buf, 8, "invalid \\U argument"),
            other => other,
        };
        buf.push(mapped);
        Ok(())
    }

    fn unicode(
        &mut self,
        buf: &mut Vec<u8>,
        size: usize,
        message: &'static str,
    ) -> Result<(), CmdError> {
        let mut value: u32 = 0;
        for _ in 0..size {
            let ch = self.getc();
            let Some(ch) = ch.filter(|c| *c != b'\n') else {
                return Err(CmdError::Syntax);
            };
            let Some(digit) = char::from(ch).to_digit(16) else {
                return Err(CmdError::Lex(message));
            };
            value = value << 4 | digit;
        }
        // wctomb: surrogates fail (the reference prints the message). Above
        // U+10FFFF glibc emits bytes that are not UTF-8, which an `Args`
        // string cannot hold.
        match char::from_u32(value) {
            Some(c) => {
                let mut tmp = [0; 4];
                buf.extend_from_slice(c.encode_utf8(&mut tmp).as_bytes());
                Ok(())
            }
            None if (0xd800..0xe000).contains(&value) => Err(CmdError::Lex(message)),
            None => Err(CmdError::Unsupported("non-UTF-8 argument")),
        }
    }

    /// `yylex_token_variable` (cmd-parse.y:1602-1660).
    fn variable(&mut self, buf: &mut Vec<u8>) -> Result<(), CmdError> {
        let Some(mut ch) = self.getc() else {
            return Err(CmdError::Syntax);
        };
        let mut name = Vec::new();
        let brackets = ch == b'{';
        if !brackets {
            if !is_var(ch, true) {
                buf.push(b'$');
                self.ungetc(Some(ch));
                return Ok(());
            }
            name.push(ch);
        }
        loop {
            let next = self.getc();
            if brackets && next == Some(b'}') {
                break;
            }
            match next {
                Some(c) if is_var(c, false) => ch = c,
                _ => {
                    if !brackets {
                        self.ungetc(next);
                        break;
                    }
                    return Err(CmdError::Lex("invalid environment variable"));
                }
            }
            if name.len() == MAX_NAME {
                return Err(CmdError::Lex("environment variable is too long"));
            }
            name.push(ch);
        }
        let name = String::from_utf8_lossy(&name);
        if let Some(value) = self.env.var(&name) {
            buf.extend_from_slice(value.as_bytes());
        }
        Ok(())
    }

    /// `yylex_token_tilde` (cmd-parse.y:1662-1720). An unknown user is a
    /// bare syntax error.
    fn tilde(&mut self, buf: &mut Vec<u8>) -> Result<(), CmdError> {
        let mut name = Vec::new();
        loop {
            let ch = self.getc();
            match ch {
                Some(c) if !b"/ \t\n\"'".contains(&c) => {
                    if name.len() == MAX_NAME {
                        return Err(CmdError::Lex("user name is too long"));
                    }
                    name.push(c);
                }
                _ => {
                    self.ungetc(ch);
                    break;
                }
            }
        }
        let user = String::from_utf8_lossy(&name);
        let home = self.env.home((!name.is_empty()).then_some(&*user));
        let home = home.ok_or(CmdError::Syntax)?;
        buf.extend_from_slice(home.as_bytes());
        Ok(())
    }
}

fn bytes_to_string(bytes: Vec<u8>) -> Result<String, CmdError> {
    String::from_utf8(bytes).map_err(|_| CmdError::Unsupported("non-UTF-8 argument"))
}

/// Longest `NAME=value` tmux accepts (`CMD_PARSE_MAX_ENVIRON_LEN`,
/// cmd-parse.y:40).
const MAX_ENVIRON_LEN: usize = 16384;

impl Lexer<'_> {
    /// `assignment` / `hidden_assignment` (cmd-parse.y:224-270). The lexer
    /// only returns an `EQUALS` token when it holds a valid name before the
    /// first `=`. Bison reduces the rule without reading a lookahead, so the
    /// assignment is applied before the next token is lexed.
    fn assign(&mut self, text: &str, hidden: bool) -> Result<(), CmdError> {
        if text.len() > MAX_ENVIRON_LEN {
            return Err(CmdError::Lex("environment variable is too long"));
        }
        let (name, value) = text.split_once('=').ok_or(CmdError::Syntax)?;
        if !self.opts.parse_only {
            self.env.set(name, value, hidden);
        }
        Ok(())
    }
}

/// A `;`, newline, `}` or end of text ends a command. tmux quirk
/// (cmd-parse.y:405-415, `commands ';' command`): when that command is a bare
/// `NAME=value` it has no arguments, so the rule builds a fresh empty list
/// and frees every command earlier in the same `;` chain, unbuilt and so
/// unchecked (`x -- ; A=1` is not an `unknown command` error). The
/// assignments already took effect when they were parsed.
fn end_command(
    items: &mut Vec<RawCommand>,
    current: &mut Vec<RawArg>,
    line: u32,
    drop: bool,
    chain_start: usize,
) {
    if !current.is_empty() {
        items.push(RawCommand {
            line,
            args: std::mem::take(current),
        });
    } else if drop {
        items.truncate(chain_start);
    }
}

/// The grammar: statements of commands and assignments separated by `;` and
/// newlines (cmd-parse.y `statements`, `commands`, `command`,
/// `hidden_assignment`), with `{ ... }` blocks as arguments.
fn lex_commands(
    text: &str,
    env: &mut dyn ConfigEnv,
    opts: ParseOpts,
) -> Result<Vec<RawCommand>, CmdError> {
    let mut lexer = Lexer {
        input: text.as_bytes(),
        off: 0,
        escapes: 0,
        eof: false,
        env,
        opts,
        line: 0,
        eol: false,
    };
    statements(&mut lexer, false)
}

/// The statements of the whole text, or of one block (`nested`, ended by
/// its `}`).
fn statements(lexer: &mut Lexer<'_>, nested: bool) -> Result<Vec<RawCommand>, CmdError> {
    let mut items = Vec::new();
    let mut current: Vec<RawArg> = Vec::new();
    // A command has started in this statement: a `;` is only valid after
    // one (`; display` is a syntax error, `display ;; display` is not).
    let mut started = false;
    // The command so far is just `NAME=value` (`optional_assignment` is
    // followed by a TOKEN, so another `NAME=value` is a syntax error).
    let mut assigned = false;
    // Where the current `;`-chain starts in `items`, and whether the
    // assignment now being read follows an earlier command in the chain.
    let mut chain_start = 0;
    let mut drop_chain = false;
    loop {
        match lexer.next_token()? {
            Token::Word { equals, text } => {
                if current.is_empty() {
                    if assigned {
                        if equals {
                            return Err(CmdError::Syntax);
                        }
                        assigned = false;
                    } else if equals {
                        drop_chain = started;
                        lexer.assign(&text, false)?;
                        started = true;
                        assigned = true;
                        continue;
                    }
                    started = true;
                }
                current.push(RawArg::Str(text));
            }
            Token::LBrace => {
                // Only an argument may be a block, never the command name.
                if current.is_empty() {
                    return Err(CmdError::Syntax);
                }
                let block = statements(lexer, true)?;
                current.push(RawArg::Block(block));
            }
            Token::RBrace => {
                if !nested {
                    return Err(CmdError::Syntax);
                }
                end_command(
                    &mut items,
                    &mut current,
                    lexer.line,
                    assigned && drop_chain,
                    chain_start,
                );
                return Ok(items);
            }
            Token::Hidden => {
                // `statement: hidden_assignment`, then the newline (or the
                // block's `}`): not part of a `;` list, so only a whole
                // statement of its own.
                if started || !current.is_empty() {
                    return Err(CmdError::Syntax);
                }
                match lexer.next_token()? {
                    Token::Word { equals: true, text } => lexer.assign(&text, true)?,
                    _ => return Err(CmdError::Syntax),
                }
                match lexer.next_token()? {
                    Token::Eol => {}
                    Token::RBrace if nested => return Ok(items),
                    _ => return Err(CmdError::Syntax),
                }
            }
            Token::Directive => {
                // Valid only where a statement or command may start.
                if !current.is_empty() || assigned {
                    return Err(CmdError::Syntax);
                }
                return Err(CmdError::Unsupported("%if conditionals"));
            }
            Token::Semi => {
                if !started {
                    return Err(CmdError::Syntax);
                }
                end_command(
                    &mut items,
                    &mut current,
                    lexer.line,
                    assigned && drop_chain,
                    chain_start,
                );
                assigned = false;
            }
            Token::Eol => {
                end_command(
                    &mut items,
                    &mut current,
                    lexer.line,
                    assigned && drop_chain,
                    chain_start,
                );
                started = false;
                assigned = false;
                chain_start = items.len();
            }
            Token::End => {
                // A block still open at the end of the text.
                return if nested {
                    Err(CmdError::Syntax)
                } else {
                    Ok(items)
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::cmd::{NumError, PrintFlags, strtonum};

    /// Test environment: fixed variables and homes.
    struct FakeEnv {
        vars: HashMap<String, String>,
        /// Every `set` call, in order: (name, value, hidden).
        sets: Vec<(String, String, bool)>,
    }

    impl ConfigEnv for FakeEnv {
        fn var(&self, name: &str) -> Option<String> {
            self.vars.get(name).cloned()
        }
        fn set(&mut self, name: &str, value: &str, hidden: bool) {
            self.vars.insert(name.to_owned(), value.to_owned());
            self.sets.push((name.to_owned(), value.to_owned(), hidden));
        }
        fn home(&self, user: Option<&str>) -> Option<String> {
            match user {
                None => Some("/home/me".to_owned()),
                Some("root") => Some("/root".to_owned()),
                Some("bin") => Some("/bin".to_owned()),
                Some(_) => None,
            }
        }
    }

    fn env() -> FakeEnv {
        FakeEnv {
            vars: HashMap::from([
                ("FOO".to_owned(), "bar".to_owned()),
                ("EMPTY".to_owned(), String::new()),
            ]),
            sets: Vec::new(),
        }
    }

    const RUN: ParseOpts = ParseOpts {
        parse_only: false,
        one_group: false,
    };
    const PARSE_ONLY: ParseOpts = ParseOpts {
        parse_only: true,
        one_group: false,
    };

    fn parse(text: &str) -> Result<Vec<CommandCall>, CmdError> {
        parse_config_with(text, &mut env(), RUN)
    }

    fn words(w: &[&str]) -> Vec<String> {
        w.iter().map(|s| (*s).to_owned()).collect()
    }

    /// `name` and positionals of each parsed command.
    fn shape(calls: &[CommandCall]) -> Vec<(&'static str, Vec<String>)> {
        calls
            .iter()
            .map(|c| {
                let pos = c.args.positionals.iter().map(|v| v.text().into_owned());
                (c.entry.name, pos.collect())
            })
            .collect()
    }

    fn argv(w: &[&str]) -> Vec<(&'static str, Vec<String>)> {
        shape(&parse_argv(&words(w)).expect("parses"))
    }

    fn argv_err(w: &[&str]) -> String {
        parse_argv(&words(w)).expect_err("fails").to_string()
    }

    fn cfg(text: &str) -> Vec<(&'static str, Vec<String>)> {
        shape(&parse(text).expect("parses"))
    }

    /// Names of the commands `text` parses to, and the `set` calls it made.
    fn assigns(text: &str) -> (Vec<&'static str>, Vec<String>) {
        let mut e = env();
        let calls = parse_config_with(text, &mut e, RUN).expect("parses");
        let sets = e
            .sets
            .iter()
            .map(|(n, v, h)| format!("{}{n}={v}", if *h { "%hidden " } else { "" }))
            .collect();
        (calls.iter().map(|c| c.entry.name).collect(), sets)
    }

    fn cfg_err(text: &str) -> String {
        parse(text).expect_err("fails").to_string()
    }

    fn one(name: &'static str, pos: &[&str]) -> Vec<(&'static str, Vec<String>)> {
        vec![(name, words(pos))]
    }

    // ---- argv path (cmd_parse_from_arguments, cmd-parse.y:1065-1136) ----
    // Reference commands: `env -u TMUX -u TMUX_PANE $T -L x -f /dev/null
    // <argv...>` against a server started with `new -d cat`.

    /// `display -p '"a b"'` prints `"a b"`: quotes are not processed.
    #[test]
    fn argv_keeps_quotes() {
        assert_eq!(
            argv(&["display", "-p", "\"a b\""]),
            one("display-message", &["\"a b\""])
        );
        assert_eq!(
            argv(&["display", "-p", "'x'"]),
            one("display-message", &["'x'"])
        );
    }

    /// `display -p hello '#comment'` -> `too many arguments (need at most 1)`:
    /// `#comment` is a second positional, not a comment.
    #[test]
    fn argv_does_not_process_comments() {
        assert_eq!(
            argv_err(&["display", "-p", "hello", "#comment"]),
            "command display-message: too many arguments (need at most 1)"
        );
        assert_eq!(
            argv(&["display", "-p", "#comment"]),
            one("display-message", &["#comment"])
        );
    }

    /// `display -p 'a;' display -p b` prints `a` then `b`.
    #[test]
    fn argv_trailing_semicolon_ends_a_command() {
        assert_eq!(
            argv(&["display", "-p", "a;", "display", "-p", "b"]),
            vec![
                ("display-message", words(&["a"])),
                ("display-message", words(&["b"])),
            ]
        );
    }

    /// A word of just `;` ends the command and contributes nothing:
    /// `display -p x ';' display -p y` prints `x`, `y`.
    #[test]
    fn argv_lone_semicolon_separates() {
        assert_eq!(
            argv(&["display", "-p", "x", ";", "display", "-p", "y"]),
            vec![
                ("display-message", words(&["x"])),
                ("display-message", words(&["y"])),
            ]
        );
        // Empty commands vanish: leading, doubled and trailing `;`.
        assert_eq!(argv(&[";"]), vec![]);
        assert_eq!(
            argv(&[";", "display", "-p", "x", ";", ";"]),
            one("display-message", &["x"])
        );
    }

    /// `display -p 'a\;'` prints `a;`; `display -p 'a\\;'` prints `a\;`
    /// (only one backslash is eaten); `display -p ';;'` prints `;`.
    #[test]
    fn argv_escaped_semicolon_is_literal() {
        assert_eq!(
            argv(&["display", "-p", "a\\;"]),
            one("display-message", &["a;"])
        );
        assert_eq!(
            argv(&["display", "-p", "a\\\\;"]),
            one("display-message", &["a\\;"])
        );
        // `\;` alone is the one-character word `;`.
        assert_eq!(
            argv(&["display", "-p", "\\;"]),
            one("display-message", &[";"])
        );
        // `;;`: the first `;` stays, the last one is trimmed and is a literal
        // word end only when preceded by `\`; here it ends the command, so
        // the word is `;`.
        assert_eq!(
            argv(&["display", "-p", ";;"]),
            one("display-message", &[";"])
        );
    }

    /// `display -p 'a;b'` prints `a;b`: `;` mid-word is literal.
    #[test]
    fn argv_midword_semicolon_is_literal() {
        assert_eq!(
            argv(&["display", "-p", "a;b"]),
            one("display-message", &["a;b"])
        );
        assert_eq!(
            argv(&["display", "-p", ";a"]),
            one("display-message", &[";a"])
        );
    }

    /// `display -p x ';;' display -p y` is `too many arguments`: `;;` is the
    /// word `;` (trailing `;` trimmed, ends the command) -> two positionals.
    #[test]
    fn argv_double_semicolon_word() {
        assert_eq!(
            argv_err(&["display-message", "-p", "x", ";;"]),
            "command display-message: too many arguments (need at most 1)"
        );
    }

    #[test]
    fn argv_empty_is_no_commands() {
        assert!(matches!(parse_argv(&[]).unwrap().as_slice(), []));
    }

    /// Everything is parsed before anything runs, so an error in the second
    /// command rejects the whole line (`cmd_parse_build_commands` returns the
    /// first error and frees the list, cmd-parse.y:960-967). Reference:
    /// `display -p first ';' nosuchcmd` prints `unknown command: nosuchcmd`
    /// and not `first`.
    #[test]
    fn argv_error_in_any_command_rejects_all() {
        assert_eq!(
            argv_err(&["display", "-p", "first", ";", "nosuchcmd"]),
            "unknown command: nosuchcmd"
        );
    }

    /// Execution-time values stay raw: `new -d -x - -y -` parses (the
    /// reference creates a session), and `display -p first ; new -d -x abc`
    /// parses too (it fails at execution with `width invalid`).
    #[test]
    fn argv_values_are_not_validated() {
        let calls = parse_argv(&words(&["new", "-d", "-x", "-", "-y", "-"])).unwrap();
        assert_eq!(calls[0].entry.name, "new-session");
        assert_eq!(calls[0].args.value('x'), Some("-"));
        assert_eq!(calls[0].args.value('y'), Some("-"));
        let calls = parse_argv(&words(&[
            "display", "-p", "first", ";", "new", "-d", "-x", "abc",
        ]))
        .unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].args.value('x'), Some("abc"));
        assert!(calls[1].args.number('x', 1, 10000).is_err());
        // capture-pane -S takes a format or garbage; both parse.
        let calls = parse_argv(&words(&["capturep", "-S", "#{cursor_y}"])).unwrap();
        assert_eq!(calls[0].args.value('S'), Some("#{cursor_y}"));
        assert!(parse_argv(&words(&["capturep", "-S", "garbage"])).is_ok());
    }

    /// Errors, verbatim from the reference (arguments.c:181, 205-233,
    /// 331-340; cmd.c:508-540).
    #[test]
    fn argv_errors() {
        // `lsw -Z`
        assert_eq!(
            argv_err(&["lsw", "-Z"]),
            "command list-windows: unknown flag -Z"
        );
        // `display -p a b c`
        assert_eq!(
            argv_err(&["display", "-p", "a", "b", "c"]),
            "command display-message: too many arguments (need at most 1)"
        );
        // `has x`
        assert_eq!(
            argv_err(&["has", "x"]),
            "command has-session: too many arguments (need at most 0)"
        );
        // `set-option`
        assert_eq!(
            argv_err(&["set-option"]),
            "command set-option: too few arguments (need at least 1)"
        );
        // `has -t`
        assert_eq!(
            argv_err(&["has", "-t"]),
            "command has-session: -t expects an argument"
        );
        // `display -p '-@'`
        assert_eq!(
            argv_err(&["display", "-p", "-@"]),
            "command display-message: invalid flag -@"
        );
        // `ls -?` and `kill-server -?`
        assert_eq!(
            argv_err(&["ls", "-?"]),
            "usage: list-sessions [-r] [-F format] [-f filter] [-O order]"
        );
        assert_eq!(argv_err(&["kill-server", "-?"]), "usage: kill-server ");
        assert_eq!(argv_err(&["nosuch"]), "unknown command: nosuch");
        assert_eq!(
            argv_err(&["displ"]),
            "ambiguous command: displ, could be: display-menu, display-message, display-popup, display-panes"
        );
        assert_eq!(
            argv_err(&["d"]),
            "ambiguous command: d, could be: delete-buffer, detach-client, display-menu, display-message, display-popup, display-panes"
        );
        // The empty name is ambiguous with everything (reference: `tmux ''`).
        assert!(argv_err(&[""]).starts_with("ambiguous command: , could be: attach-session,"));
    }

    /// A non-ASCII flag byte is one raw byte in tmux's message (reference:
    /// `display -p -é` prints `invalid flag -` and the byte 0xc3).
    #[test]
    fn invalid_flag_is_a_raw_byte() {
        let e = parse_argv(&words(&["display", "-p", "-é"])).unwrap_err();
        assert_eq!(e.to_bytes(), b"command display-message: invalid flag -\xc3");
    }

    // ---- args_parse (arguments.c:207-345) ----

    fn parsed(w: &[&str]) -> CommandCall {
        parse_argv(&words(w)).unwrap().remove(0)
    }

    /// `-ta` and `-t a` are the same; combined flags; repeats; `--`.
    #[test]
    fn flag_value_forms() {
        let a = parsed(&["has", "-ta"]).args;
        assert_eq!(a.value('t'), Some("a"));
        let a = parsed(&["has", "-t", "a"]).args;
        assert_eq!(a.value('t'), Some("a"));
        // The value may start with `-` (reference: `display -p -t -x` fails on
        // the target, not on parsing).
        let a = parsed(&["display", "-p", "-t", "-x"]).args;
        assert_eq!(a.value('t'), Some("-x"));
        // `-pt a`: boolean then value flag.
        let a = parsed(&["display", "-pt", "a", "msg"]).args;
        assert!(a.flag('p'));
        assert_eq!(a.value('t'), Some("a"));
        assert_eq!(a.positionals, [ArgValue::Str("msg".to_owned())]);
        // `-ta` where the value is the rest of the word: `-pta`.
        assert_eq!(parsed(&["display", "-pta"]).args.value('t'), Some("a"));
    }

    /// Reference: `display-message -p -pp x` prints `x`; repeats count
    /// (arguments.c:663-683) and the last value wins (`args_get`).
    #[test]
    fn repeated_flags() {
        let a = parsed(&["display", "-p", "-pp", "x"]).args;
        assert_eq!(a.count('p'), 3);
        let a = parsed(&["new", "-e", "A=1", "-e", "B=2", "-s", "x", "-s", "y"]).args;
        assert_eq!(a.values('e').collect::<Vec<_>>(), ["A=1", "B=2"]);
        assert_eq!(a.value('s'), Some("y"));
        assert!(!a.flag('d'));
    }

    /// Reference: `display -p -- -x` prints `-x`; `display -p -` prints `-`.
    #[test]
    fn double_dash_and_lone_dash() {
        assert_eq!(
            argv(&["display", "-p", "--", "-x"]),
            one("display-message", &["-x"])
        );
        assert_eq!(
            argv(&["display", "-p", "-"]),
            one("display-message", &["-"])
        );
        // After the first positional, flags are positionals: `display -p a -p`.
        assert_eq!(
            argv_err(&["display", "-p", "a", "-p"]),
            "command display-message: too many arguments (need at most 1)"
        );
    }

    /// `x::` flags (arguments.c:192-199): `resize-pane -U` with no value is
    /// valid; `-U 5` takes 5; but `-U -D` does not swallow `-D`.
    #[test]
    fn optional_value_flags() {
        let a = parsed(&["resizep", "-U"]).args;
        assert!(a.flag('U'));
        assert_eq!(a.value('U'), None);
        let a = parsed(&["resizep", "-U", "5"]).args;
        assert_eq!(a.value('U'), Some("5"));
        let a = parsed(&["resizep", "-U", "-D"]).args;
        assert!(a.flag('U') && a.flag('D'));
        assert_eq!(a.value('U'), None);
        // A value that merely starts with `-` and a digit is swallowed.
        let a = parsed(&["resizep", "-U", "-3"]).args;
        assert_eq!(a.value('U'), Some("-3"));
    }

    #[test]
    fn number_accessor_is_strtonum() {
        let a = parsed(&["new", "-x", " +80", "-y", "99999999999999999999"]).args;
        assert_eq!(a.number('x', 1, 10000), Ok(80));
        assert_eq!(a.number('y', 1, 10000), Err(NumError::TooLarge));
        assert_eq!(a.number('s', 1, 2), Err(NumError::Missing));
        assert_eq!(strtonum("0", 1, 5), Err(NumError::TooSmall));
        assert_eq!(strtonum("-", 1, 5), Err(NumError::Invalid));
        assert_eq!(strtonum("7x", 1, 9), Err(NumError::Invalid));
        assert_eq!(strtonum("", 1, 9), Err(NumError::Invalid));
        assert_eq!(strtonum("1", 9, 1), Err(NumError::Invalid));
    }

    #[test]
    fn target_accessor_keeps_raw_text() {
        let a = parsed(&["has", "-t", "=a:1.2"]).args;
        assert_eq!(a.target('t').unwrap().raw(), "=a:1.2");
        assert!(a.target('s').is_none());
    }

    // ---- config path (yylex, cmd-parse.y:1262-1780) ----
    // Reference: `$T -f /dev/null start-server \; source-file -v c.conf`.

    #[test]
    fn config_plain_and_separators() {
        assert_eq!(cfg("display -p a"), one("display-message", &["a"]));
        assert_eq!(cfg(""), vec![]);
        assert_eq!(cfg("\n\n  \n"), vec![]);
        assert_eq!(
            cfg("display -p a ; display -p b"),
            vec![
                ("display-message", words(&["a"])),
                ("display-message", words(&["b"]))
            ]
        );
        assert_eq!(cfg("display -p a;display -p b").len(), 2);
        assert_eq!(cfg("display -p a\ndisplay -p b").len(), 2);
        assert_eq!(cfg("display -p a ;; display -p b").len(), 2);
        assert_eq!(cfg("display -p a ;").len(), 1);
        assert_eq!(cfg("display -p a\r\ndisplay -p b\r\n").len(), 2);
    }

    /// A leading `;` is a syntax error (reference: `; display-message -p a`
    /// -> `syntax error`), as is a stray `}` and an unknown `%word`.
    #[test]
    fn config_syntax_errors() {
        assert_eq!(cfg_err("; display -p a"), "syntax error");
        assert_eq!(cfg_err("}"), "syntax error");
        assert_eq!(cfg_err("display -p a }"), "syntax error");
        assert_eq!(cfg_err("%foo"), "syntax error");
        assert_eq!(cfg_err("display -p x %if"), "syntax error");
    }

    /// Reference: `display -p 'a;b' "c;d"` -> too many arguments (two
    /// words); quotes protect `;` and spaces and the `#`.
    #[test]
    fn config_quotes() {
        assert_eq!(cfg("display -p 'a b'"), one("display-message", &["a b"]));
        assert_eq!(cfg("display -p \"a b\""), one("display-message", &["a b"]));
        assert_eq!(cfg("display -p 'a;b'"), one("display-message", &["a;b"]));
        assert_eq!(
            cfg("display -p \"a # b\""),
            one("display-message", &["a # b"])
        );
        // Adjacent quoted and bare parts join: `a'b c'd`.
        assert_eq!(
            cfg("display -p a'b c'd"),
            one("display-message", &["ab cd"])
        );
        // The other quote is literal inside quotes.
        assert_eq!(
            cfg("display -p \"it's\""),
            one("display-message", &["it's"])
        );
        assert_eq!(
            cfg("display -p '\"q\"'"),
            one("display-message", &["\"q\""])
        );
        // Empty quotes are an empty word.
        assert_eq!(cfg("display -p ''"), one("display-message", &[""]));
        // An unterminated quote runs to the end of input (reference: `display
        // "abc` displays `abc\n` ... the newline appended by tmux's lexer is
        // inside the quote).
        assert_eq!(cfg("display \"abc"), one("display-message", &["abc"]));
        assert_eq!(
            cfg_err("display -p 'a;b' \"c;d\""),
            "command display-message: too many arguments (need at most 1)"
        );
    }

    /// Reference: `display -p "a\n  b"` as a real newline inside quotes keeps
    /// the newline and drops the following indentation
    /// (cmd-parse.y:1676-1699).
    #[test]
    fn config_newline_in_quotes() {
        assert_eq!(
            cfg("display -p \"a\n  b\""),
            one("display-message", &["a\nb"])
        );
        // A comment line inside quotes is dropped but its newline is not
        // (reference: `display -p "a<nl>  # gone<nl>b"` prints a, blank, b).
        assert_eq!(
            cfg("display -p \"a\n  # gone\nb\""),
            one("display-message", &["a\n\nb"])
        );
        assert_eq!(
            cfg("display -p \"a\n#{x}\""),
            one("display-message", &["a\n#{x}"])
        );
    }

    /// Reference: `display -p "a\\n"` -> `a\n` (two chars); `\8` -> `8`;
    /// `\s` space; `\e` ESC; octal `\101`; `\u00e9`; invalid octal and
    /// `\ud800` errors (cmd-parse.y:1490-1600).
    #[test]
    fn config_escapes() {
        assert_eq!(cfg("display -p a\\ b"), one("display-message", &["a b"]));
        assert_eq!(
            cfg("display -p 'a\\nb'"),
            one("display-message", &["a\\nb"])
        );
        assert_eq!(
            cfg("display -p \"a\\nb\""),
            one("display-message", &["a\nb"])
        );
        assert_eq!(
            cfg("display -p a\\tb\\rc"),
            one("display-message", &["a\tb\rc"])
        );
        assert_eq!(cfg("display -p \\8"), one("display-message", &["8"]));
        assert_eq!(cfg("display -p a\\sb"), one("display-message", &["a b"]));
        assert_eq!(
            cfg("display -p \\e\\a\\b\\f\\v"),
            one("display-message", &["\x1b\x07\x08\x0c\x0b"])
        );
        assert_eq!(cfg("display -p \\101"), one("display-message", &["A"]));
        assert_eq!(
            cfg("display -p \\u00e9\\u0041"),
            one("display-message", &["éA"])
        );
        assert_eq!(
            cfg("display -p \\U0001F600"),
            one("display-message", &["😀"])
        );
        assert_eq!(
            cfg("display -p \"\\\"q\\\"\""),
            one("display-message", &["\"q\""])
        );
        assert_eq!(cfg("display -p x\\\\"), one("display-message", &["x\\"]));
        assert_eq!(
            cfg("display -p \\$HOME"),
            one("display-message", &["$HOME"])
        );
        assert_eq!(cfg("display -p \\#x"), one("display-message", &["#x"]));
        // `\;` in the config lexer is a literal `;` in the word.
        assert_eq!(cfg("display -p a\\;b"), one("display-message", &["a;b"]));
        assert_eq!(cfg_err("display -p \\4"), "invalid octal escape");
        assert_eq!(cfg_err("display -p \\18"), "invalid octal escape");
        assert_eq!(cfg_err("display -p \\u00zz"), "invalid \\u argument");
        assert_eq!(cfg_err("display -p \\ud800"), "invalid \\u argument");
        assert_eq!(cfg_err("display -p \\Uzzzzzzzz"), "invalid \\U argument");
        // Truncated at end of input: a bare syntax error.
        assert_eq!(cfg_err("display -p \\u00"), "syntax error");
        // A NUL from `\000` ends the word like a C string.
        assert_eq!(cfg("display -p a\\000b"), one("display-message", &["a"]));
    }

    /// Backslash-newline joins lines (cmd-parse.y:1220-1245). Reference:
    /// `display -p a\<nl> b` is two words (`a`, `b`) -> too many arguments,
    /// while `\<nl> display -p x` is a normal command.
    #[test]
    fn config_line_continuation() {
        assert_eq!(
            cfg_err("display -p a\\\n b"),
            "command display-message: too many arguments (need at most 1)"
        );
        assert_eq!(cfg("\\\n display -p x"), one("display-message", &["x"]));
        // `a\<nl>b` joins into one word.
        assert_eq!(cfg("display -p a\\\nb"), one("display-message", &["ab"]));
    }

    /// Reference: `# c`, `display -p a # c` and `display -p a#x` (`a#x`: `#`
    /// only starts a comment at the start of a token).
    #[test]
    fn config_comments() {
        assert_eq!(cfg("# c"), vec![]);
        assert_eq!(cfg("  # c\ndisplay -p a"), one("display-message", &["a"]));
        assert_eq!(cfg("display -p a # c"), one("display-message", &["a"]));
        assert_eq!(
            cfg("display -p a #c ; nosuch"),
            one("display-message", &["a"])
        );
        assert_eq!(cfg("display -p a#x"), one("display-message", &["a#x"]));
        assert_eq!(cfg("display -p '#x'"), one("display-message", &["#x"]));
        // `#x` as a word is a comment to end of line.
        assert_eq!(cfg("display -p #x"), one("display-message", &[]));
    }

    /// `$VAR`, `${VAR}` and `~` expand outside single quotes
    /// (cmd-parse.y:1602-1720). Reference: `display -p ~` prints the home
    /// directory; `~nonexistentuser` is a `syntax error`;
    /// `${HO` -> `invalid environment variable`.
    #[test]
    fn config_variables_and_tilde() {
        assert_eq!(cfg("display -p $FOO"), one("display-message", &["bar"]));
        assert_eq!(cfg("display -p ${FOO}x"), one("display-message", &["barx"]));
        assert_eq!(cfg("display -p \"$FOO\""), one("display-message", &["bar"]));
        assert_eq!(cfg("display -p '$FOO'"), one("display-message", &["$FOO"]));
        assert_eq!(cfg("display -p $NOPE"), one("display-message", &[""]));
        assert_eq!(cfg("display -p \"$\""), one("display-message", &["$"]));
        assert_eq!(cfg("display -p $ "), one("display-message", &["$"]));
        // `$` as the very last byte is a syntax error (EOF after `$`).
        assert_eq!(cfg_err("display -p $"), "syntax error");
        assert_eq!(cfg("display -p $1"), one("display-message", &["$1"]));
        assert_eq!(cfg("display -p ~"), one("display-message", &["/home/me"]));
        assert_eq!(
            cfg("display -p ~/x"),
            one("display-message", &["/home/me/x"])
        );
        assert_eq!(
            cfg("display -p ~root/x"),
            one("display-message", &["/root/x"])
        );
        assert_eq!(cfg("display -p '~'"), one("display-message", &["~"]));
        // Expands again after a quote boundary; not mid-word.
        assert_eq!(cfg("display -p x~"), one("display-message", &["x~"]));
        assert_eq!(
            cfg("display -p \"~\""),
            one("display-message", &["/home/me"])
        );
        assert_eq!(cfg_err("display -p ~nouser"), "syntax error");
        assert_eq!(cfg_err("display -p ${FO"), "invalid environment variable");
        assert_eq!(
            cfg_err("display -p ${FO}${"),
            "invalid environment variable"
        );
    }

    /// `%` words: all digits and `%` is a plain word (pane ids), reference
    /// `display -p %1 %` is too many arguments.
    #[test]
    fn config_percent_words() {
        assert_eq!(cfg("display -p %1"), one("display-message", &["%1"]));
        assert_eq!(cfg("display -p %%"), one("display-message", &["%%"]));
        assert_eq!(cfg("display -p 5%"), one("display-message", &["5%"]));
        assert_eq!(
            cfg_err("display -p %1 %"),
            "command display-message: too many arguments (need at most 1)"
        );
    }

    /// Typed "unsupported" errors, the allowed escape hatch in the brief:
    /// conditionals and non-UTF-8 words. Reference accepts all of them.
    #[test]
    fn config_m0_unsupported_is_typed() {
        for text in [
            "%if 1\ndisplay -p x\n%endif",
            "display -p \\377",
            "display -p \\U00110000",
        ] {
            let e = parse(text).expect_err(text);
            assert!(matches!(e, CmdError::Unsupported(_)), "{text}: {e}");
        }
        // `=` after the command name is just an argument, and `=x` first is
        // not a valid name.
        assert_eq!(cfg("display -p =x"), one("display-message", &["=x"]));
        assert_eq!(cfg_err("=x display"), "unknown command: =x");
    }

    /// `NAME=value` statements (cmd-parse.y `assignment`, `command`,
    /// `hidden_assignment`). Reference: `source-file -nv` on each text.
    #[test]
    fn config_assignments_are_typed_statements() {
        // `A=1` is ok with no output; `A=1 display -p x` prints
        // `display-message -p x`; `display ; A=1` is ok.
        let a = |t: &str| {
            let (c, s) = assigns(t);
            (
                c,
                s.iter().map(String::as_str).collect::<Vec<_>>().join(","),
            )
        };
        assert_eq!(a("A=1"), (vec![], "A=1".into()));
        assert_eq!(
            a("A=1 display -p x"),
            (vec!["display-message"], "A=1".into())
        );
        // A bare assignment after `;` drops the commands before it in the
        // chain, unbuilt (cmd-parse.y:405-415): reference prints nothing.
        assert_eq!(a("display -p x ; A=1"), (vec![], "A=1".into()));
        assert_eq!(
            cfg("display -p x ; A=1 ; display -p y"),
            one("display-message", &["y"])
        );
        assert_eq!(a("x -- ; A=1"), (vec![], "A=1".into()));
        assert_eq!(cfg("display -p x ; A=1 display -p y").len(), 2);
        assert_eq!(
            a("A=1 ; display -p x"),
            (vec!["display-message"], "A=1".into())
        );
        assert_eq!(a("A=1;"), (vec![], "A=1".into()));
        assert_eq!(
            a("A=1 \\\n display -p x"),
            (vec!["display-message"], "A=1".into())
        );
        assert_eq!(a("A=\"b c\" ; B="), (vec![], "A=b c,B=".into()));
        assert_eq!(a("A=x=y"), (vec![], "A=x=y".into()));
        assert_eq!(a("%hidden A=1"), (vec![], "%hidden A=1".into()));
        assert_eq!(
            a("%hidden A=1 \n%hidden B=2\nA=$FOO"),
            (vec![], "%hidden A=1,%hidden B=2,A=bar".into())
        );
        assert_eq!(cfg("A=1 display -p x"), one("display-message", &["x"]));
        // `=` in a later word is just an argument.
        assert_eq!(cfg("display -p A=1"), one("display-message", &["A=1"]));
        // Reference: syntax error for each.
        for text in [
            "A=1 B=2",
            "A=1 B=2 display -p x",
            "%hidden",
            "%hidden A=1 display -p x",
            "%hidden A=1 ; display -p x",
            "display ; %hidden A=1",
            "A=1 ; %hidden B=2",
            "%hidden display",
        ] {
            assert_eq!(cfg_err(text), "syntax error", "{text}");
        }
        // The command after an assignment still goes through lookup.
        assert_eq!(cfg_err("A=1 nosuch"), "unknown command: nosuch");
        // cmd-parse.y:237: longer than CMD_PARSE_MAX_ENVIRON_LEN.
        let long = format!("A={}", "x".repeat(16384));
        assert_eq!(cfg_err(&long), "environment variable is too long");
        assert_eq!(assigns(&long[..16384]).1.len(), 1);
    }

    /// Value of `$name` after parsing `text` with `opts`, and the display
    /// arguments it produced.
    fn probe(text: &str, opts: ParseOpts) -> (Vec<String>, FakeEnv) {
        let mut e = FakeEnv {
            vars: HashMap::new(),
            sets: Vec::new(),
        };
        let calls = parse_config_with(text, &mut e, opts).expect("parses");
        (
            calls
                .iter()
                .flat_map(|c| c.args.positionals.iter().map(|v| v.text().into_owned()))
                .collect(),
            e,
        )
    }

    /// Assignments take effect while parsing (cmd-parse.y:224-270,
    /// `environ_put(global_environ)` unless `CMD_PARSE_PARSEONLY`), and
    /// `yylex_token_variable` (1584) reads that environment, so a later
    /// `$NAME` in the same text, even the same statement, sees the value.
    /// Reference, tmux 3.8, `source-file -v f` (no -n), one file per text:
    ///   `A=1 display -p "[$A]"`       -> display-message -p [1]
    ///   `B=2 ; display -p "[$B]"`     -> display-message -p [2]
    ///   `C=3\ndisplay -p "[$C]"`      -> display-message -p [3]
    ///   `%hidden H=1\ndisplay -p "[$H]"` -> display-message -p [1]
    ///   `A=1 ; A=2 display -p "[$A]"` -> display-message -p [2]
    /// and with `source-file -nv` all print `[]`, setting nothing.
    #[test]
    fn config_assignments_are_visible_to_later_variables() {
        for (text, name, want) in [
            ("A=1 display -p \"[$A]\"", "A", "1"),
            ("B=2 ; display -p \"[$B]\"", "B", "2"),
            ("C=3\ndisplay -p \"[$C]\"", "C", "3"),
            ("%hidden H=1\ndisplay -p \"[$H]\"", "H", "1"),
            ("A=1 ; A=2 display -p \"[$A]\"", "A", "2"),
        ] {
            let (args, e) = probe(text, RUN);
            assert_eq!(args, [format!("[{want}]")], "{text}");
            assert_eq!(e.vars[name], want, "{text}");
            let (args, e) = probe(text, PARSE_ONLY);
            assert_eq!(args, ["[]"], "{text} (parse only)");
            assert!(e.sets.is_empty() && e.vars.is_empty(), "{text}");
        }
        let (_, e) = probe("%hidden H=1\nA=2", RUN);
        assert_eq!(
            e.sets,
            [
                ("H".to_owned(), "1".to_owned(), true),
                ("A".to_owned(), "2".to_owned(), false)
            ]
        );
    }

    /// Assignments read before a later error stay applied. Reference:
    /// `source-file z.conf` with `ZZ=1\n; x` -> `z.conf:2: syntax error`,
    /// then `show-environment -g ZZ` -> `ZZ=1`. Also true of an unknown
    /// command, which fails only after the whole text is lexed.
    #[test]
    fn config_assignments_survive_a_later_error() {
        for text in ["ZZ=1\n; x", "ZZ=1\nnosuch"] {
            let mut e = env();
            parse_config_with(text, &mut e, RUN).expect_err(text);
            assert_eq!(e.vars["ZZ"], "1", "{text}");
            let mut e = env();
            parse_config_with(text, &mut e, PARSE_ONLY).expect_err(text);
            assert!(!e.vars.contains_key("ZZ"), "{text} (parse only)");
        }
    }

    /// `~` and `$VAR` come from the `ConfigEnv`, never the host. Reference:
    /// `display -p ~bin/x` -> `display-message -p /bin/x` on a host whose
    /// `bin` user lives in /usr/lib/passwd (NSS), not /etc/passwd.
    #[test]
    fn config_tilde_and_vars_go_through_config_env() {
        assert_eq!(
            cfg("display -p ~bin/x"),
            one("display-message", &["/bin/x"])
        );
        assert_eq!(cfg("display -p ~root"), one("display-message", &["/root"]));
        assert_eq!(
            cfg("display -p ~/x"),
            one("display-message", &["/home/me/x"])
        );
        assert_eq!(cfg("display -p $FOO"), one("display-message", &["bar"]));
        assert_eq!(cfg("display -p ${EMPTY}x"), one("display-message", &["x"]));
        assert_eq!(cfg("display -p $UNSET"), one("display-message", &[""]));
        assert_eq!(cfg_err("display -p ~nosuchuser"), "syntax error");
    }

    /// Default command aliases (options-table.c:322-331, `cmd_parse_expand_alias`):
    /// `info -x` becomes `show-messages -JT -x`. Reference: `info -x` ->
    /// `command show-messages: unknown flag -x`.
    #[test]
    fn config_default_aliases() {
        assert_eq!(cfg_err("info -x"), "command show-messages: unknown flag -x");
        let calls = parse("info").unwrap();
        assert_eq!(calls[0].entry.name, "show-messages");
        assert!(calls[0].args.flag('J') && calls[0].args.flag('T'));
        let calls = parse("splitp -h").unwrap();
        assert_eq!(calls[0].entry.name, "split-window");
        assert!(calls[0].args.flag('h'));
    }

    /// Lookup errors through the config path carry the same text.
    #[test]
    fn config_lookup_errors() {
        assert_eq!(cfg_err("nosuch"), "unknown command: nosuch");
        assert_eq!(
            cfg_err("displ"),
            "ambiguous command: displ, could be: display-menu, display-message, display-popup, display-panes"
        );
        assert_eq!(
            cfg_err("display -p a b c"),
            "command display-message: too many arguments (need at most 1)"
        );
        assert_eq!(
            cfg_err("has -t"),
            "command has-session: -t expects an argument"
        );
    }

    /// Non-M0 commands parse (arity and flags are validated) but resolve to
    /// a table entry that reports `not implemented`.
    #[test]
    fn non_m0_commands_parse_but_do_not_run() {
        let calls = parse_argv(&words(&["split-window", "-h", "-t", "a", "vim"])).unwrap();
        assert_eq!(calls[0].entry.name, "split-window");
        assert_eq!(
            calls[0].entry.ensure_implemented().unwrap_err().to_string(),
            "not implemented in vtmux yet: split-window"
        );
        assert_eq!(
            argv_err(&["split-window", "-Q"]),
            "command split-window: unknown flag -Q"
        );
    }

    // ---- { } blocks and the canonical printer ----
    // Reference: crates/vtmux-core/tests/keys_cases.sh (`config.txt`), which
    // checks the same through `bind-key` and `list-keys` against tmux 3.8.

    /// The commands of the block that is `calls[0]`'s positional `idx`.
    fn block(calls: &[CommandCall], idx: usize) -> &CommandList {
        match &calls[0].args.positionals[idx] {
            ArgValue::Commands(list) => list,
            ArgValue::Str(s) => panic!("{s:?} is not a block"),
        }
    }

    #[test]
    fn block_is_a_nested_command_list() {
        let calls = parse("if -F 1 { display a ; display b } { display c }").unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].entry.name, "if-shell");
        assert_eq!(calls[0].args.positionals[0], "1");
        let then = block(&calls, 1);
        assert_eq!(
            then.calls.iter().map(|c| c.entry.name).collect::<Vec<_>>(),
            ["display-message", "display-message"]
        );
        assert_eq!(block(&calls, 2).calls.len(), 1);
        // A block as a string is its printed commands (args_value_as_string).
        assert_eq!(
            calls[0].args.positional_text(1).unwrap(),
            "display-message a ; display-message b"
        );
        assert_eq!(calls[0].args.positional(1), None);
    }

    #[test]
    fn block_print_and_groups() {
        // One line is one group (`;`); each line of a block is its own
        // (`;;`) unless the text is parsed as a string.
        let calls = parse("if -F 1 { display a ; display b\ndisplay c } { }").unwrap();
        let list = block(&calls, 1);
        assert_eq!(
            list.print(PrintFlags::default()),
            "display-message a ; display-message b ;; display-message c"
        );
        assert_eq!(
            list.print(PrintFlags {
                escaped: true,
                no_groups: false
            }),
            "display-message a \\; display-message b \\;\\; display-message c"
        );
        assert_eq!(
            list.print(PrintFlags::LIST_KEYS),
            "display-message a \\; display-message b \\; display-message c"
        );
        let one = parse_string("display a ; display b\ndisplay c", &mut env()).unwrap();
        assert_eq!(
            one.print(PrintFlags::default()),
            "display-message a ; display-message b ; display-message c"
        );
        assert_eq!(block(&calls, 2).print(PrintFlags::default()), "");
    }

    #[test]
    fn block_errors() {
        // A block is only accepted where the command's args_parse says so.
        assert_eq!(
            cfg_err("display { a }"),
            "command display-message: argument 1 must be \"string\""
        );
        assert_eq!(
            cfg_err("if -F 1 a b c"),
            "command if-shell: too many arguments (need at most 3)"
        );
        // Position 4 is checked before the count is (reference: `if -F 1 a b
        // { display c }` -> `argument 4 must be "string"`).
        assert_eq!(
            cfg_err("if -F 1 a b { display c }"),
            "command if-shell: argument 4 must be \"string\""
        );
        assert_eq!(
            cfg_err("display -t { a }"),
            "command display-message: -t argument must be a string"
        );
        assert_eq!(
            cfg_err("run { display a }"),
            "command run-shell: argument 1 must be \"string\""
        );
        assert!(parse("run -C { display a }").is_ok());
        for text in [
            "{ display a }",
            "display a }",
            "bind a { display a",
            "bind a { { x } }",
        ] {
            assert!(
                matches!(
                    parse(text).unwrap_err(),
                    CmdError::Syntax | CmdError::Edge(_)
                ),
                "{text}"
            );
        }
        // The block is built before the arity is checked, so its own
        // errors come first.
        assert!(cfg_err("if -F 1 a b { c }").starts_with("ambiguous command: c,"));
        assert_eq!(cfg_err("bind a { ; }"), "syntax error");
        assert_eq!(cfg_err("bind a { display a }}"), "syntax error");
    }

    #[test]
    fn block_edge_forms_parse() {
        for text in [
            "bind a {}",
            "bind a { }",
            "bind a {display}",
            "bind a { display a;display b;; display c}",
            "bind a { A=1 }",
            "bind a { %hidden A=1\ndisplay a }",
            "bind a display \"a}\"",
            "bind a display a\\}",
            "bind a display '{'",
            "bind a { display a } ; bind b { display b }",
            "bind a { display a } display b",
        ] {
            assert!(parse(text).is_ok(), "{text}: {:?}", parse(text).err());
        }
    }

    #[test]
    fn print_regroups_flags_and_expands_aliases() {
        let p = |text: &str| parse(text).unwrap()[0].print();
        assert_eq!(p("choose-tree -sZ -O name"), "choose-tree -Zs -O name");
        assert_eq!(p("send -X cancel"), "send-keys -X cancel");
        assert_eq!(p("splitp -h"), "split-window -h");
        assert_eq!(p("display"), "display-message");
        assert_eq!(p("send -N3 -X -l foo"), "send-keys -Xl -N 3 foo");
        assert_eq!(p("resize-pane -D -D5"), "resize-pane -D --");
        assert_eq!(p("display ~/x"), "display-message /home/me/x");
        assert_eq!(p("display 'a $b'"), "display-message \"a \\$b\"");
    }

    // ---- robustness ----

    /// xorshift64*: a fixed-seed generator, so failures replay.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
        }

        fn below(&mut self, n: usize) -> usize {
            usize::try_from(self.next() % n as u64).unwrap()
        }

        /// Random UTF-8 biased toward the characters the lexer cares about.
        fn string(&mut self, max: usize) -> String {
            const SPICY: &[&str] = &[
                " ", "\t", "\n", "\r", ";", "\\", "'", "\"", "#", "{", "}", "$", "~", "%", "-",
                "=", "\\u", "\\U", "\\0", "\\x", "%if", "%endif", "${", "#{", "display", "-p",
                "-t", "é", "😀", "\0", "\\;",
            ];
            let len = self.below(max + 1);
            let mut s = String::new();
            for _ in 0..len {
                if self.below(3) == 0 {
                    s.push_str(SPICY[self.below(SPICY.len())]);
                } else {
                    let c = loop {
                        let cp = u32::try_from(self.next() % 0x11_0000).unwrap();
                        if let Some(c) = char::from_u32(cp) {
                            break c;
                        }
                    };
                    s.push(c);
                }
            }
            s
        }
    }

    #[test]
    fn config_never_panics_on_random_utf8() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        let mut env = env();
        for _ in 0..10_000 {
            let s = rng.string(40);
            let _ = parse_config_with(&s, &mut env, RUN);
        }
    }

    #[test]
    fn argv_never_panics_on_random_words() {
        let mut rng = Rng(0xdead_beef_cafe_f00d);
        let names = [
            "display", "new", "has", "ls", "send", "capturep", "resizep", "-", "--", "",
        ];
        for _ in 0..10_000 {
            let n = rng.below(8);
            let v: Vec<String> = (0..n)
                .map(|i| {
                    if i == 0 && rng.below(2) == 0 {
                        names[rng.below(names.len())].to_owned()
                    } else {
                        rng.string(6)
                    }
                })
                .collect();
            let _ = parse_argv(&v);
        }
    }
}
