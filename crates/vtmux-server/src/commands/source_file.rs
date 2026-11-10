//! `source-file` (cmd-source-file.c) and config loading (cfg.c
//! `load_cfg`): parse a whole file, then run it.
//!
//! The whole file is one parse (`cmd_parse_from_file`), so `%if` blocks
//! span lines, assignments apply while the file is read
//! (cmd-parse.y:224-266), and a command in the file never runs before a
//! later `%if` is decided. Any parse error rejects the file before a
//! command runs. Each command's errors carry `file:line: `.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use vtmux_core::cmd::{self, Args, CmdError, CommandCall, CommandList, ParseOpts, PrintFlags};
use vtmux_core::error::{EdgeError, strerror};

use super::Item;
use super::file_io::Then;
use crate::cmdq::{Groups, Source, StepId, Work};
use crate::server::{GlobalEnv, Server};
use crate::target::Found;

/// `CMD_SOURCE_FILE_DEPTH_LIMIT` (cmd-source-file.c:34).
const DEPTH_LIMIT: usize = 50;

/// `cmd_source_file_exec` (cmd-source-file.c:174-273): each file's
/// commands go into the queue right after this one, then a step that
/// reports the causes (`cmd_source_file_complete_cb`). `verbose`: the
/// command itself was read by a `source-file -v` (`CMD_PARSE_VERBOSE` in
/// its parse flags), which makes it verbose as `-v` does.
pub fn exec(
    server: &mut Server,
    item: &mut Item,
    args: &Args,
    found: Found,
    verbose: bool,
) -> Result<(), CmdError> {
    // Without a client the depth is global, else the client's.
    let depth = server.source_depths.entry(item.client).or_default();
    if *depth >= DEPTH_LIMIT {
        return Err(EdgeError::TooManyNestedFiles.into());
    }
    *depth += 1;
    let quiet = args.flag('q');
    let mut files = Vec::new();
    let mut failed = false;
    for raw in &args.positionals {
        let raw = &*raw.text();
        let path = if args.flag('F') {
            let tc = server.target_client(item.client, item.origin);
            super::expand_from_target(server, found, tc, raw)
        } else {
            raw.to_owned()
        };
        // cmd-source-file.c:225-228: `-` is the client's stdin.
        if path == "-" {
            files.push(File::Stdin);
            continue;
        }
        // Relative paths are against the client's directory, then globbed
        // (cmd-source-file.c:218-256); a miss is reported and the other
        // paths still load.
        // The directory is quoted, so only `path` is a pattern.
        let pattern = if path.starts_with('/') {
            path.clone()
        } else {
            format!("{}/{path}", quote_for_glob(&item.cwd.to_string_lossy()))
        };
        let matches = glob(&pattern);
        if matches.is_empty() && !quiet {
            item.error(
                server,
                vtmux_core::rawstr::to_bytes(&format!("No such file or directory: {path}"))
                    .into_owned(),
            );
            failed = true;
        }
        files.extend(matches.into_iter().map(File::Path));
    }
    let opts = LoadOpts {
        quiet: false,
        parse_only: args.flag('n'),
        // cmd-source-file.c:211-215: not for a control client.
        verbose: (args.flag('v') || verbose) && item.control.is_none(),
        parse_client: None,
    };
    // cmd-source-file.c:264-272: with nothing to read, a miss is the
    // command's error (the rest of its group is dropped); otherwise the
    // command waits for its files and the miss only fails the client.
    if files.is_empty() {
        if server.cfg_finished
            && let Some(step) = item.step
        {
            server.cmdq_insert_work(step, Work::SourceDone { failed });
        }
        return if failed { Err(CmdError::Quiet) } else { Ok(()) };
    }
    let Some(step) = item.step else {
        // No queue to insert into: only parsed (and the causes kept).
        for file in files {
            if let File::Path(path) = file {
                parse(server, item, &path, opts);
            }
        }
        return Ok(());
    };
    files.reverse();
    load_files(
        server,
        item,
        Cont {
            after: step,
            step,
            current: found.session.is_some().then_some(found),
            files,
            opts,
            failed,
        },
    );
    Ok(())
}

