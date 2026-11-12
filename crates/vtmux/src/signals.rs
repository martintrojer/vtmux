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
    let caught = catch();
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

/// [`CAUGHT`], its handlers installed the first time.
fn catch() -> &'static Arc<AtomicUsize> {
    CAUGHT.get_or_init(|| {
        let caught = Arc::<AtomicUsize>::default();
        for sig in [SIGTERM, SIGHUP] {
            if let Ok(value) = usize::try_from(sig) {
                let _ = signal_hook::flag::register_usize(sig, Arc::clone(&caught), value);
            }
        }
        caught
    })
}

/// A re-attach after a live upgrade (`--vtmux-reattach`), first thing:
/// the client never stopped being attached (the server kept its record),
/// so a SIGTERM or SIGHUP from now on is the attach's, as one after
/// `Ready` is: kept in [`CAUGHT`] for it to detach for, never cutting the
/// connection, which would leave the server's record waiting and the
/// client ending as one not attached. `carried` is the one the image
/// before caught and could not end for (`VTMUX_REATTACH_SIGNAL`).
pub fn reattaching(carried: Option<&std::ffi::OsStr>) {
    STATE.store(ATTACHED, Ordering::SeqCst);
    let caught = catch();
    let carried = match carried.and_then(|c| c.to_str()) {
        Some("TERM") => SIGTERM,
        Some("HUP") => SIGHUP,
        _ => 0,
    };
    if let Ok(sig) = usize::try_from(carried) {
        // One caught since is the later.
        let _ = caught.compare_exchange(0, sig, Ordering::SeqCst, Ordering::SeqCst);
    }
    reexec_hook(true);
}

/// The SIGTERM or SIGHUP caught since the client connected, if any (the
/// last): before a live upgrade's re-exec, which would lose it.
fn caught() -> Option<i32> {
    reexec_hook(false);
    last_caught()
}

fn last_caught() -> Option<i32> {
    let caught = CAUGHT.get().map_or(0, |c| c.load(Ordering::SeqCst));
    i32::try_from(caught).ok().filter(|&sig| sig > 0)
}

/// Whether a re-attaching client has a SIGTERM or SIGHUP to end for:
/// its reason, as an attached client gives it (client.c
/// `client_exit_message`).
pub fn reason() -> Option<&'static str> {
    match last_caught()? {
        SIGTERM => Some("terminated"),
        _ => Some("lost tty"),
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
    caught,
    exec_blocked: vtmux_server::sys::signal::exec_handled_blocked,
};

/// Attached: the client's own handlers, installed just before, take
/// over from these. A SIGTERM or SIGHUP delivered before now is the
/// attached client's to report: one the watcher acted on before `Ready`
/// (the connection is shut down), or one since, still in [`CAUGHT`], which
/// the attach detaches for as for its own. One delivered after reaches the
/// client's own handlers (and [`CAUGHT`] too: the attach sends one
/// `Detach` for both).
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
///   the attached client's handlers go in, so only [`CAUGHT`] has it;
/// - `TERM-cut` or `HUP-cut`: raised as `Ready` is read, the watcher left
///   to act on it first, as when the signal lands just before `Ready`.
const ATTACH_SIGNAL_HOOK: &str = "VTMUX_TEST_ATTACH_SIGNAL";

/// When [`ATTACH_SIGNAL_HOOK`] raises its signal.
#[derive(Clone, Copy, PartialEq)]
enum At {
    Handlers,
    Early,
    Cut,
}

/// [`ATTACH_SIGNAL_HOOK`]: the signal, and when it comes.
fn hook() -> Option<(i32, At)> {
    let value = std::env::var(ATTACH_SIGNAL_HOOK).ok()?;
    let (name, at) = match value.split_once('-') {
        Some((name, "early")) => (name, At::Early),
        Some((name, "cut")) => (name, At::Cut),
        Some(_) => return None,
        None => (value.as_str(), At::Handlers),
    };
    Some((signal_named(name)?, at))
}

fn signal_named(name: &str) -> Option<i32> {
    match name {
        "TERM" => Some(SIGTERM),
        "HUP" => Some(SIGHUP),
        _ => None,
    }
}

/// The server's `Ready` came: the client is attached (client.c
/// `client_dispatch_wait` sets `client_attached`), so a SIGTERM or SIGHUP
/// from here on is the attach's: it detaches, where the watcher would cut
/// the connection.
pub fn ready() {
    if let Some((sig, At::Cut)) = hook() {
        let _ = signal_hook::low_level::raise(sig);
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !terminated() && std::time::Instant::now() < end {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
    let _ = STATE.compare_exchange(WATCHING, ATTACHED, Ordering::SeqCst, Ordering::SeqCst);
    if let Some((sig, At::Early)) = hook() {
        let _ = signal_hook::low_level::raise(sig);
    }
}

/// A test hook read from the client's environment
/// (`crates/vtmux/tests/upgrade.rs`): `TERM` or `HUP`, raised once a live
/// upgrade's re-exec is decided, the attach's handlers gone (a signal
/// that once left the client attached after the exec); `TERM-reattach`
/// or `HUP-reattach`, raised in the image it execs, before its `Hello`;
/// `<sig>-start` (`TERM`, `HUP`, `INT`, `USR1`, `USR2`), raised in that
/// image first thing, before it has any handler (a signal that once
/// killed it there, the terminal it was handed raw left raw).
const REEXEC_SIGNAL_HOOK: &str = "VTMUX_TEST_REEXEC_SIGNAL";

fn reexec_hook(reattach: bool) {
    let Ok(value) = std::env::var(REEXEC_SIGNAL_HOOK) else {
        return;
    };
    let sig = match value.strip_suffix("-reattach") {
        Some(name) if reattach => signal_named(name),
        None if !reattach && !value.ends_with("-start") => signal_named(&value),
        _ => None,
    };
    if let Some(sig) = sig {
        let _ = signal_hook::low_level::raise(sig);
    }
}

/// [`REEXEC_SIGNAL_HOOK`]'s `<sig>-start`: in a re-attaching image, before
/// its first handler.
pub fn reattach_start_hook() {
    let Ok(value) = std::env::var(REEXEC_SIGNAL_HOOK) else {
        return;
    };
    let sig = match value.strip_suffix("-start") {
        Some("INT") => SIGINT,
        Some("USR1") => SIGUSR1,
        Some("USR2") => SIGUSR2,
        Some(name) => match signal_named(name) {
            Some(sig) => sig,
            None => return,
        },
        None => return,
    };
    let _ = signal_hook::low_level::raise(sig);
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
    if let Some((sig, At::Handlers)) = hook() {
        let _ = signal_hook::low_level::raise(sig);
    }
}
