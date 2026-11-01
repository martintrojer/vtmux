//! The status-line prompt's editor (prompt.c): the line being typed, the
//! cursor in it, emacs and vi (`status-keys`) editing, history recall, and
//! the `-1`, `-N`, `-k` and `-e` kinds of prompt.
//!
//! The editor does not run anything. A key gives a [`KeyOutcome`]; the
//! prompt's owner (command-prompt, confirm-before) decides what a finished
//! line does and whether the prompt stays open for another one
//! (`prompt_fire_callback`).
//!
//! Not here yet: incremental (`-i`) and pane (`-P`) prompts, Tab
//! completion, mouse.

use vtmux_core::keys::Key;
use vtmux_core::width::char_width;

use crate::prompt_history::{History, Position, PromptType};

/// How a prompt behaves (`PROMPT_*` flags); `-1`, `-N`, `-k` and `-e`
/// exclude each other (cmd-command-prompt.c:212-221).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Kind {
    #[default]
    Line,
    /// `-1` (`PROMPT_SINGLE`): one key, as a character.
    Single,
    /// `-N` (`PROMPT_NUMERIC`): digits; any other key accepts the prompt
    /// and goes on to the key tables.
    Numeric,
    /// `-k` (`PROMPT_KEY`): one key, as its name.
    Key,
    /// `-e` (`PROMPT_BSPACE_EXIT`): Backspace on an empty line cancels.
    BackspaceExit,
}

/// What a key did to the prompt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyOutcome {
    /// Edited, moved, or ignored; the key is used up.
    Handled,
    /// The line is finished (`None`: cancelled); the owner says whether the
    /// prompt closes (`prompt_done`).
    Done(Option<String>),
    /// `-k` or `-N`: the owner gets the line, then the prompt closes
    /// whatever it says. With `pass_on` the key goes on to the key tables
    /// (`PROMPT_KEY_NOT_HANDLED`, prompt.c:1242-1249).
    DoneClose { value: String, pass_on: bool },
    /// `-1` with more than one character: closed without the owner seeing
    /// a line (prompt.c:1525-1528).
    Close,
}

/// What a key needs from outside the prompt.
pub struct KeyEnv<'a> {
    pub history: &'a mut History,
    /// `prompt-history-limit`.
    pub history_limit: usize,
    /// The top automatic paste buffer (`paste_get_top(NULL)`).
    pub paste: Option<&'a [u8]>,
}

/// An editing command, from an emacs key or a translated vi one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Left,
    Right,
    Home,
    End,
    Complete,
    Backspace,
    Delete,
    KillLine,
    KillToEnd,
    DeleteWord,
    ForwardWord,
    /// vi `e` (`true`: word-separators count) and `E`.
    EndWord(bool),
    /// vi `w` and `W`.
    ForwardWordVi(bool),
    /// `M-b` / vi `b`, and vi `B`.
    BackwardWord(bool),
    HistoryUp,
    HistoryDown,
    Paste,
    Transpose,
    Enter,
    Cancel,
    /// `C-r` / `C-s`: only incremental prompts use them.
    Search,
    QuoteNext,
}

/// What a vi key becomes (`prompt_translate_key`'s 0, 1, 2).
enum Vi {
    Drop,
    Act(Action),
    Append(Key),
}

/// The prompt's line and its state (`struct prompt`).
#[expect(clippy::struct_excessive_bools, reason = "independent PROMPT_* flags")]
#[derive(Debug)]
pub struct Prompt {
    /// The prompt text, a format (`pr->string`).
    pub label: String,
    buffer: Vec<char>,
    index: usize,
    pub kind: Kind,
    pub ty: PromptType,
    /// `PROMPT_NOFREEZE` (`-C`).
    pub no_freeze: bool,
    /// `status-keys vi`.
    vi: bool,
    /// `PROMPT_COMMANDMODE`: vi command mode.
    command_mode: bool,
    /// `PROMPT_QUOTENEXT`: the next key goes in as it is (`C-v`).
    quote_next: bool,
    word_separators: String,
    /// `pr->hindex`.
    history_at: Position,
    /// What `C-w` cut, which `C-y` pastes before any buffer.
    copied: Option<Vec<char>>,
}

