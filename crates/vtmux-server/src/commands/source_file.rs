//! `source-file` (cmd-source-file.c) and config loading (cfg.c
//! `load_cfg`): parse a whole file, then run it.
//!
//! The file is parsed statement by statement so a `NAME=value` applies to
//! the global environment before later statements are lexed, as tmux's
//! grammar applies assignments while parsing (cmd-parse.y:224-270). Any
//! parse error rejects the file before a command runs. Each command's
//! errors carry `file:line: `.

use std::path::{Path, PathBuf};

use vtmux_core::cmd::{self, Args, CmdError, CommandCall, ParseOpts};
use vtmux_core::error::{EdgeError, strerror};

use super::Item;
use crate::names::args_escape;
use crate::server::{GlobalEnv, Server};
use crate::target::Found;

/// `CMD_SOURCE_FILE_DEPTH_LIMIT` (cmd-source-file.c:34).
const DEPTH_LIMIT: usize = 50;

pub fn exec(
    server: &mut Server,
    item: &mut Item,
    args: &Args,
    found: Found,
) -> Result<(), CmdError> {
    if server.source_depth >= DEPTH_LIMIT {
        return Err(EdgeError::TooManyNestedFiles.into());
    }
    let quiet = args.flag('q');
    let mut paths = Vec::new();
    for raw in &args.positionals {
        let path = if args.flag('F') {
            super::expand(server, found, raw)
        } else {
            raw.clone()
        };
        if path == "-" {
            return Err(EdgeError::NotImplemented("source-file -".into()).into());
        }
        // Relative paths are against the client's directory, then globbed
        // (cmd-source-file.c:218-256); a miss is reported and the other
        // paths still load.
        let pattern = if path.starts_with('/') {
            PathBuf::from(&path)
        } else {
            item.cwd.join(&path)
        };
        let matches = glob(&pattern);
        if matches.is_empty() && !quiet {
            item.error(
                server,
                format!("No such file or directory: {path}").into_bytes(),
            );
        }
        paths.extend(matches);
    }
    let opts = LoadOpts {
        quiet: false,
        parse_only: args.flag('n'),
        verbose: args.flag('v'),
    };
    server.source_depth += 1;
    for path in paths {
        load(server, item, &path, opts);
    }
    server.source_depth -= 1;
    // cmd_source_file_complete_cb -> cfg_print_causes: parse errors are
    // printed (stdout) once every file ran; any makes the client fail.
    let causes = std::mem::take(&mut server.causes);
    if !causes.is_empty() {
        item.status = 1;
    }
    for cause in causes {
        item.print(&cause);
    }
    Ok(())
}

/// How [`load`] treats a file.
#[derive(Clone, Copy, Debug, Default)]
pub struct LoadOpts {
    /// A missing file is not an error (`CMD_PARSE_QUIET`).
    pub quiet: bool,
    /// Parse only (`source-file -n`): nothing runs, assignments are not
    /// applied.
    pub parse_only: bool,
    /// Print each line's commands before running (`-v`).
    pub verbose: bool,
}

/// `glob(3)` for the patterns tmux passes (no `{}` or `~`): `*`, `?`,
/// `[...]` per component, results sorted. A pattern without
/// metacharacters matches itself if it exists.
fn glob(pattern: &Path) -> Vec<PathBuf> {
    let text = pattern.to_string_lossy();
    if !text.contains(['*', '?', '[']) {
        return if pattern.exists() {
            vec![pattern.to_owned()]
        } else {
            Vec::new()
        };
    }
    let mut current = vec![PathBuf::from("/")];
    for comp in text.split('/').filter(|c| !c.is_empty()) {
        let mut next = Vec::new();
        for dir in &current {
            if comp.contains(['*', '?', '[']) {
                let Ok(entries) = std::fs::read_dir(dir) else {
                    continue;
                };
                let mut names: Vec<String> = entries
                    .filter_map(|e| e.ok()?.file_name().into_string().ok())
                    .filter(|n| !n.starts_with('.') || comp.starts_with('.'))
                    .filter(|n| crate::target::fnmatch(comp, n))
                    .collect();
                names.sort();
                next.extend(names.into_iter().map(|n| dir.join(n)));
            } else {
                let p = dir.join(comp);
                if p.exists() {
                    next.push(p);
                }
            }
        }
        current = next;
    }
    current
}

