//! A client's signals until it is attached, all along for a command or
//! control client (client.c `client_signal` while `!client_attached`):
//! SIGTERM and SIGHUP end it (`proc_exit`: status 0, and a control client
//! prints `%exit`), SIGINT, SIGUSR1 and SIGUSR2 are caught and do nothing,
//! and SIGPIPE, SIGTSTP, SIGTTIN, SIGTTOU and SIGQUIT are ignored from the
//! start (proc.c `proc_set_signals`). An attached client's are
//! vtmux-client's.

use std::os::unix::net::UnixStream;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM, SIGUSR1, SIGUSR2};
use signal_hook::iterator::{Handle, Signals};

/// SIGTERM or SIGHUP came: the client leaves as the server's EOF.
static TERMINATED: AtomicBool = AtomicBool::new(false);
static HANDLE: OnceLock<Handle> = OnceLock::new();

/// Ignore the job-control signals, as soon as the command line is read.
pub fn ignore_job_control() {
    vtmux_server::sys::signal::ignore_job_control();
}

/// Catch the others once connected: a handler installed before the server
/// forks would be the server's too, writing to a descriptor the daemon has
/// closed, or reused. SIGTERM and SIGHUP shut the connection down, so the
/// relay reading it stops as at the server's EOF and asks [`terminated`].
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
            if sig == SIGTERM || sig == SIGHUP {
                TERMINATED.store(true, Ordering::SeqCst);
                let _ = socket.shutdown(std::net::Shutdown::Both);
            }
        }
    });
}

/// Whether SIGTERM or SIGHUP has ended the client.
pub fn terminated() -> bool {
    TERMINATED.load(Ordering::SeqCst)
}

/// The attached client's signal dispositions (vtmux-client).
pub const SYS: vtmux_client::attach::Sys = vtmux_client::attach::Sys {
    stop_on_tstp: vtmux_server::sys::signal::default_stop_on_tstp,
    ignore_tstp: vtmux_server::sys::signal::ignore_tstp,
    defaults: vtmux_server::sys::signal::default_job_control,
    attached,
};

/// Attached: the client's own handlers have replaced these.
fn attached() {
    if let Some(handle) = HANDLE.get() {
        handle.close();
    }
}