impl Prompt {
    /// `prompt_create` with `input` already expanded.
    #[must_use]
    pub fn new(label: String, input: &str, kind: Kind, ty: PromptType) -> Self {
        let buffer: Vec<char> = input.chars().collect();
        Self {
            label,
            index: buffer.len(),
            buffer,
            kind,
            ty,
            no_freeze: false,
            vi: false,
            command_mode: false,
            quote_next: false,
            word_separators: String::new(),
            history_at: Position::default(),
            copied: None,
        }
    }

    /// `status-keys` and `word-separators` from the session
    /// (`prompt_set_options`).
    #[must_use]
    pub fn with_options(mut self, vi: bool, word_separators: String) -> Self {
        self.vi = vi;
        self.word_separators = word_separators;
        self
    }

    /// `prompt_update`: the next prompt of a multi-prompt command, its
    /// input already expanded.
    pub fn update(&mut self, label: String, input: &str) {
        self.label = label;
        self.buffer = input.chars().collect();
        self.index = self.buffer.len();
        self.history_at = Position::default();
    }

    /// The line typed so far.
    #[must_use]
    pub fn input(&self) -> String {
        self.buffer.iter().collect()
    }

    /// `PROMPT_COMMANDMODE`.
    #[must_use]
    pub fn command_mode(&self) -> bool {
        self.command_mode
    }

    /// `prompt_flags_to_string`, the flags this prompt can have.
    #[must_use]
    pub fn flags_string(&self) -> String {
        let mut flags = Vec::new();
        match self.kind {
            Kind::Single => flags.push("SINGLE"),
            Kind::Numeric => flags.push("NUMERIC"),
            Kind::Key => flags.push("KEY"),
            Kind::Line | Kind::BackspaceExit => {}
        }
        if self.quote_next {
            flags.push("QUOTENEXT");
        }
        if self.kind == Kind::BackspaceExit {
            flags.push("BSPACE_EXIT");
        }
        if self.no_freeze {
            flags.push("NOFREEZE");
        }
        if self.command_mode {
            flags.push("COMMANDMODE");
        }
        flags.join(",")
    }

    /// The variables of the prompt's own format tree
    /// (`prompt_format_tree`).
    #[must_use]
    pub fn format_vars(&self) -> Vec<(&'static str, String)> {
        vec![
            ("prompt_input", self.input()),
            ("prompt_flags", self.flags_string()),
            ("prompt_type", self.ty.name().to_owned()),
            ("command_prompt", u8::from(self.command_mode).to_string()),
        ]
    }

    /// The characters to draw and their columns: control characters show
    /// as `^X` in two columns (`prompt_redraw_character`), and a pending
    /// `C-v` is a `^` at the cursor (`prompt_redraw_quote`).
    #[must_use]
    pub fn cells(&self) -> Vec<(String, u8)> {
        let mut cells: Vec<(String, u8)> = self.buffer.iter().map(|&c| display(c)).collect();
        if self.quote_next {
            cells.insert(self.index, ("^".to_owned(), 1));
        }
        cells
    }

    /// Columns before the cursor (`utf8_strwidth(pr->buffer, pr->index)`).
    #[must_use]
    pub fn cursor_width(&self) -> usize {
        self.buffer[..self.index]
            .iter()
            .map(|&c| usize::from(display(c).1))
            .sum()
    }

    /// `prompt_key`.
    pub fn key(&mut self, key: Key, env: &mut KeyEnv<'_>) -> KeyOutcome {
        if self.kind == Kind::Key {
            return KeyOutcome::DoneClose {
                value: key.to_string(),
                pass_on: false,
            };
        }
        let key = keypad_key(key.without_flags());
        if self.kind == Kind::Numeric {
            if key.as_char().is_some_and(|c| c.is_ascii_digit()) && key.modifiers() == 0 {
                self.insert(key);
                return KeyOutcome::Handled;
            }
            return KeyOutcome::DoneClose {
                value: self.input(),
                pass_on: true,
            };
        }
        if self.kind == Kind::Single || self.quote_next {
            // A key with no character leaves C-v pending (prompt.c:1256).
            let Some(c) = literal(key) else {
                return KeyOutcome::Handled;
            };
            self.quote_next = false;
            return self.append(Key::from_char(c));
        }
        let action = if self.vi {
            match self.translate_vi(key) {
                Vi::Drop => return KeyOutcome::Handled,
                Vi::Append(k) => return self.append(k),
                Vi::Act(a) => a,
            }
        } else {
            match emacs(key) {
                Some(a) => a,
                None => return self.append(key),
            }
        };
        self.act(action, env)
    }

