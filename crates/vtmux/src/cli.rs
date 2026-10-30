//! Global options, parsed as tmux's `main` does (tmux.c:425-494) with
//! OpenBSD `getopt` semantics (tmux builds its own, configure.ac:233): no
//! permutation, options end at the first non-option or `--`, clustered
//! flags, and an option argument either attached or the next word.

use std::ffi::OsString;
use std::path::PathBuf;

use vtmux_core::protocol::ControlMode;

/// `getopt(argc, argv, "2c:CDdf:hlL:NqS:T:uUvV")`.
const OPTSTRING: &str = "2c:CDdf:hlL:NqS:T:uUvV";

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Cli {
    pub control: ControlMode,
    /// `-c shell-command`.
    pub shell_command: Option<String>,
    /// `-D`: do not fork the server.
    pub no_fork: bool,
    /// Every `-f` file, in order (the first `-f` replaces the defaults).
    pub config_files: Vec<PathBuf>,
    pub label: Option<String>,
    pub socket: Option<PathBuf>,
    pub no_start: bool,
    pub utf8: bool,
    /// `-T` feature strings and `-2` (`256`), parsed by the client task.
    pub features: Vec<String>,
    /// The command words after the options.
    pub command: Vec<String>,
}

/// What `main` does instead of running a command.
#[derive(Debug, PartialEq, Eq)]
pub enum Early {
    /// `-V`.
    Version,
    /// `-h`: usage on stdout, exit 0.
    Help,
    /// A bad option: the `getopt` message (if any) then usage on stderr,
    /// exit 1.
    Usage(Option<String>),
}

/// `usage()` (tmux.c:70-78).
#[must_use]
pub fn usage(progname: &str) -> String {
    format!(
        "usage: {progname} [-2CDhlNuVv] [-c shell-command] [-f file] [-L socket-name]\n            [-S socket-path] [-T features] [command [flags]]\n"
    )
}

/// Parse `args` (without argv[0]). `progname` is for getopt's messages.
pub fn parse(args: &[OsString], progname: &str) -> Result<Cli, Early> {
    let mut cli = Cli::default();
    let words: Vec<String> = args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let mut i = 0;
    let mut f_seen = false;
    'words: while i < words.len() {
        let word = &words[i];
        if word == "--" {
            i += 1;
            break;
        }
        if !word.starts_with('-') || word.len() == 1 {
            break;
        }
        i += 1;
        let bytes = word.as_bytes();
        let mut pos = 1;
        while pos < bytes.len() {
            let opt = char::from(bytes[pos]);
            pos += 1;
            let Some(at) = OPTSTRING.find(opt).filter(|_| opt != ':') else {
                return Err(Early::Usage(Some(format!(
                    "{progname}: unknown option -- {opt}"
                ))));
            };
            let takes_arg = OPTSTRING.as_bytes().get(at + 1) == Some(&b':');
            let value = if takes_arg {
                let v = if pos < bytes.len() {
                    word[pos..].to_owned()
                } else if let Some(next) = words.get(i) {
                    i += 1;
                    next.clone()
                } else {
                    return Err(Early::Usage(Some(format!(
                        "{progname}: option requires an argument -- {opt}"
                    ))));
                };
                pos = bytes.len();
                Some(v)
            } else {
                None
            };
            match (opt, value) {
                ('2', _) => cli.features.push("256".to_owned()),
                ('c', Some(v)) => cli.shell_command = Some(v),
                ('C', _) => {
                    cli.control = match cli.control {
                        ControlMode::None => ControlMode::Control,
                        _ => ControlMode::ControlControl,
                    };
                }
                ('D', _) => cli.no_fork = true,
                ('f', Some(v)) => {
                    if !f_seen {
                        f_seen = true;
                        cli.config_files.clear();
                    }
                    cli.config_files.push(PathBuf::from(v));
                }
                ('h', _) => return Err(Early::Help),
                ('V', _) => return Err(Early::Version),
                // -l (login shell for -c) and -v (logging) change nothing
                // vtmux does yet; accepted like -q.
                ('l' | 'q' | 'v', _) => {}
                ('L', Some(v)) => cli.label = Some(v),
                ('N', _) => cli.no_start = true,
                ('S', Some(v)) => cli.socket = Some(PathBuf::from(v)),
                ('T', Some(v)) => cli.features.push(v),
                ('u', _) => cli.utf8 = true,
                // `d` and `U` are in the option string but have no case:
                // getopt accepts them and the default branch prints usage.
                _ => return Err(Early::Usage(None)),
            }
            if takes_arg {
                continue 'words;
            }
        }
    }
    cli.command = words[i..].to_vec();
    // tmux.c:491-494.
    if (cli.shell_command.is_some() || cli.no_fork) && !cli.command.is_empty() {
        return Err(Early::Usage(None));
    }
    Ok(cli)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Result<Cli, Early> {
        let args: Vec<OsString> = args.iter().map(OsString::from).collect();
        parse(&args, "tmux")
    }

    #[test]
    fn options_then_command() {
        let cli = p(&["-L", "x", "-f/dev/null", "new", "-d", "-s", "a"]).unwrap();
        assert_eq!(cli.label.as_deref(), Some("x"));
        assert_eq!(cli.config_files, vec![PathBuf::from("/dev/null")]);
        assert_eq!(cli.command, ["new", "-d", "-s", "a"]);
        // getopt stops at the first non-option: `-V` after `ls` is the
        // command's.
        assert_eq!(p(&["ls", "-V"]).unwrap().command, ["ls", "-V"]);
        assert_eq!(p(&["--", "-x"]).unwrap().command, ["-x"]);
        assert_eq!(p(&["-CC"]).unwrap().control, ControlMode::ControlControl);
        // `-fL`: L is -f's argument.
        assert_eq!(p(&["-fL"]).unwrap().config_files, vec![PathBuf::from("L")]);
    }

    /// Reference: `tmux -Z` -> `tmux: unknown option -- Z` + usage, exit 1;
    /// `tmux -L` -> `tmux: option requires an argument -- L`; `-U`, `-d`,
    /// `-c x ls` and `-D ls` -> usage only.
    #[test]
    fn errors() {
        assert_eq!(
            p(&["-Z"]),
            Err(Early::Usage(Some("tmux: unknown option -- Z".into())))
        );
        assert_eq!(
            p(&["-L"]),
            Err(Early::Usage(Some(
                "tmux: option requires an argument -- L".into()
            )))
        );
        for args in [&["-U"][..], &["-d"], &["-c", "x", "ls"], &["-D", "ls"]] {
            assert_eq!(p(args), Err(Early::Usage(None)), "{args:?}");
        }
        assert_eq!(p(&["-V"]), Err(Early::Version));
        assert_eq!(p(&["-h"]), Err(Early::Help));
    }
}
