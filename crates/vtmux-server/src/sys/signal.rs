//! Signal dispositions (proc.c `proc_set_signals`, 244-276).

/// Ignore the signals tmux ignores in every process: SIGPIPE, SIGTSTP,
/// SIGTTIN, SIGTTOU and SIGQUIT. The others are caught through
/// signal-hook.
pub fn ignore_job_control() {
    for sig in [
        libc::SIGPIPE,
        libc::SIGTSTP,
        libc::SIGTTIN,
        libc::SIGTTOU,
        libc::SIGQUIT,
    ] {
        // SAFETY: SIG_IGN installs no handler code; signal(2) with a valid
        // signal number cannot violate memory safety.
        unsafe {
            libc::signal(sig, libc::SIG_IGN);
        }
    }
}

/// What [`ignore_job_control`] ignores back to the default action, for a
/// client about to become another program (client.c `client_exec`,
/// `proc_clear_signals`): an ignored signal stays ignored across exec.
pub fn default_job_control() {
    for sig in [
        libc::SIGPIPE,
        libc::SIGTSTP,
        libc::SIGTTIN,
        libc::SIGTTOU,
        libc::SIGQUIT,
    ] {
        // SAFETY: SIG_DFL installs no handler code; signal(2) with a valid
        // signal number cannot violate memory safety.
        unsafe {
            libc::signal(sig, libc::SIG_DFL);
        }
    }
}

/// SIGTSTP back to its default action, stopping the process: an attached
/// client stops itself with it on `suspend-client` (client.c:788-795 sets
/// `SIG_DFL` before `kill(getpid(), SIGTSTP)`), and only then.
pub fn default_stop_on_tstp() {
    // SAFETY: SIG_DFL installs no handler code; signal(2) with a valid
    // signal number cannot violate memory safety.
    unsafe {
        libc::signal(libc::SIGTSTP, libc::SIG_DFL);
    }
}

/// SIGTSTP ignored again: an attached client back from its stop
/// (client.c:551-556, on SIGCONT).
pub fn ignore_tstp() {
    // SAFETY: SIG_IGN installs no handler code; signal(2) with a valid
    // signal number cannot violate memory safety.
    unsafe {
        libc::signal(libc::SIGTSTP, libc::SIG_IGN);
    }
}

/// The signals the server catches (`server_signal`, server.c:438-470;
/// proc.c `proc_set_signals`): the server's `Signals` set and what a live
/// upgrade blocks across `execve`.
pub const HANDLED: [libc::c_int; 8] = [
    libc::SIGCHLD,
    libc::SIGTERM,
    libc::SIGINT,
    libc::SIGHUP,
    libc::SIGUSR1,
    libc::SIGUSR2,
    libc::SIGWINCH,
    libc::SIGCONT,
];

/// A signal mask saved by [`block_handled`], for [`restore_mask`].
#[derive(Clone, Copy)]
pub struct Mask(libc::sigset_t);

/// Block every signal of [`HANDLED`] in the calling thread; the mask
/// before. A blocked signal stays pending, across `execve` too, until
/// [`unblock_handled`] (or [`restore_mask`]) lets it in: a live upgrade
/// blocks them for the window between `execve` and the new image's
/// handlers, where most of them would kill the process by default.
#[must_use]
pub fn block_handled() -> Mask {
    // SAFETY: sigset_t is a plain C struct, all-zero is a valid value, and
    // sigemptyset/sigaddset/pthread_sigmask only read and write the sets.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        let mut old: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&raw mut set);
        for sig in HANDLED {
            libc::sigaddset(&raw mut set, sig);
        }
        libc::pthread_sigmask(libc::SIG_BLOCK, &raw const set, &raw mut old);
        Mask(old)
    }
}

/// Put back a mask [`block_handled`] returned.
pub fn restore_mask(mask: Mask) {
    // SAFETY: sets the calling thread's mask from a valid saved set.
    unsafe {
        libc::pthread_sigmask(libc::SIG_SETMASK, &raw const mask.0, std::ptr::null_mut());
    }
}

/// Unblock every signal of [`HANDLED`]: the new image of a live upgrade,
/// once it has handlers for them, takes what arrived meanwhile.
pub fn unblock_handled() {
    // SAFETY: as in `block_handled`.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&raw mut set);
        for sig in HANDLED {
            libc::sigaddset(&raw mut set, sig);
        }
        libc::pthread_sigmask(libc::SIG_UNBLOCK, &raw const set, std::ptr::null_mut());
    }
}