    /// The switch in `prompt_key` (prompt.c:1260-1497).
    fn act(&mut self, action: Action, env: &mut KeyEnv<'_>) -> KeyOutcome {
        let size = self.buffer.len();
        match action {
            Action::Left => self.index = self.index.saturating_sub(1),
            Action::Right => self.index = (self.index + 1).min(size),
            Action::Home => self.index = 0,
            Action::End => self.index = size,
            // prompt_replace_complete: completion is not here yet.
            Action::Complete | Action::Search => {}
            Action::Backspace => {
                if self.kind == Kind::BackspaceExit && size == 0 {
                    return KeyOutcome::Done(None);
                }
                if self.index != 0 {
                    self.index -= 1;
                    self.buffer.remove(self.index);
                }
            }
            Action::Delete => {
                if self.index != size {
                    self.buffer.remove(self.index);
                }
            }
            Action::KillLine => {
                self.buffer.clear();
                self.index = 0;
            }
            Action::KillToEnd => self.buffer.truncate(self.index),
            Action::DeleteWord => {
                let start = self.word_start(&self.word_separators.clone());
                self.copied = Some(self.buffer.drain(start..self.index).collect());
                self.index = start;
            }
            Action::ForwardWord => self.forward_word(false, true),
            Action::ForwardWordVi(seps) => self.forward_word(true, seps),
            Action::EndWord(seps) => self.end_word(seps),
            Action::BackwardWord(seps) => {
                let seps = if seps {
                    self.word_separators.clone()
                } else {
                    String::new()
                };
                self.index = self.word_start(&seps);
            }
            Action::HistoryUp => {
                if let Some(line) = env.history.up(&mut self.history_at, self.ty) {
                    self.set_line(line);
                }
            }
            Action::HistoryDown => {
                let line = env.history.down(&mut self.history_at, self.ty).to_owned();
                self.set_line(&line);
            }
            Action::Paste => self.paste(env.paste),
            Action::Transpose => {
                let idx = if self.index < size {
                    self.index + 1
                } else {
                    self.index
                };
                if idx >= 2 {
                    self.buffer.swap(idx - 2, idx - 1);
                    self.index = idx;
                }
            }
            Action::Enter => {
                let line = self.input();
                if !line.is_empty() {
                    env.history.add(&line, self.ty, env.history_limit);
                }
                return KeyOutcome::Done(Some(line));
            }
            Action::Cancel => return KeyOutcome::Done(None),
            Action::QuoteNext => self.quote_next = true,
        }
        KeyOutcome::Handled
    }

    /// `append_key`: the key as a character at the cursor; a key that is
    /// no character is dropped. A `-1` prompt is done with it.
    fn append(&mut self, key: Key) -> KeyOutcome {
        if !self.insert(key) {
            return KeyOutcome::Handled;
        }
        if self.kind == Kind::Single {
            if self.buffer.len() != 1 {
                return KeyOutcome::Close;
            }
            return KeyOutcome::Done(Some(self.input()));
        }
        KeyOutcome::Handled
    }

    /// Insert `key`'s character: ASCII (with control characters) or an
    /// unmodified Unicode key.
    fn insert(&mut self, key: Key) -> bool {
        let c = if key.code() <= 0x7f {
            key.as_char()
        } else if key.modifiers() == 0 {
            key.as_char().filter(|c| !c.is_ascii())
        } else {
            None
        };
        let Some(c) = c else { return false };
        self.buffer.insert(self.index, c);
        self.index += 1;
        true
    }

