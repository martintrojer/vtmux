//! `run-shell` (cmd-run-shell.c) and `if-shell` (cmd-if-shell.c): a shell
//! command as a job; its output printed, or its exit status choosing tmux
//! commands to run.
//!
//! Without `-b` the command waits (`CMD_RETURN_WAIT`) and its queue holds
//! until the job is done; the commands it makes go right after it
//! (`cmdq_insert_after`). With `-b` it returns at once and the commands go
//! at the end of the target client's queue, or the global one
//! (`cmdq_append`).

use std::rc::Rc;
use std::time::{Duration, Instant};

use vtmux_core::cmd::{ArgValue, Args, CmdError, CommandCall};
use vtmux_core::error::EdgeError;

use super::Item;
use crate::cmdq::{End, NewBatch, Source, StepId};
use crate::jobs::{JobSpec, JobStatus};
use crate::server::{ClientId, GlobalEnv, Server};
use crate::target::{Found, Origin};

/// Where a job's commands and messages go once it is done: the waiting
/// step (`cdata->item`), or for `-b` the target client (`cdata->client`).
#[derive(Clone)]
struct Done {
    step: Option<StepId>,
    client: Option<ClientId>,
    origin: Origin,
    source: Option<Rc<Source>>,
}

impl Done {
    fn new(server: &Server, item: &Item, wait: bool) -> Self {
        Self {
            step: item.step.filter(|_| wait),
            client: if wait {
                item.client
            } else {
                server.target_client(item.client, item.origin)
            },
            origin: item.origin,
            source: item.source.clone(),
        }
    }

    /// `cmdq_error(cdata->item, ...)`, or with no item
    /// `status_message_set(c, ...)` with the first letter upper-cased
    /// (cmd-run-shell.c:211-213, cmd-if-shell.c:157-160).
    fn error(&self, server: &mut Server, msg: &str) {
        if let Some(step) = self.step {
            let line = msg.as_bytes().to_vec();
            server.cmdq_with_item(step, |server, item| item.error(server, line));
        } else if let Some(c) = self.client {
            let mut msg = msg.to_owned();
            if let Some(first) = msg.get(..1) {
                let upper = first.to_ascii_uppercase();
                msg.replace_range(..1, &upper);
            }
            server.status_message_set(c, msg);
        }
    }

    /// The commands made from `value` (`args_make_commands`): a `{ }`
    /// block as it is, a string parsed now (for the client found when the
    /// command ran). Queued after the waiting step, or for `-b` at the end
    /// of the client's queue. A parse error is reported instead.
    fn queue(&self, server: &mut Server, value: &ArgValue) {
        let calls = match make_commands(
            server,
            self.client,
            self.origin,
            self.source.as_deref(),
            value,
        ) {
            Ok(calls) => calls,
            Err(msg) => {
                self.error(server, &msg);
                return;
            }
        };
        if let Some(step) = self.step {
            server.cmdq_insert_commands(step, vec![(self.source.clone(), calls)]);
            return;
        }
        let cwd = server.client_cwd(self.client, None);
        let mut item = Item::new(self.client, cwd);
        item.origin = self.origin;
        if let Some(c) = self.client
            && let Some(hello) = server.clients.get(&c).and_then(|c| c.hello.as_ref())
        {
            item.env.clone_from(&hello.env);
            item.utf8 = hello.flags.utf8;
            if server.is_control(c) {
                item.control = Some(0);
            }
        }
        server.cmdq_append(self.client, NewBatch::new(item, calls, End::Quiet));
    }

    /// `cmdq_continue(cdata->item)`.
    fn finish(&self, server: &mut Server) {
        if let Some(step) = self.step {
            server.cmdq_continue(step);
        }
    }
}

