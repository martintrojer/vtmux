//! `source-file` (cmd-source-file.c) and config loading (cfg.c
//! `load_cfg`): parse a whole file, then run it.
//!
//! The whole file is one parse (`cmd_parse_from_file`), so `%if` blocks
//! span lines, assignments apply while the file is read
//! (cmd-parse.y:224-270), and a command in the file never runs before a
//! later `%if` is decided. Any parse error rejects the file before a
//! command runs. Each command's errors carry `file:line: `.

use std::path::{Path, PathBuf};

use vtmux_core::cmd::{self, Args, CmdError, CommandCall, CommandList, ParseOpts, PrintFlags};
use vtmux_core::error::{EdgeError, strerror};

use super::Item;
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
        let raw = &*raw.text();
        let path = if args.flag('F') {
            super::expand(server, found, raw)
        } else {
            raw.to_owned()
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
        parse_client: None,
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
    /// The client (and its origin) the file is parsed for when the item
    /// has none: `pi.c` of the startup config (cfg.c:81-90).
    pub parse_client: Option<(crate::server::ClientId, crate::target::Origin)>,
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
        one_group: false,
    };
    // Each line is its own group (cmd-parse.y cmd_parse_build_commands):
    // an error stops the rest of that line only. `pi->c` is the item's
    // client, its state from `cmd_find_from_client` (cfg.c:116-121).
    // `printed` is every group as tmux prints it for -v, `groups` the
    // top-level ones that run.
    let mut printed: Vec<(u32, String)> = Vec::new();
    let mut groups: Vec<(u32, Vec<CommandCall>)> = Vec::new();
    let (tc, origin) = match item.client {
        Some(id) => (Some(id), item.origin),
        None => opts
            .parse_client
            .map_or((None, item.origin), |(id, o)| (Some(id), o)),
    };
    let tc = tc.filter(|id| server.clients.contains_key(id));
    let ctx = server.format_ctx(Found::default(), tc);
    let (client, clients) = (ctx.client, ctx.clients);
    let mut env = GlobalEnv::new(&mut server.global_env, &server.model, &server.info)
        .for_client(origin, client, clients);
    let mut collect = |group: cmd::Group<'_>| {
        let calls = group.calls.to_vec();
        if opts.verbose {
            let list = CommandList {
                calls: calls.clone(),
            };
            printed.push((group.line, list.print(PrintFlags::default())));
        }
        if !group.nested {
            groups.push((group.line, calls));
        }
    };
    let parsed = cmd::parse_file(&text, &mut env, parse, Some(&mut collect));
    if opts.verbose {
        // cmd_parse_print_commands (cmd-parse.y:612-624): groups built
        // before an error are printed too.
        for (line, text) in &printed {
            item.print(&format!("{file}:{line}: {text}"));
        }
    }
    if let Err(e) = parsed {
        let msg = match &e.error {
            CmdError::Edge(edge) => edge.to_bytes(),
            other => other.to_bytes(),
        };
        server.causes.push(format!(
            "{file}:{}: {}",
            e.line,
            String::from_utf8_lossy(&msg)
        ));
        return;
    }
    if opts.parse_only {
        return;
    }
    for (line, calls) in groups {
        let before = server.causes.len();
        // wait-for refuses to block inside a file (wait_for.rs).
        let resume = super::run(server, item, &calls);
        debug_assert!(resume.is_none());
        for cause in &mut server.causes[before..] {
            *cause = format!("{file}:{line}: {cause}");
        }
    }
}