    fn set_line(&mut self, line: &str) {
        self.buffer = line.chars().collect();
        self.index = self.buffer.len();
    }

    /// `prompt_paste`: what `C-w` cut, else the top buffer up to its first
    /// control character or invalid byte.
    fn paste(&mut self, top: Option<&[u8]>) {
        let chars: Vec<char> = if let Some(copied) = &self.copied {
            copied.clone()
        } else if let Some(data) = top {
            let valid = match std::str::from_utf8(data) {
                Ok(s) => s,
                Err(e) => std::str::from_utf8(&data[..e.valid_up_to()]).unwrap_or_default(),
            };
            valid.chars().take_while(|c| !c.is_control()).collect()
        } else {
            return;
        };
        let n = chars.len();
        self.buffer.splice(self.index..self.index, chars);
        self.index += n;
    }

    fn space(&self, i: usize) -> bool {
        self.buffer.get(i) == Some(&' ')
    }

    /// `prompt_in_list`: a one-column ASCII character in `seps`.
    fn in_list(&self, seps: &str, i: usize) -> bool {
        self.buffer
            .get(i)
            .is_some_and(|&c| c.is_ascii() && !c.is_ascii_control() && seps.contains(c))
    }

    /// The start of the word before the cursor (`prompt_backward_word`, and
    /// the same walk in `C-w`).
    fn word_start(&self, seps: &str) -> usize {
        let mut idx = self.index;
        while idx != 0 {
            idx -= 1;
            if !self.space(idx) {
                break;
            }
        }
        let separators = self.in_list(seps, idx);
        while idx != 0 {
            idx -= 1;
            if self.space(idx) || separators != self.in_list(seps, idx) {
                idx += 1;
                break;
            }
        }
        idx
    }

    /// `prompt_forward_word`: `vi` stops at the next word's start, emacs
    /// at the end of this one. `seps` false treats only spaces as breaks.
    fn forward_word(&mut self, vi: bool, seps: bool) {
        let seps = if seps {
            self.word_separators.clone()
        } else {
            String::new()
        };
        let size = self.buffer.len();
        let mut idx = self.index;
        if !vi {
            while idx != size && self.space(idx) {
                idx += 1;
            }
        }
        if idx == size {
            self.index = idx;
            return;
        }
        let separators = self.in_list(&seps, idx) && !self.space(idx);
        loop {
            idx += 1;
            if self.space(idx) {
                if vi {
                    while idx != size && self.space(idx) {
                        idx += 1;
                    }
                }
                break;
            }
            if idx == size || separators != self.in_list(&seps, idx) {
                break;
            }
        }
        self.index = idx;
    }

    /// `prompt_end_word`.
    fn end_word(&mut self, seps: bool) {
        let seps = if seps {
            self.word_separators.clone()
        } else {
            String::new()
        };
        let size = self.buffer.len();
        let mut idx = self.index;
        if idx == size {
            return;
        }
        loop {
            idx += 1;
            if idx == size {
                self.index = idx;
                return;
            }
            if !self.space(idx) {
                break;
            }
        }
        let separators = self.in_list(&seps, idx);
        loop {
            idx += 1;
            if idx == size || self.space(idx) || separators != self.in_list(&seps, idx) {
                break;
            }
        }
        self.index = idx - 1;
    }

