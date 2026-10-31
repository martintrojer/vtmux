//! `source-file` (cmd-source-file.c) and config loading (cfg.c
//! `load_cfg`): parse a whole file, then run it.
//!
//! The whole file is one parse (`cmd_parse_from_file`), so `%if` blocks
//! span lines, assignments apply while the file is read
//! (cmd-parse.y:224-270), and a command in the file never runs before a
//! later `%if` is decided. Any parse error rejects the file before a
//! command runs. Each command's errors carry `file:line: `.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use vtmux_core::cmd::{self, Args, CmdError, CommandCall, CommandList, ParseOpts, PrintFlags};
use vtmux_core::error::{EdgeError, strerror};

use super::Item;
use crate::cmdq::{Groups, Source, Work};
use crate::server::{GlobalEnv, Server};
use crate::target::Found;

/// `CMD_SOURCE_FILE_DEPTH_LIMIT` (cmd-source-file.c:34).
const DEPTH_LIMIT: usize = 50;

/// `cmd_source_file_exec` (cmd-source-file.c:174-273): each file's
/// commands go into the queue right after this one, then a step that
/// reports the causes (`cmd_source_file_complete_cb`).
pub fn exec(
    server: &mut Server,
    item: &mut Item,
    args: &Args,
    found: Found,
) -> Result<(), CmdError> {
    // Without a client the depth is global, else the client's.
    let depth = server.source_depths.entry(item.client).or_default();
    if *depth >= DEPTH_LIMIT {
        return Err(EdgeError::TooManyNestedFiles.into());
    }
    *depth += 1;
    let quiet = args.flag('q');
    let mut paths = Vec::new();
    let mut failed = false;
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
            failed = true;
        }
        paths.extend(matches);
    }
    let opts = LoadOpts {
        quiet: false,
        parse_only: args.flag('n'),
        // cmd-source-file.c:211-215: not for a control client.
        verbose: args.flag('v') && item.control.is_none(),
        parse_client: None,
    };
    let mut groups = Vec::new();
    for path in paths {
        let before = server.causes.len();
        groups.extend(parse(server, item, &path, opts));
        failed |= server.causes.len() > before;
    }
    let Some(step) = item.step else {
        return Ok(());
    };
    let last = server.cmdq_insert_commands(step, groups).unwrap_or(step);
    // cmd_source_file_complete: while the config loads its causes wait
    // for cfg_show_causes, and the depth is never given back.
    if server.cfg_finished {
        server.cmdq_insert_work(last, Work::SourceDone { failed });
    }
    Ok(())
}

/// `cmd_source_file_complete_cb` (cmd-source-file.c:77-94): one level
/// less deep, and the causes printed (`cfg_print_causes`): a control
/// client gets `%config-error` lines, any other client stdout. A file
/// that did not load makes an unattached client fail
/// (cmd-source-file.c:105-108).
pub fn done(server: &mut Server, item: &mut Item, failed: bool) {
    if let Some(depth) = server.source_depths.get_mut(&item.client) {
        *depth = depth.saturating_sub(1);
    }
    if failed
        && let Some(c) = item.client
        && server.client_session(c).is_none()
    {
        item.status = 1;
    }
    for cause in std::mem::take(&mut server.causes) {
        match item.client.filter(|&c| server.is_control(c)) {
            Some(c) => server.control_config_error(c, &cause),
            None => item.print(&cause),
        }
    }
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

/// `load_cfg`: read and parse one file (printing it for `-v`), its
/// groups (one per line) to queue; none if it failed (a cause is queued)
/// or only parses.
pub fn parse(server: &mut Server, item: &mut Item, path: &Path, opts: LoadOpts) -> Groups {
    let file = path.to_string_lossy().into_owned();
    let text = match std::fs::read(path) {
        Ok(t) => String::from_utf8_lossy(&t).into_owned(),
        Err(e) => {
            if !(opts.quiet && e.kind() == std::io::ErrorKind::NotFound) {
                server.causes.push(format!("{file}: {}", strerror(&e)));
            }
            return Vec::new();
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
        let msg = crate::cmdq::error_bytes(&e.error);
        server.causes.push(format!(
            "{file}:{}: {}",
            e.line,
            String::from_utf8_lossy(&msg)
        ));
        return Vec::new();
    }
    if opts.parse_only {
        return Vec::new();
    }
    groups
        .into_iter()
        .map(|(line, calls)| {
            let source = Rc::new(Source {
                file: file.clone(),
                line,
            });
            (Some(source), calls)
        })
        .collect()
}