/// `args_make_commands` for one argument: a block, or a string parsed
/// with `cmd_parse_from_string` (every command in one group). The parse
/// input carries the command's file and line
/// (`args_make_commands_prepare`, arguments.c:807-809), so a parse error
/// reads `file:line: error` (`cmd_parse_get_error`) before `cmdq_error`
/// adds its own prefix.
fn make_commands(
    server: &mut Server,
    client: Option<ClientId>,
    origin: Origin,
    source: Option<&Source>,
    value: &ArgValue,
) -> Result<Vec<CommandCall>, String> {
    if let ArgValue::Commands(list) = value {
        return Ok(list.calls.clone());
    }
    let text = value.text();
    let ctx = server.format_ctx(Found::default(), client);
    let (view, clients) = (ctx.client, ctx.clients);
    let mut env = GlobalEnv::new(&mut server.global_env, &server.model, &server.info)
        .for_client(origin, view, clients);
    match vtmux_core::cmd::parse_string(&text, &mut env) {
        Ok(list) => Ok(list.calls),
        Err(e) => {
            let msg = String::from_utf8_lossy(&crate::cmdq::error_bytes(&e)).into_owned();
            Err(match source {
                Some(src) => format!("{}:{}: {msg}", src.file, src.line),
                None => msg,
            })
        }
    }
}

/// `cmd_run_shell_exec` (cmd-run-shell.c:107-192).
pub fn exec(
    server: &mut Server,
    item: &mut Item,
    args: &Args,
    found: Found,
) -> Result<(), CmdError> {
    let wait = !args.flag('b');
    let delay = match args.value('d') {
        Some(text) => Some(
            parse_delay(text)
                .ok_or_else(|| EdgeError::Message(format!("invalid delay time: {text}")))?,
        ),
        None if args.positionals.is_empty() => return Ok(()),
        None => None,
    };
    let done = Done::new(server, item, wait);
    // -t names the pane output goes to (cdata->wp_id).
    let to_pane = args.target('t').is_some() && found.pane.is_some();
    let session = found.session;
    let cwd = match args.value('c') {
        Some(c) => std::path::PathBuf::from(super::expand(server, found, c)),
        None => server.client_cwd(item.client, session),
    };
    let now = Instant::now();
    let at = now.checked_add(delay.unwrap_or_default()).unwrap_or(now);
    if args.flag('C') {
        let value = args.positionals.first().cloned();
        server.timer_add(
            at,
            Box::new(move |server| {
                if let Some(value) = value {
                    done.queue(server, &value);
                }
                done.finish(server);
            }),
        );
    } else {
        let command = args.positional_text(0).map(|cmd| {
            // `#{1}`, `#{2}`... are the arguments after the command.
            let mut ctx = server.format_ctx(found, None);
            for (i, arg) in args.positionals.iter().enumerate().skip(1) {
                ctx.extra
                    .push((i.to_string().into(), arg.text().into_owned()));
            }
            vtmux_core::format::expand_str(&cmd, &ctx)
        });
        let spec = command.map(|command| JobSpec {
            command,
            session,
            cwd,
            show_stderr: args.flag('E'),
            nowait: !wait,
        });
        server.timer_add(
            at,
            Box::new(move |server| start_job(server, spec, &done, to_pane)),
        );
    }
    if wait {
        item.blocked = true;
    }
    Ok(())
}

/// `strtod` for `-d` (cmd-run-shell.c:131-136): the whole string, leading
/// space allowed; empty is 0. tmux converts with `(time_t)d`, so a delay
/// past what `time_t` holds (`inf`, `1e30`) fires at once in practice, as
/// do negative ones and NaN.
fn parse_delay(text: &str) -> Option<Duration> {
    // An empty string converts nothing and leaves `end` at its NUL: 0.
    if text.is_empty() {
        return Some(Duration::ZERO);
    }
    let d = strtod(text.trim_start())?;
    // time_t holds seconds up to 2^63; beyond that the conversion is
    // undefined and tmux 3.8 runs the command now.
    if !(d > 0.0 && d < 9.2e18) {
        return Some(Duration::ZERO);
    }
    Some(Duration::try_from_secs_f64(d).unwrap_or(Duration::ZERO))
}

/// `strtod` over a whole string: decimal as Rust parses it (which takes
/// `inf`, `infinity` and `nan` in any case, as strtod does), or a C99 hex
/// integer (`0x10`; hex fractions are rare enough to leave out).
fn strtod(t: &str) -> Option<f64> {
    let (sign, body) = match t.as_bytes().first() {
        Some(b'-') => (-1.0, &t[1..]),
        Some(b'+') => (1.0, &t[1..]),
        _ => (1.0, t),
    };
    if let Some(hex) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
        #[expect(clippy::cast_precision_loss, reason = "strtod rounds too")]
        return u64::from_str_radix(hex, 16).ok().map(|n| sign * n as f64);
    }
    // `nan(chars)` is NaN.
    if body.len() > 4
        && body[..4].eq_ignore_ascii_case("nan(")
        && body.ends_with(')')
        && body[4..body.len() - 1]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Some(f64::NAN);
    }
    t.parse().ok()
}