    /// `prompt_translate_key`: vi insert mode passes editing keys on and
    /// types the rest; command mode maps vi keys to editing commands.
    fn translate_vi(&mut self, key: Key) -> Vi {
        let ch = |c: u8| Key::ascii(c);
        let ctrl = |c: u8| Key::ascii(c).ctrl();
        if !self.command_mode {
            if key == ch(0x1b) || key == ctrl(b'[') {
                self.command_mode = true;
                self.index = self.index.saturating_sub(1);
                return Vi::Drop;
            }
            let passes = [
                ctrl(b'a'),
                ctrl(b'c'),
                ctrl(b'e'),
                ctrl(b'g'),
                ctrl(b'h'),
                ch(b'\t'),
                ctrl(b'k'),
                ctrl(b'n'),
                ctrl(b'p'),
                ctrl(b't'),
                ctrl(b'u'),
                ctrl(b'v'),
                ctrl(b'w'),
                ctrl(b'y'),
                ch(b'\n'),
                ch(b'\r'),
                Key::LEFT.ctrl(),
                Key::RIGHT.ctrl(),
                Key::BSPACE,
                Key::DC,
                Key::DOWN,
                Key::END,
                Key::HOME,
                Key::LEFT,
                Key::RIGHT,
                Key::UP,
            ];
            if passes.contains(&key) {
                return emacs(key).map_or(Vi::Drop, Vi::Act);
            }
            return Vi::Append(key);
        }
        if key == Key::BSPACE {
            return Vi::Act(Action::Left);
        }
        let c = if key.modifiers() == 0 && key.code() <= 0x7f {
            key.as_char()
        } else {
            None
        };
        match c {
            Some('A' | 'I' | 'C' | 's' | 'a') => self.command_mode = false,
            Some('S') => {
                self.command_mode = false;
                return Vi::Act(Action::KillLine);
            }
            Some('i') => {
                self.command_mode = false;
                return Vi::Drop;
            }
            Some('\x1b') => return Vi::Drop,
            _ if key == ctrl(b'[') => return Vi::Drop,
            _ => {}
        }
        let action = match c {
            Some('A' | '$') => Action::End,
            Some('I' | '0' | '^') => Action::Home,
            Some('C' | 'D') => Action::KillToEnd,
            Some('X') => Action::Backspace,
            Some('b') => Action::BackwardWord(true),
            Some('B') => Action::BackwardWord(false),
            Some('d') => Action::KillLine,
            Some('e') => Action::EndWord(true),
            Some('E') => Action::EndWord(false),
            Some('w') => Action::ForwardWordVi(true),
            Some('W') => Action::ForwardWordVi(false),
            Some('p') => Action::Paste,
            Some('q') => Action::Cancel,
            Some('s' | 'x') => Action::Delete,
            Some('j') => Action::HistoryDown,
            Some('h') => Action::Left,
            Some('a' | 'l') => Action::Right,
            Some('k') => Action::HistoryUp,
            Some('\n' | '\r') => Action::Enter,
            _ if key == Key::DC => Action::Delete,
            _ if key == Key::DOWN => Action::HistoryDown,
            _ if key == Key::LEFT => Action::Left,
            _ if key == Key::RIGHT => Action::Right,
            _ if key == Key::UP => Action::HistoryUp,
            _ if key == ctrl(b'h') => Action::Backspace,
            _ if key == ctrl(b'c') => Action::Cancel,
            _ => return Vi::Drop,
        };
        Vi::Act(action)
    }
}

/// The emacs keys (prompt.c:1260-1497); `None` types the key.
fn emacs(key: Key) -> Option<Action> {
    let ch = Key::ascii;
    let ctrl = |c: u8| Key::ascii(c).ctrl();
    let meta = |c: u8| Key::ascii(c).meta();
    let table = [
        (Key::LEFT, Action::Left),
        (ctrl(b'b'), Action::Left),
        (Key::RIGHT, Action::Right),
        (ctrl(b'f'), Action::Right),
        (Key::HOME, Action::Home),
        (ctrl(b'a'), Action::Home),
        (Key::END, Action::End),
        (ctrl(b'e'), Action::End),
        (ch(b'\t'), Action::Complete),
        (Key::BSPACE, Action::Backspace),
        (ctrl(b'h'), Action::Backspace),
        (Key::DC, Action::Delete),
        (ctrl(b'd'), Action::Delete),
        (ctrl(b'u'), Action::KillLine),
        (ctrl(b'k'), Action::KillToEnd),
        (ctrl(b'w'), Action::DeleteWord),
        (Key::RIGHT.ctrl(), Action::ForwardWord),
        (meta(b'f'), Action::ForwardWord),
        (Key::LEFT.ctrl(), Action::BackwardWord(true)),
        (meta(b'b'), Action::BackwardWord(true)),
        (Key::UP, Action::HistoryUp),
        (ctrl(b'p'), Action::HistoryUp),
        (Key::DOWN, Action::HistoryDown),
        (ctrl(b'n'), Action::HistoryDown),
        (ctrl(b'y'), Action::Paste),
        (ctrl(b't'), Action::Transpose),
        (ch(b'\r'), Action::Enter),
        (ch(b'\n'), Action::Enter),
        (ch(0x1b), Action::Cancel),
        (ctrl(b'['), Action::Cancel),
        (ctrl(b'c'), Action::Cancel),
        (ctrl(b'g'), Action::Cancel),
        (ctrl(b'r'), Action::Search),
        (ctrl(b's'), Action::Search),
        (ctrl(b'v'), Action::QuoteNext),
    ];
    table.iter().find(|(k, _)| *k == key).map(|(_, a)| *a)
}

