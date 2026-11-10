//! A client's signal dispositions (proc.c `proc_set_signals`): SIGPIPE,
//! SIGTSTP, SIGTTIN, SIGTTOU and SIGQUIT are ignored from the start. An
//! attached client's handlers are vtmux-client's.

/// Ignore the job-control signals, as soon as the command line is read.
pub fn ignore_job_control() {
    vtmux_server::sys::signal::ignore_job_control();
}

/// The attached client's signal dispositions (vtmux-client).
pub const SYS: vtmux_client::attach::Sys = vtmux_client::attach::Sys {
    stop_on_tstp: vtmux_server::sys::signal::default_stop_on_tstp,
    ignore_tstp: vtmux_server::sys::signal::ignore_tstp,
    defaults: vtmux_server::sys::signal::default_job_control,
    attached,
};

/// Attached: the client's own handlers are in place.
fn attached() {}
