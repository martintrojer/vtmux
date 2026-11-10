//! Global options, parsed as tmux's `main` does (tmux.c:425-494) with
//! OpenBSD `getopt` semantics (tmux builds its own, configure.ac:233): no
//! permutation, options end at the first non-option or `--`, clustered
//! flags, and an option argument either attached or the next word.

use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::PathBuf;

use vtmux_core::protocol::ControlMode;

/// `getopt(argc, argv, "2c:CDdf:hlL:NqS:T:uUvV")`.
const OPTSTRING: &str = "2c:CDdf:hlL:NqS:T:uUvV";

#[derive(Debug, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // one per getopt flag
pub struct Cli {
    pub control: ControlMode,
    /// `-c shell-command`.
    pub shell_command: Option<OsString>,
    /// `-D`: do not fork the server.
    pub no_fork: bool,
    /// `-l`: `-c` and `detach-client -E` run a login shell.
    pub login: bool,
    /// Every `-f` file, in order (the first `-f` replaces the defaults).
    pub config_files: Vec<PathBuf>,
    pub label: Option<OsString>,
    pub socket: Option<PathBuf>,
    pub no_start: bool,
    pub utf8: bool,
    /// `-T` feature strings and `-2` (`256`), parsed by the client task.
    pub features: Vec<String>,
    /// The command words after the options, as given: tmux's argv is C
    /// strings, so a word need not be UTF-8 (`set-buffer` keeps the bytes).
    pub command: Vec<Vec<u8>>,
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
    // Option values are bytes, as tmux's argv is: a socket or config path
    // that is not UTF-8 names that file.
    let words: Vec<&[u8]> = args.iter().map(|a| a.as_bytes()).collect();
    let mut i = 0;
    let mut f_seen = false;
    'words: while i < words.len() {
        let bytes = words[i];
        if bytes == b"--" {
            i += 1;
            break;
        }
        if !bytes.starts_with(b"-") || bytes.len() == 1 {
            break;
        }
        i += 1;
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
                    OsString::from_vec(bytes[pos..].to_vec())
                } else if let Some(next) = words.get(i) {
                    i += 1;
                    OsString::from_vec(next.to_vec())
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
                ('l', _) => cli.login = true,
                // -v (logging) changes nothing vtmux does; accepted like -q.
                ('q' | 'v', _) => {}
                ('L', Some(v)) => cli.label = Some(v),
                ('N', _) => cli.no_start = true,
                ('S', Some(v)) => cli.socket = Some(PathBuf::from(v)),
                ('T', Some(v)) => cli.features.push(v.to_string_lossy().into_owned()),
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
    cli.command = args[i..]
        .iter()
        .map(|a| std::os::unix::ffi::OsStrExt::as_bytes(a.as_os_str()).to_vec())
        .collect();
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
        assert_eq!(cli.label.as_deref(), Some(std::ffi::OsStr::new("x")));
        assert_eq!(cli.config_files, vec![PathBuf::from("/dev/null")]);
        assert_eq!(cli.command, [&b"new"[..], b"-d", b"-s", b"a"]);
        // getopt stops at the first non-option: `-V` after `ls` is the
        // command's.
        assert_eq!(p(&["ls", "-V"]).unwrap().command, [&b"ls"[..], b"-V"]);
        assert_eq!(p(&["--", "-x"]).unwrap().command, [b"-x"]);
        assert_eq!(p(&["-CC"]).unwrap().control, ControlMode::ControlControl);
        assert!(p(&["-l", "-c", "x"]).unwrap().login);
        assert!(!p(&["-c", "x"]).unwrap().login);
        // `-fL`: L is -f's argument.
        assert_eq!(p(&["-fL"]).unwrap().config_files, vec![PathBuf::from("L")]);
    }

    /// Command words are passed on byte for byte (reviewer-36: `setb` of
    /// `\xff\x80` stored EF BF BD twice).
    #[test]
    fn command_words_keep_their_bytes() {
        use std::os::unix::ffi::OsStringExt;
        let args = vec![
            OsString::from("setb"),
            OsString::from_vec(b"a\xc3\xa9\xff\x80".to_vec()),
        ];
        let cli = parse(&args, "tmux").unwrap();
        assert_eq!(
            cli.command,
            [b"setb".to_vec(), b"a\xc3\xa9\xff\x80".to_vec()]
        );
    }

    /// `-S`, `-f`, `-L` and `-c` values are bytes, as tmux's argv is: a
    /// path that is not UTF-8 names that file, not one with U+FFFD in it.
    #[test]
    fn option_values_keep_their_bytes() {
        let raw = |b: &[u8]| OsString::from_vec(b.to_vec());
        let args = [
            raw(b"-S"),
            raw(b"/tmp/s\xff.sock"),
            raw(b"-f\xfe.conf"),
            raw(b"-L"),
            raw(b"l\x80"),
            raw(b"-c"),
            raw(b"echo \xc3"),
        ];
        let cli = parse(&args, "tmux").unwrap();
        assert_eq!(cli.socket, Some(PathBuf::from(raw(b"/tmp/s\xff.sock"))));
        assert_eq!(cli.config_files, [PathBuf::from(raw(b"\xfe.conf"))]);
        assert_eq!(cli.label, Some(raw(b"l\x80")));
        assert_eq!(cli.shell_command, Some(raw(b"echo \xc3")));
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