/// One file `source-file` reads.
#[derive(Debug)]
enum File {
    Path(PathBuf),
    /// `-`: the client's stdin.
    Stdin,
}

/// Where a `source-file` is in its list of files
/// (`struct cmd_source_file_data`): the step the next file's commands go
/// after, the `source-file` step and its target (none if `-t` found
/// nothing), the files still to read (last first), and whether one failed.
#[derive(Debug)]
pub(crate) struct Cont {
    after: StepId,
    step: StepId,
    current: Option<Found>,
    files: Vec<File>,
    opts: LoadOpts,
    failed: bool,
}

impl Cont {
    /// `load_cfg_from_buffer` (cfg.c): a file's commands go after the
    /// last file's, in a state copied from the `source-file`'s with its
    /// target as their current state (`cmdq_copy_state`): a new session
    /// one of them makes does not become the current one for the rest,
    /// and a hook's formats stay behind.
    fn queue(&mut self, server: &mut Server, groups: Groups) {
        if groups.is_empty() {
            return;
        }
        let state = server.cmdq_copy_state(self.step, self.current);
        self.after = server
            .cmdq_insert_with_state(self.after, groups, Some(&state))
            .unwrap_or(self.after);
    }
}

/// `cmd_source_file_done` for each file in turn: a file's commands go in
/// after the last file's. Stdin is read through the client
/// ([`super::file_io`]): the command waits, and [`stdin_done`] carries on.
/// After the last file, the step that reports the causes.
fn load_files(server: &mut Server, item: &mut Item, mut cont: Cont) {
    while let Some(file) = cont.files.pop() {
        match file {
            File::Path(path) => {
                let before = server.causes.len();
                let groups = parse(server, item, &path, cont.opts);
                cont.failed |= server.causes.len() > before;
                cont.queue(server, groups);
            }
            File::Stdin => {
                // file_read always completes through its callback; the
                // error is the done callback's.
                let _ = super::file_io::read(server, item, None, Then::Source(Box::new(cont)));
                return;
            }
        }
    }
    // cmd_source_file_complete: while the config loads its causes wait for
    // cfg_show_causes, and the depth is never given back.
    if server.cfg_finished {
        server.cmdq_insert_work(
            cont.after,
            Work::SourceDone {
                failed: cont.failed,
            },
        );
    }
}

/// `cmd_source_file_done` for stdin (cmd-source-file.c:115-146): an error
/// is the command's (`strerror: -`), otherwise what was read is parsed as
/// file `-`; then the next file.
pub(crate) fn stdin_done(
    server: &mut Server,
    item: &mut Item,
    mut cont: Cont,
    data: Vec<u8>,
    error: i32,
) {
    if error != 0 {
        let e = std::io::Error::from_raw_os_error(error);
        item.error(
            server,
            vtmux_core::rawstr::to_bytes(&format!("{}: -", strerror(&e))).into_owned(),
        );
    } else if !data.is_empty() {
        let before = server.causes.len();
        let text = vtmux_core::rawstr::from_vec(data);
        let groups = parse_text(server, item, "-", &text, cont.opts);
        cont.failed |= server.causes.len() > before;
        cont.queue(server, groups);
    }
    load_files(server, item, cont);
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

/// `cmd_source_file_quote_for_glob` (cmd-source-file.c): a backslash
/// before every ASCII character but a letter, a digit or `/`.
fn quote_for_glob(path: &str) -> String {
    let mut quoted = String::with_capacity(path.len() * 2);
    for c in path.chars() {
        if c.is_ascii() && !c.is_ascii_alphanumeric() && c != '/' {
            quoted.push('\\');
        }
        quoted.push(c);
    }
    quoted
}

/// Whether a glob(3) pattern has an unescaped `*`, `?` or `[`.
fn has_magic(pattern: &str) -> bool {
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                chars.next();
            }
            '*' | '?' | '[' => return true,
            _ => {}
        }
    }
    false
}

