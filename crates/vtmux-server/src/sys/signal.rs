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

/// SIGTSTP back to its default action, stopping the process: an attached
/// client stops itself with it on `suspend-client` (client.c:788-795 sets
/// `SIG_DFL` before `kill(getpid(), SIGTSTP)`), whatever it inherited.
pub fn default_stop_on_tstp() {
    // SAFETY: SIG_DFL installs no handler code; signal(2) with a valid
    // signal number cannot violate memory safety.
    unsafe {
        libc::signal(libc::SIGTSTP, libc::SIG_DFL);
    }
}