/// `cmd_print`: the command's canonical text for `-v`.
fn print_command(call: &CommandCall) -> String {
    let args = &call.args;
    let mut parts: Vec<String> = Vec::new();
    let bare: String = args
        .flags
        .iter()
        .filter(|(_, v)| v.iter().all(Option::is_none))
        .flat_map(|(f, v)| std::iter::repeat_n(*f, v.len()))
        .collect();
    if !bare.is_empty() {
        parts.push(format!("-{bare}"));
    }
    for (f, values) in &args.flags {
        for v in values.iter().flatten() {
            parts.push(format!("-{f} {}", args_escape(v)));
        }
    }
    parts.extend(args.positionals.iter().map(|p| args_escape(p)));
    if parts.is_empty() {
        call.entry.name.to_owned()
    } else {
        format!("{} {}", call.entry.name, parts.join(" "))
    }
}

/// Read, parse and (unless `parse_only`) run one file for `item`'s
/// client (`None` while loading the startup config). Read and parse errors
/// are queued as causes (`cfg_add_cause`); runtime errors go to the client
/// as any command's do, or become `file:line:` causes without a client.
pub fn load(server: &mut Server, item: &mut Item, path: &Path, opts: LoadOpts) {
    let file = path.to_string_lossy().into_owned();
    let text = match std::fs::read(path) {
        Ok(t) => String::from_utf8_lossy(&t).into_owned(),
        Err(e) => {
            if !(opts.quiet && e.kind() == std::io::ErrorKind::NotFound) {
                server.causes.push(format!("{file}: {}", strerror(&e)));
            }
            return;
        }
    };
    let parse = ParseOpts {
        parse_only: opts.parse_only,
    };
    // Each line is its own group (cmd-parse.y cmd_parse_build_commands):
    // an error stops the rest of that line only.
    let mut groups: Vec<(usize, Vec<CommandCall>)> = Vec::new();
    let mut line_no = 1;
    for stmt in split_statements(&text) {
        match cmd::parse_config_with(&stmt.text, &mut GlobalEnv(&mut server.global_env), parse) {
            Ok(calls) if calls.is_empty() => {}
            Ok(calls) => groups.push((line_no, calls)),
            Err(e) => {
                let msg = match &e {
                    CmdError::Edge(edge) => edge.to_bytes(),
                    other => other.to_bytes(),
                };
                server.causes.push(format!(
                    "{file}:{line_no}: {}",
                    String::from_utf8_lossy(&msg)
                ));
                return;
            }
        }
        line_no += stmt.lines;
    }
    if opts.verbose {
        // cmd_parse_print_commands (cmd-parse.y:612-624).
        for (line, calls) in &groups {
            let text: Vec<String> = calls.iter().map(print_command).collect();
            item.print(&format!("{file}:{line}: {}", text.join(" ; ")));
        }
    }
    if opts.parse_only {
        return;
    }
    for (line, calls) in groups {
        let before = server.causes.len();
        super::run(server, item, &calls);
        for cause in &mut server.causes[before..] {
            *cause = format!("{file}:{line}: {cause}");
        }
    }
}

/// A statement of config text and how many physical lines it spans.
struct Statement {
    text: String,
    lines: usize,
}

/// Split config text into newline-terminated statements, keeping quoted
/// newlines and backslash continuations inside one statement, so each can
/// be parsed (and its assignments applied) before the next.
fn split_statements(text: &str) -> Vec<Statement> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut lines = 0;
    let mut quote: Option<char> = None;
    let mut chars = text.chars().peekable();
    let mut comment = false;
    while let Some(c) = chars.next() {
        cur.push(c);
        if comment {
            if c == '\n' {
                comment = false;
            } else {
                continue;
            }
        }
        match (quote, c) {
            (_, '\\') => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                    if n == '\n' {
                        lines += 1;
                    }
                }
            }
            (None, '#')
                if cur.len() == 1 || cur[..cur.len() - 1].ends_with([' ', '\t', '\n', ';']) =>
            {
                comment = true;
            }
            (None, '\'' | '"') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (_, '\n') => {
                lines += 1;
                if quote.is_none() {
                    out.push(Statement {
                        text: std::mem::take(&mut cur),
                        lines,
                    });
                    lines = 0;
                }
            }
            _ => {}
        }
    }
    if !cur.is_empty() {
        out.push(Statement {
            text: cur,
            lines: lines + 1,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statements_split_on_unquoted_newlines() {
        let s = split_statements("a\nb 'x\ny'\nc \\\nd\n# x 'q\ne\n");
        let texts: Vec<_> = s.iter().map(|s| (s.text.as_str(), s.lines)).collect();
        assert_eq!(
            texts,
            vec![
                ("a\n", 1),
                ("b 'x\ny'\n", 2),
                ("c \\\nd\n", 2),
                ("# x 'q\n", 1),
                ("e\n", 1)
            ]
        );
    }
}
