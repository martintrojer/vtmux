//! A client's signals until it is attached, all along for a command or
//! control client (client.c `client_signal` while `!client_attached`):
//! SIGTERM and SIGHUP end it (`proc_exit`: status 0, and a control client
//! prints `%exit`), SIGINT, SIGUSR1 and SIGUSR2 are caught and do nothing,
//! and SIGPIPE, SIGTSTP, SIGTTIN, SIGTTOU and SIGQUIT are ignored from the
//! start (proc.c `proc_set_signals`). An attached client's are
//! vtmux-client's.

use std::os::unix::net::UnixStream;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI32, Ordering};

use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM, SIGUSR1, SIGUSR2};
use signal_hook::iterator::{Handle, Signals};

/// Who has the client's SIGTERM and SIGHUP: [`WATCHING`] (the watcher),
/// [`ATTACHED`] (the attached client's handlers), or the one of them that
/// came while watching, which ended the client. The watcher acts and the
/// attach takes over each with one atomic step, so exactly one of them
/// owns a signal that comes as the client attaches.
static STATE: AtomicI32 = AtomicI32::new(WATCHING);
const WATCHING: i32 = 0;
const ATTACHED: i32 = -1;
static HANDLE: OnceLock<Handle> = OnceLock::new();

/// Ignore the job-control signals, as soon as the command line is read.
pub fn ignore_job_control() {
    vtmux_server::sys::signal::ignore_job_control();
}

/// Catch the others once connected: a handler installed before the server
/// forks would be the server's too, writing to a descriptor the daemon has
/// closed, or reused. SIGTERM and SIGHUP shut the connection down, so the
/// relay reading it stops as at the server's EOF and asks [`terminated`];
/// once [`attached`], they do nothing here.
pub fn watch(stream: &UnixStream) {
    let (Ok(mut signals), Ok(socket)) = (
        Signals::new([SIGTERM, SIGHUP, SIGINT, SIGUSR1, SIGUSR2]),
        stream.try_clone(),
    ) else {
        return;
    };
    let _ = HANDLE.set(signals.handle());
    std::thread::spawn(move || {
        for sig in signals.forever() {
            // Once attached, the attached client's own handlers have it
            // (they were installed first): shutting the connection down
            // would leave it unable to say why it goes.
            if (sig == SIGTERM || sig == SIGHUP)
                && STATE
                    .compare_exchange(WATCHING, sig, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
            {
                let _ = socket.shutdown(std::net::Shutdown::Both);
            }
        }
    });
}

/// Whether SIGTERM or SIGHUP has ended the client.
pub fn terminated() -> bool {
    STATE.load(Ordering::SeqCst) > 0
}

/// The attached client's signal dispositions (vtmux-client).
pub const SYS: vtmux_client::attach::Sys = vtmux_client::attach::Sys {
    stop_on_tstp: vtmux_server::sys::signal::default_stop_on_tstp,
    ignore_tstp: vtmux_server::sys::signal::ignore_tstp,
    defaults: vtmux_server::sys::signal::default_job_control,
    attached,
};

/// Attached: the client's own handlers, installed just before, have
/// replaced these. The SIGTERM or SIGHUP the watcher took before this
/// (its connection is shut down) is the attached client's to report.
fn attached() -> Option<i32> {
    at_attach_hook();
    let taken = STATE.swap(ATTACHED, Ordering::SeqCst);
    if let Some(handle) = HANDLE.get() {
        handle.close();
    }
    (taken > 0).then_some(taken)
}

/// A test hook read from the client's environment: `TERM` or `HUP`
/// raises that signal once the attached client's handlers are installed,
/// before this watcher is closed, and waits for the watcher to take it
/// first (`crates/vtmux/tests/attach.rs`): the order that once left the
/// client saying "server exited unexpectedly".
const ATTACH_SIGNAL_HOOK: &str = "VTMUX_TEST_ATTACH_SIGNAL";

fn at_attach_hook() {
    let sig = match std::env::var(ATTACH_SIGNAL_HOOK).as_deref() {
        Ok("TERM") => SIGTERM,
        Ok("HUP") => SIGHUP,
        _ => return,
    };
    let _ = signal_hook::low_level::raise(sig);
    let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !terminated() && std::time::Instant::now() < end {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}
