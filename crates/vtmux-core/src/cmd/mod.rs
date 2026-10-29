//! Command language: the command table, argument parsing, and the two ways
//! tmux turns text into commands.
//!
//! - [`parse_argv`]: words already split by the shell (`tmux display -p x`).
//!   No quotes, comments or escapes are processed; a word ending in `;` ends
//!   a command (cmd-parse.y `cmd_parse_from_arguments`, 1065-1136).
//! - [`parse_config_line`]: tmux's own lexer for config files and command
//!   strings: quotes, escapes, `#` comments, `;` separators.
//!
//! Both end in the same step (`cmd_parse_build_command`): expand a default
//! `command-alias`, look the command up (cmd.c `cmd_find`), and split its
//! arguments (arguments.c `args_parse`). That step checks flags and arity
//! only. Everything tmux interprets at execution time (numbers, sizes,
//! targets, formats) stays raw text in [`Args`] for the server to interpret.
//!
//! Errors are [`CmdError`]. Most are [`EdgeError`]s; the few tmux messages
//! `EdgeError` has no variant for live here until it grows them.

pub mod args;
pub mod parse;
pub mod table;
pub mod target;

use std::fmt;

pub use args::{Args, NumError, strtonum};
pub use parse::{ConfigEnv, ProcessEnv, parse_argv, parse_config_line, parse_config_with};
pub use table::{CmdEntry, FindFlags, FindSpec, TMUX_COMMANDS};
pub use target::{TargetKind, TargetSpec};

use crate::error::EdgeError;

/// One parsed command: its table entry and its raw arguments.
#[derive(Clone, Debug)]
pub struct CommandCall {
    pub entry: &'static CmdEntry,
    pub args: Args,
}

/// Why text did not become commands, or why a command cannot run. `Display`
/// is the exact stderr line (without the `file:line: ` prefix tmux adds when
/// parsing a file; the caller owns that).
#[derive(Debug)]
pub enum CmdError {
    /// Unknown or ambiguous command, unknown flag, arity.
    Edge(EdgeError),
    /// `command C: -t expects an argument` (arguments.c:181).
    FlagNeedsValue { command: String, flag: char },
    /// `command C: invalid flag -X`: a non-alphanumeric flag byte
    /// (arguments.c:228). The byte is the first byte of the offending
    /// character.
    InvalidFlag { command: String, flag: u8 },
    /// `-?` on any command: tmux prints `usage: name usage` (cmd.c:520-523).
    Usage {
        name: &'static str,
        usage: &'static str,
    },
    /// The lexer or grammar rejected the text without a better message.
    Syntax,
    /// A lexer message (`invalid octal escape`, `invalid \u argument`, ...).
    Lex(&'static str),
    /// Valid tmux syntax that vtmux M0 does not handle yet (`{ }` blocks,
    /// `%if`, assignments, a non-UTF-8 argument).
    Unsupported(&'static str),
    /// The command exists in tmux 3.8 but vtmux does not run it yet.
    NotImplemented(String),
}

impl fmt::Display for CmdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Edge(e) => e.fmt(f),
            Self::FlagNeedsValue { command, flag } => {
                write!(f, "command {command}: -{flag} expects an argument")
            }
            Self::InvalidFlag { command, flag } => {
                write!(f, "command {command}: invalid flag -")?;
                if flag.is_ascii() {
                    write!(f, "{}", char::from(*flag))
                } else {
                    f.write_str("\u{fffd}")
                }
            }
            Self::Usage { name, usage } => write!(f, "usage: {name} {usage}"),
            Self::Syntax => f.write_str("syntax error"),
            Self::Lex(s) => f.write_str(s),
            Self::Unsupported(s) => write!(f, "not supported in vtmux yet: {s}"),
            Self::NotImplemented(name) => write!(f, "not implemented in vtmux yet: {name}"),
        }
    }
}

impl CmdError {
    /// The stderr line with the exact bytes tmux would print: an invalid
    /// flag byte from a multi-byte character is one raw byte there.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        match self {
            Self::InvalidFlag { command, flag } => {
                let mut out = format!("command {command}: invalid flag -").into_bytes();
                out.push(*flag);
                out
            }
            _ => self.to_string().into_bytes(),
        }
    }
}

impl std::error::Error for CmdError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Edge(e) => Some(e),
            _ => None,
        }
    }
}

impl From<EdgeError> for CmdError {
    fn from(e: EdgeError) -> Self {
        Self::Edge(e)
    }
}