/// A key as `-1` and `C-v` take it (prompt.c:1252-1262): Backspace is
/// DEL, a Ctrl key its control character, Unicode as it is; other keys
/// are dropped.
/// `prompt_keypad_key` (prompt.c:735-775): an unmodified keypad key is
/// its character, `KPEnter` is Enter.
fn keypad_key(key: Key) -> Key {
    const CHARS: &[u8; 16] = b"/*-789+456123\r0.";
    match key.keypad_index() {
        Some(n) if key.modifiers() == 0 => Key::ascii(CHARS[usize::from(n)]),
        _ => key,
    }
}

fn literal(key: Key) -> Option<char> {
    let base = key.base();
    if base == Key::BSPACE {
        return Some('\x7f');
    }
    let c = base.as_char()?;
    if !c.is_ascii() {
        return Some(c);
    }
    if key.has_ctrl() {
        return char::from_u32(u32::from(c) & 0x1f);
    }
    Some(c)
}

/// A character as the prompt draws it, with its columns.
fn display(c: char) -> (String, u8) {
    if c <= '\x1f' || c == '\x7f' {
        let shown = if c == '\x7f' {
            '?'
        } else {
            char::from_u32(u32::from(c) | 0x40).unwrap_or('?')
        };
        return (format!("^{shown}"), 2);
    }
    (c.to_string(), char_width(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(h: &mut History) -> KeyEnv<'_> {
        KeyEnv {
            history: h,
            history_limit: 100,
            paste: Some(b"pa\nste"),
        }
    }

    fn typed(p: &mut Prompt, h: &mut History, keys: &[Key]) -> Vec<KeyOutcome> {
        keys.iter().map(|&k| p.key(k, &mut env(h))).collect()
    }

    fn chars(s: &str) -> Vec<Key> {
        s.chars().map(Key::from_char).collect()
    }

    #[test]
    fn emacs_editing() {
        let mut h = History::default();
        let mut p = Prompt::new(":".into(), "", Kind::Line, PromptType::Command)
            .with_options(false, " -_@".into());
        typed(&mut p, &mut h, &chars("abc def"));
        typed(&mut p, &mut h, &[Key::ascii(b'w').ctrl()]);
        assert_eq!((p.input(), p.index), ("abc ".into(), 4));
        typed(
            &mut p,
            &mut h,
            &[Key::ascii(b'y').ctrl(), Key::ascii(b'y').ctrl()],
        );
        assert_eq!(p.input(), "abc defdef");
        typed(
            &mut p,
            &mut h,
            &[Key::ascii(b'a').ctrl(), Key::DC, Key::RIGHT],
        );
        assert_eq!((p.input(), p.index), ("bc defdef".into(), 1));
        typed(&mut p, &mut h, &[Key::ascii(b't').ctrl()]);
        assert_eq!((p.input(), p.index), ("cb defdef".into(), 2));
        typed(&mut p, &mut h, &[Key::ascii(b'f').meta()]);
        assert_eq!(p.index, 9);
        typed(
            &mut p,
            &mut h,
            &[Key::ascii(b'b').meta(), Key::ascii(b'k').ctrl()],
        );
        assert_eq!(p.input(), "cb ");
        // A Ctrl key that is no command types nothing; C-v types it.
        typed(&mut p, &mut h, &[Key::ascii(b'x').ctrl()]);
        assert_eq!(p.input(), "cb ");
        typed(&mut p, &mut h, &[Key::ascii(b'v').ctrl()]);
        assert_eq!(p.cells().last().unwrap().0, "^");
        typed(&mut p, &mut h, &[Key::ascii(b'x').ctrl()]);
        assert_eq!(p.input(), "cb \x18");
        assert_eq!(p.cursor_width(), 5);
        let out = typed(&mut p, &mut h, &[Key::ascii(b'\r')]);
        assert_eq!(out, [KeyOutcome::Done(Some("cb \x18".into()))]);
        assert_eq!(h.lines(PromptType::Command), ["cb \x18"]);
    }

    #[test]
    fn history_recall_and_cancel() {
        let mut h = History::default();
        h.add("one", PromptType::Command, 100);
        h.add("two", PromptType::Command, 100);
        let mut p = Prompt::new(":".into(), "x", Kind::Line, PromptType::Command);
        typed(&mut p, &mut h, &[Key::UP, Key::UP]);
        assert_eq!(p.input(), "one");
        typed(&mut p, &mut h, &[Key::DOWN, Key::DOWN]);
        assert_eq!(p.input(), "");
        let out = typed(&mut p, &mut h, &[Key::ascii(b'g').ctrl()]);
        assert_eq!(out, [KeyOutcome::Done(None)]);
    }

    #[test]
    fn vi_modes() {
        let mut h = History::default();
        let mut p = Prompt::new(":".into(), "", Kind::Line, PromptType::Command)
            .with_options(true, " ".into());
        typed(&mut p, &mut h, &chars("foo bar"));
        typed(&mut p, &mut h, &[Key::ascii(0x1b)]);
        assert!(p.command_mode());
        assert_eq!(p.index, 6);
        assert_eq!(p.flags_string(), "COMMANDMODE");
        typed(&mut p, &mut h, &chars("0w"));
        assert_eq!(p.index, 4);
        typed(&mut p, &mut h, &chars("x"));
        assert_eq!(p.input(), "foo ar");
        typed(&mut p, &mut h, &chars("iZ"));
        assert_eq!(p.input(), "foo Zar");
        typed(&mut p, &mut h, &[Key::ascii(0x1b)]);
        typed(&mut p, &mut h, &chars("$"));
        assert_eq!(p.index, 7);
        typed(&mut p, &mut h, &chars("bD"));
        assert_eq!(p.input(), "foo ");
        // q in command mode cancels.
        assert_eq!(typed(&mut p, &mut h, &chars("q")), [KeyOutcome::Done(None)]);
    }

    #[test]
    fn single_numeric_key_and_backspace_exit() {
        let mut h = History::default();
        let mut p = Prompt::new("? ".into(), "", Kind::Single, PromptType::Command);
        assert_eq!(
            typed(&mut p, &mut h, &[Key::ascii(b'c').ctrl()]),
            [KeyOutcome::Done(Some("\x03".into()))]
        );
        let mut p = Prompt::new("? ".into(), "a", Kind::Single, PromptType::Command);
        assert_eq!(typed(&mut p, &mut h, &chars("y")), [KeyOutcome::Close]);

        let mut p = Prompt::new("n ".into(), "1", Kind::Numeric, PromptType::Command);
        assert_eq!(
            typed(&mut p, &mut h, &chars("2j")),
            [
                KeyOutcome::Handled,
                KeyOutcome::DoneClose {
                    value: "12".into(),
                    pass_on: true
                }
            ]
        );
        let mut p = Prompt::new("k ".into(), "", Kind::Key, PromptType::Command);
        assert_eq!(
            typed(&mut p, &mut h, &[Key::ascii(b' ').ctrl()]),
            [KeyOutcome::DoneClose {
                value: "C-Space".into(),
                pass_on: false
            }]
        );
        let mut p = Prompt::new("e ".into(), "a", Kind::BackspaceExit, PromptType::Search);
        assert_eq!(
            typed(&mut p, &mut h, &[Key::BSPACE, Key::BSPACE]),
            [KeyOutcome::Handled, KeyOutcome::Done(None)]
        );
    }
}
