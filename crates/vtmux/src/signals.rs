//! A client's signals until it is attached, all along for a command or
//! control client (client.c `client_signal` while `!client_attached`):
//! SIGTERM and SIGHUP end it (`proc_exit`: status 0, and a control client
//! prints `%exit`), SIGINT, SIGUSR1 and SIGUSR2 are caught and do nothing,
//! and SIGPIPE, SIGTSTP, SIGTTIN, SIGTTOU and SIGQUIT are ignored from the
//! start (proc.c `proc_set_signals`). An attached client's are
//! vtmux-client's.

use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM, SIGUSR1, SIGUSR2};
use signal_hook::iterator::{Handle, Signals};

/// Whether the watcher's thread may still act: [`WATCHING`], until
/// [`ATTACHED`]; or the SIGTERM or SIGHUP it acted on (it shut the
/// connection down). Its thread acts on a signal some time after the
/// handler ran, so after the attach has taken over it does nothing, and
/// the attach finds the signal in [`CAUGHT`] instead.
static STATE: AtomicI32 = AtomicI32::new(WATCHING);
const WATCHING: i32 = 0;
const ATTACHED: i32 = -1;
static HANDLE: OnceLock<Handle> = OnceLock::new();
/// The last SIGTERM or SIGHUP, stored by the signal handler itself
/// (`signal_hook::flag`), so it is there as soon as the signal is
/// delivered, whether or not the watcher's thread has run.
static CAUGHT: OnceLock<Arc<AtomicUsize>> = OnceLock::new();

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
    // First, so every signal the watcher gets is in `CAUGHT` too.
    let caught = CAUGHT.get_or_init(Arc::default);
    for sig in [SIGTERM, SIGHUP] {
        if let Ok(value) = usize::try_from(sig) {
            let _ = signal_hook::flag::register_usize(sig, Arc::clone(caught), value);
        }
    }
    let (Ok(mut signals), Ok(socket)) = (
        Signals::new([SIGTERM, SIGHUP, SIGINT, SIGUSR1, SIGUSR2]),
        stream.try_clone(),
    ) else {
        return;
    };
    let _ = HANDLE.set(signals.handle());
    let held = hook().is_some_and(|(_, early)| early);
    std::thread::spawn(move || {
        for sig in signals.forever() {
            if held {
                wait_until(|| STATE.load(Ordering::SeqCst) == ATTACHED);
            }
            // Once attached, the attach has it (from `CAUGHT`, or its own
            // handlers): shutting the connection down would leave the
            // attached client unable to say why it goes.
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

/// Attached: the client's own handlers, installed just before, take
/// over from these. A SIGTERM or SIGHUP delivered before now is the
/// attached client's to report, whether the watcher's thread acted on it
/// (the connection is shut down) or not yet (it no longer will). One
/// delivered after reaches the client's own handlers. Left over: a
/// handler still running on another thread at this very instant.
fn attached() -> Option<i32> {
    at_attach_hook();
    let acted = STATE.swap(ATTACHED, Ordering::SeqCst);
    if let Some(handle) = HANDLE.get() {
        handle.close();
    }
    if acted > 0 {
        return Some(acted);
    }
    let caught = CAUGHT.get().map_or(0, |c| c.load(Ordering::SeqCst));
    i32::try_from(caught).ok().filter(|&sig| sig > 0)
}

/// A test hook read from the client's environment
/// (`crates/vtmux/tests/attach.rs`), for the two orders that once lost a
/// SIGTERM or SIGHUP at the attach:
/// - `TERM` or `HUP`: raised once the attached client's handlers are
///   installed, before this watcher is closed, the watcher taking it
///   first (it shuts the connection down);
/// - `TERM-early` or `HUP-early`: raised on the server's `Ready`, before
///   the attached client's handlers go in, the watcher's thread held until
///   the attach has taken over (a thread that had not run yet).
const ATTACH_SIGNAL_HOOK: &str = "VTMUX_TEST_ATTACH_SIGNAL";

/// [`ATTACH_SIGNAL_HOOK`]: the signal, and whether it comes early.
fn hook() -> Option<(i32, bool)> {
    let value = std::env::var(ATTACH_SIGNAL_HOOK).ok()?;
    let (name, early) = match value.strip_suffix("-early") {
        Some(name) => (name.to_owned(), true),
        None => (value, false),
    };
    let sig = match name.as_str() {
        "TERM" => SIGTERM,
        "HUP" => SIGHUP,
        _ => return None,
    };
    Some((sig, early))
}

/// `done`, waited for at most 10 s.
fn wait_until(done: impl Fn() -> bool) {
    let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !done() && std::time::Instant::now() < end {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

/// The server's `Ready` came: the attach starts.
pub fn ready() {
    if let Some((sig, true)) = hook() {
        let _ = signal_hook::low_level::raise(sig);
    }
}

fn at_attach_hook() {
    if let Some((sig, false)) = hook() {
        let _ = signal_hook::low_level::raise(sig);
        wait_until(terminated);
    }
}