/// `command.exec()` unless `proceed`, asked with every signal of
/// [`HANDLED`] blocked just before the `execve`, says no: a live
/// upgrade's re-exec, which carries the SIGTERM or SIGHUP the client has
/// caught so far in the new image's environment, so one caught after
/// that was read would be lost with this image's handlers. Asked with
/// them blocked, `proceed` sees every signal handled before the exec: a
/// later one stays pending. With `keep_blocked` (the terminal is handed
/// over raw) it stays pending across the `execve` until the new image's
/// handlers are in (`vtmux_core::protocol::REATTACH_TTY_ENV`): without it
/// a SIGINT, SIGTERM, SIGHUP, SIGUSR1 or SIGUSR2 in between took the
/// default action and killed the new image before it could give the
/// terminal its settings back. Without (a binary that may never unblock
/// them), SIGTERM and SIGHUP go back to their default action before the
/// mask is let in again, so one pending ends the client, as it ends a
/// tmux client starting up, rather than being lost. Done in a `pre_exec`
/// hook: std's `exec` empties the mask first. Returns only if the exec
/// fails or `proceed` said no (`Interrupted`), the mask let in again and
/// SIGPIPE's disposition as it was.
pub fn exec_unless(
    command: &mut std::process::Command,
    keep_blocked: bool,
    proceed: impl Fn() -> bool + Send + Sync + 'static,
) -> std::io::Error {
    use std::os::unix::process::CommandExt as _;
    // SAFETY: `exec` does not fork: the hook runs in this process just
    // before `execve`, and only sets the calling thread's signal mask and
    // two signals' dispositions to their defaults.
    unsafe {
        command.pre_exec(move || {
            let before = block_handled();
            if !proceed() {
                return Err(std::io::ErrorKind::Interrupted.into());
            }
            if !keep_blocked {
                libc::signal(libc::SIGTERM, libc::SIG_DFL);
                libc::signal(libc::SIGHUP, libc::SIG_DFL);
                restore_mask(before);
            }
            Ok(())
        });
    }
    // std's `exec` puts SIGPIPE to its default action for the new image:
    // one that fails leaves it so, and a write to a closed pipe (a
    // dropped `Signals`' handler writing to its own) would kill the
    // client. What it was goes back.
    // SAFETY: reads SIGPIPE's disposition, setting it to what it was.
    let pipe = unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };
    // SAFETY: as above.
    unsafe { libc::signal(libc::SIGPIPE, pipe) };
    let err = command.exec();
    // SAFETY: puts back the disposition read above.
    unsafe { libc::signal(libc::SIGPIPE, pipe) };
    unblock_handled();
    err
}

/// Whether `sig` is blocked in the calling thread.
#[must_use]
pub fn blocked(sig: libc::c_int) -> bool {
    // SAFETY: reads the calling thread's mask into a local set.
    unsafe {
        let mut cur: libc::sigset_t = std::mem::zeroed();
        libc::pthread_sigmask(libc::SIG_BLOCK, std::ptr::null(), &raw mut cur);
        libc::sigismember(&raw const cur, sig) == 1
    }
}

/// Ignore every signal of [`HANDLED`] but SIGTERM and SIGCHLD, and let
/// them in: for a process that keeps a server's descriptors with no
/// server to handle signals (the live upgrade's very last resort), so a
/// stray SIGHUP does not end it and its panes; SIGTERM still does.
pub fn ignore_handled_but_term() {
    for sig in HANDLED {
        if sig != libc::SIGTERM && sig != libc::SIGCHLD {
            // SAFETY: SIG_IGN installs no handler code.
            unsafe {
                libc::signal(sig, libc::SIG_IGN);
            }
        }
    }
    unblock_handled();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocking_the_handled_signals_is_undone_by_the_saved_mask() {
        assert!(!blocked(libc::SIGHUP));
        let old = block_handled();
        assert!(HANDLED.iter().all(|&s| blocked(s)));
        restore_mask(old);
        assert!(!blocked(libc::SIGHUP));
        let _ = block_handled();
        unblock_handled();
        assert!(!HANDLED.iter().any(|&s| blocked(s)));
    }
}
