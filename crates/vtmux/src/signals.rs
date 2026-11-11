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

/// Whether the watcher may still act: [`WATCHING`], until [`ATTACHED`]
/// at the server's `Ready` ([`ready`]: the client is attached from then
/// on, client.c `client_dispatch_wait`); or the SIGTERM or SIGHUP it acted
/// on (it shut the connection down). Its thread acts on a signal
/// some time after the handler ran, so once attached it does nothing, and
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
    watch_hook();
    let (Ok(mut signals), Ok(socket), Ok(here)) = (
        Signals::new([SIGTERM, SIGHUP, SIGINT, SIGUSR1, SIGUSR2]),
        stream.try_clone(),
        stream.try_clone(),
    ) else {
        return;
    };
    let _ = HANDLE.set(signals.handle());
    std::thread::spawn(move || {
        for sig in signals.forever() {
            act(sig, &socket);
        }
    });
    // One delivered before the watcher's handlers were in reached only the
    // flag: act on it here, or nothing would.
    if let Ok(sig) = i32::try_from(caught.load(Ordering::SeqCst)) {
        act(sig, &here);
    }
}

/// SIGTERM or SIGHUP while not attached: shut the connection down, so the
/// relay reading it stops and asks [`terminated`]. Once attached, the
/// attach has it (from `CAUGHT`, or its own handlers): shutting the
/// connection down would leave the attached client unable to say why it
/// goes, or to detach.
fn act(sig: i32, socket: &UnixStream) {
    if (sig == SIGTERM || sig == SIGHUP)
        && STATE
            .compare_exchange(WATCHING, sig, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    {
        let _ = socket.shutdown(std::net::Shutdown::Both);
    }
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
/// attached client's to report: one the watcher acted on before `Ready`
/// (the connection is shut down), or one since, still in [`CAUGHT`], which
/// the attach detaches for as for its own. One delivered after reaches the
/// client's own handlers (and [`CAUGHT`] too, so the attach may send a
/// second `Detach`, which the server ignores, as tmux's does a second
/// `MSG_EXITING`).
fn attached() -> Option<vtmux_client::attach::Caught> {
    at_attach_hook();
    let acted = STATE.swap(ATTACHED, Ordering::SeqCst);
    if let Some(handle) = HANDLE.get() {
        handle.close();
    }
    if acted > 0 {
        return Some(vtmux_client::attach::Caught {
            sig: acted,
            cut: true,
        });
    }
    let caught = CAUGHT.get().map_or(0, |c| c.load(Ordering::SeqCst));
    i32::try_from(caught)
        .ok()
        .filter(|&sig| sig > 0)
        .map(|sig| vtmux_client::attach::Caught { sig, cut: false })
}

/// A test hook read from the client's environment
/// (`crates/vtmux/tests/attach.rs`), for the two orders that once lost a
/// SIGTERM or SIGHUP at the attach, or its `Detach`:
/// - `TERM` or `HUP`: raised once the attached client's handlers are
///   installed, before this watcher is closed, so both have it;
/// - `TERM-early` or `HUP-early`: raised on the server's `Ready`, before
///   the attached client's handlers go in, so only [`CAUGHT`] has it.
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

/// The server's `Ready` came: the client is attached (client.c
/// `client_dispatch_wait` sets `client_attached`), so a SIGTERM or SIGHUP
/// from here on is the attach's: it detaches, where the watcher would cut
/// the connection.
pub fn ready() {
    let _ = STATE.compare_exchange(WATCHING, ATTACHED, Ordering::SeqCst, Ordering::SeqCst);
    if let Some((sig, true)) = hook() {
        let _ = signal_hook::low_level::raise(sig);
    }
}

/// A test hook read from the client's environment
/// (`crates/vtmux/tests/control.rs`): `TERM` or `HUP`, raised once the flag
/// handlers are in, before the watcher's (a signal that once ended no
/// command or control client).
const WATCH_SIGNAL_HOOK: &str = "VTMUX_TEST_WATCH_SIGNAL";

fn watch_hook() {
    let sig = match std::env::var(WATCH_SIGNAL_HOOK).as_deref() {
        Ok("TERM") => SIGTERM,
        Ok("HUP") => SIGHUP,
        _ => return,
    };
    let _ = signal_hook::low_level::raise(sig);
}

fn at_attach_hook() {
    if let Some((sig, false)) = hook() {
        let _ = signal_hook::low_level::raise(sig);
    }
}