/// `cmd_run_shell_timer` (cmd-run-shell.c:195-245).
fn start_job(server: &mut Server, spec: Option<JobSpec>, done: &Done, to_pane: bool) {
    let Some(spec) = spec else {
        done.finish(server);
        return;
    };
    let command = spec.command.clone();
    let complete_done = done.clone();
    let complete = Box::new(
        move |server: &mut Server, output: Vec<u8>, status: JobStatus| {
            job_done(server, &complete_done, &command, &output, status, to_pane);
        },
    );
    if server.job_run(&spec, complete).is_err() {
        done.error(server, &format!("failed to run command: {}", spec.command));
        done.finish(server);
    }
}

/// `cmd_run_shell_callback` (cmd-run-shell.c:248-296): each line of
/// output, then a message for a failure, printed; an unattached client
/// takes the job's exit status.
fn job_done(
    server: &mut Server,
    done: &Done,
    command: &str,
    output: &[u8],
    status: JobStatus,
    to_pane: bool,
) {
    let mut lines: Vec<Vec<u8>> = output.split(|&b| b == b'\n').map(<[u8]>::to_vec).collect();
    // A final newline ends the last line; it does not start another.
    if lines.last().is_some_and(Vec::is_empty) {
        lines.pop();
    }
    let retcode = match status {
        JobStatus::Exited(0) => 0,
        JobStatus::Exited(code) => {
            lines.push(format!("'{command}' returned {code}").into_bytes());
            code
        }
        JobStatus::Signaled(sig) => {
            lines.push(format!("'{command}' terminated by signal {sig}").into_bytes());
            sig + 128
        }
    };
    // cmd_run_shell_print: with no pane, the waiting item's client prints
    // (cmdq_print); otherwise the output would go to a pane's view mode,
    // which vtmux has not got (D-006), so it is dropped.
    if let Some(step) = done.step.filter(|_| !to_pane) {
        server.cmdq_with_item(step, |_, item| {
            for line in &lines {
                item.print_line(line);
            }
        });
    }
    if let Some(step) = done.step {
        server.cmdq_with_item(step, |server, item| {
            if let Some(c) = item.client
                && server.client_session(c).is_none()
            {
                item.status = retcode;
            }
        });
    }
    done.finish(server);
}

/// `cmd_if_shell_exec` (cmd-if-shell.c:76-129).
pub fn if_shell(
    server: &mut Server,
    item: &mut Item,
    args: &Args,
    found: Found,
) -> Result<(), CmdError> {
    let wait = !args.flag('b');
    let shellcmd = super::expand(server, found, &args.positional_text(0).unwrap_or_default());
    let then = args.positionals.get(1).cloned();
    let otherwise = args.positionals.get(2).cloned();
    if args.flag('F') {
        let chosen = if !shellcmd.is_empty() && !shellcmd.starts_with('0') {
            then
        } else {
            otherwise
        };
        let Some(value) = chosen else {
            return Ok(());
        };
        let source = item.source.clone();
        let calls = make_commands(server, item.client, item.origin, source.as_deref(), &value)
            .map_err(EdgeError::Message)?;
        if let Some(step) = item.step {
            server.cmdq_insert_commands(step, vec![(item.source.clone(), calls)]);
        }
        return Ok(());
    }
    let done = Done::new(server, item, wait);
    let spec = JobSpec {
        command: shellcmd.clone(),
        session: found.session,
        cwd: server.client_cwd(item.client, found.session),
        show_stderr: false,
        nowait: false,
    };
    let job_done = done.clone();
    let complete = Box::new(move |server: &mut Server, _: Vec<u8>, status: JobStatus| {
        // cmd_if_shell_callback (cmd-if-shell.c:132-169).
        let chosen = if status.success() { then } else { otherwise };
        if let Some(value) = chosen {
            job_done.queue(server, &value);
        }
        job_done.finish(server);
    });
    if server.job_run(&spec, complete).is_err() {
        return Err(EdgeError::Message(format!("failed to run command: {shellcmd}")).into());
    }
    if wait {
        item.blocked = true;
    }
    Ok(())
}