/// A glob(3) pattern without magic as the name it stands for: each
/// backslash escapes the character after it.
fn unescape(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len());
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        out.push(if c == '\\' {
            chars.next().unwrap_or(c)
        } else {
            c
        });
    }
    out
}

/// `glob(3)` for the patterns tmux passes (no flags, so no `{}` or `~`):
/// `*`, `?`, `[...]` and backslash escapes per component, results sorted.
/// A pattern without magic matches the name it stands for if that exists.
fn glob(pattern: &str) -> Vec<PathBuf> {
    if !has_magic(pattern) {
        let path = PathBuf::from(unescape(pattern));
        return if path.exists() {
            vec![path]
        } else {
            Vec::new()
        };
    }
    let mut current = vec![PathBuf::from("/")];
    for comp in pattern.split('/').filter(|c| !c.is_empty()) {
        let mut next = Vec::new();
        for dir in &current {
            if has_magic(comp) {
                let Ok(entries) = std::fs::read_dir(dir) else {
                    continue;
                };
                let mut names: Vec<String> = entries
                    .filter_map(|e| e.ok()?.file_name().into_string().ok())
                    .filter(|n| !n.starts_with('.') || comp.starts_with('.'))
                    .filter(|n| vtmux_core::fnmatch::matches(comp, n, false))
                    .collect();
                names.sort();
                next.extend(names.into_iter().map(|n| dir.join(n)));
            } else {
                let p = dir.join(unescape(comp));
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
    let read = std::fs::File::open(path).and_then(|mut f| {
        let mut t = Vec::new();
        f.read_to_end(&mut t).map(|_| t)
    });
    let text = match read {
        Ok(t) => vtmux_core::rawstr::from_vec(t),
        Err(e) => {
            if opts.parse_client.is_some() {
                // `load_cfg` (cfg.c:110-113): the startup config's cause.
                if !(opts.quiet && e.kind() == std::io::ErrorKind::NotFound) {
                    server.causes.push(format!("{file}: {}", strerror(&e)));
                }
            } else {
                // `cmd_source_file_done` (cmd-source-file.c:131-132): the
                // file was read through `file_read`, so a failure is the
                // command's own error, `strerror: path`. A command client
                // reads it itself and its read error (a directory) is EIO
                // (file.c `file_read_error_callback`); an attached client's
                // server-side `fread` gives the real errno.
                let text = if e.raw_os_error() == Some(libc::EISDIR)
                    && !super::file_io::is_attached(server, item)
                {
                    "Input/output error".to_owned()
                } else {
                    strerror(&e)
                };
                item.error(
                    server,
                    vtmux_core::rawstr::to_bytes(&format!("{text}: {file}")).into_owned(),
                );
            }
            return Vec::new();
        }
    };
    parse_text(server, item, &file, &text, opts)
}

/// Parse `text`, read from `file`, as [`parse`] does a file's.
fn parse_text(
    server: &mut Server,
    item: &mut Item,
    file: &str,
    text: &str,
    opts: LoadOpts,
) -> Groups {
    let parse = ParseOpts {
        parse_only: opts.parse_only,
        one_group: false,
        // Only the startup config has a parse client, and only `load_cfg`
        // reads with `cmd_parse_from_file` (cfg.c:96-123); `source-file`
        // goes through `load_cfg_from_buffer`. Byte 0xff tells them apart.
        from_file: opts.parse_client.is_some(),
        verbose: opts.verbose,
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
    let parsed = cmd::parse_file(text, &mut env, parse, Some(&mut collect));
    if opts.verbose {
        // cmd_parse_print_commands (cmd-parse.y:613-625): groups built
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
            vtmux_core::rawstr::from_bytes(&msg)
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
                file: file.to_owned(),
                line,
            });
            (Some(source), calls)
        })
        .collect()
}
