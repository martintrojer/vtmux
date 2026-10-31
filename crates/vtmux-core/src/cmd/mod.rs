//! Command language: the command table, argument parsing, and the two ways
//! tmux turns text into commands.
//!
//! - [`parse_argv`]: words already split by the shell (`tmux display -p x`).
//!   No quotes, comments or escapes are processed; a word ending in `;` ends
//!   a command (cmd-parse.y `cmd_parse_from_arguments`, 1065-1136).
//! - [`parse_config_with`]: tmux's own lexer for config files and command
//!   strings: quotes, escapes, `#` comments, `;` separators, `NAME=value`.
//!   `$VAR` and `~user` go through a [`ConfigEnv`]; this crate does no I/O.
//!
//! Both end in the same step (`cmd_parse_build_command`): expand a default
//! `command-alias`, look the command up (cmd.c `cmd_find`), and split its
//! arguments (arguments.c `args_parse`). That step checks flags and arity
//! only. Everything tmux interprets at execution time (numbers, sizes,
//! targets, formats) stays raw text in [`Args`] for the server to interpret.
//!
//! `{ ... }` blocks parse to nested [`CommandList`]s ([`ArgValue::Commands`]),
//! and [`print`] turns commands back into the canonical text `list-keys`
//! shows (`cmd_list_print`, `args_print`, `args_escape`).
//!
//! Errors are [`CmdError`]. Most are [`EdgeError`]s; the few tmux messages
//! `EdgeError` has no variant for live here until it grows them.

pub mod args;
#[cfg(test)]
mod golden;
pub mod parse;
pub mod print;
pub mod table;
pub mod target;

use std::fmt;

pub use args::{ArgValue, Args, NumError, strtonum};
pub use parse::{
    ConfigEnv, Group, NoEnv, ParseError, ParseOpts, PrintGroup, parse_argv, parse_argv_bytes,
    parse_argv_values, parse_config_with, parse_file, parse_string,
};
pub use print::{PrintFlags, args_escape};
pub use table::{CmdEntry, FindFlags, FindSpec, TMUX_COMMANDS};
pub use target::{TargetKind, TargetSpec};

use crate::error::EdgeError;

/// One parsed command: its table entry and its arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandCall {
    pub entry: &'static CmdEntry,
    pub args: Args,
    /// Which group of its list the command belongs to (`cmd->group`). A
    /// command list printed with groups separates commands of different
    /// groups with ` ;; ` instead of ` ; `. The parser starts a new group at
    /// each source line unless [`ParseOpts::one_group`] is set. Only
    /// equality inside one list means anything.
    pub group: u32,
}

/// A parsed command list (`struct cmd_list`): what a `{ }` block is, and
/// what a key binding runs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommandList {
    pub calls: Vec<CommandCall>,
}

/// Why text did not become commands, or why a command cannot run. `Display`
/// is the exact stderr line (without the `file:line: ` prefix tmux adds when
/// parsing a file; the caller owns that).
#[derive(Debug)]
pub enum CmdError {
    /// Unknown or ambiguous command, unknown flag, arity.
    Edge(EdgeError),
    /// Runtime option validation (cmd-set-option.c, cmd-show-options.c).
    Option(crate::options::OptionError),
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
    /// `command C: argument N must be "string"`: a `{ }` block where the
    /// command's `args_parse` callback wants text (arguments.c:310-316).
    ArgumentNotString { command: String, index: usize },
    /// `command C: -t argument must be a string`: a block as a flag's value
    /// (arguments.c:166).
    FlagArgumentNotString { command: String, flag: char },
    /// Valid tmux syntax that vtmux does not handle yet (a non-UTF-8
    /// argument).
    Unsupported(&'static str),
    /// The command exists in tmux 3.8 but vtmux does not run it yet.
    NotImplemented(String),
    /// The command failed without a message (`unbind-key -q`): the rest of
    /// its group is dropped, nothing is printed.
    Quiet,
}

impl fmt::Display for CmdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Edge(e) => e.fmt(f),
            Self::Option(e) => e.fmt(f),
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
            Self::ArgumentNotString { command, index } => {
                write!(f, "command {command}: argument {index} must be \"string\"")
            }
            Self::FlagArgumentNotString { command, flag } => {
                write!(f, "command {command}: -{flag} argument must be a string")
            }
            Self::Usage { name, usage } => write!(f, "usage: {name} {usage}"),
            Self::Syntax => f.write_str("syntax error"),
            Self::Lex(s) => f.write_str(s),
            Self::Unsupported(s) => write!(f, "not supported in vtmux yet: {s}"),
            Self::NotImplemented(name) => write!(f, "not implemented in vtmux yet: {name}"),
            Self::Quiet => Ok(()),
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
            Self::Option(e) => Some(e),
            _ => None,
        }
    }
}

impl From<crate::options::OptionError> for CmdError {
    fn from(e: crate::options::OptionError) -> Self {
        Self::Option(e)
    }
}

impl From<EdgeError> for CmdError {
    fn from(e: EdgeError) -> Self {
        Self::Edge(e)
    }
}
