//! The client flags tmux lets commands change (`CLIENT_READONLY`,
//! `CLIENT_IGNORESIZE`, `CLIENT_NO_DETACH_ON_DESTROY` and the control
//! client's `CLIENT_CONTROL_*`) and `server_client_set_flags` /
//! `server_client_get_flags` (server-client.c:3099-3199) over them.

/// A client's changeable flags. `attached`, `focused`, `control-mode` and
/// `UTF-8` are derived elsewhere; `STATUSFORCE` and the control size flags
/// are not listed to users.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[expect(clippy::struct_excessive_bools, reason = "independent CLIENT_* flags")]
pub struct ClientFlags {
    /// `CLIENT_READONLY`: keys and most commands are refused.
    pub read_only: bool,
    /// `CLIENT_IGNORESIZE`: the client's size counts for nothing while an
    /// unflagged client is attached (resize.c:95-107).
    pub ignore_size: bool,
    pub no_detach_on_destroy: bool,
    /// `CLIENT_CONTROL_NOOUTPUT`.
    pub no_output: bool,
    /// `CLIENT_CONTROL_WAITEXIT`: stored and listed; the client does not
    /// wait yet.
    pub wait_exit: bool,
    /// `CLIENT_CONTROL_NEWLAYOUTS`: layouts in the v2 form.
    pub new_layouts: bool,
    /// `CLIENT_CONTROL_PAUSEAFTER` with `c->pause_age` in milliseconds:
    /// stored and listed; panes do not pause yet.
    pub pause_after: Option<u64>,
    /// `CLIENT_SIZECHANGED`: a control client gave its size with
    /// `refresh-client -C` (resize.c:109-111). Not listed.
    pub size_changed: bool,
    /// `CLIENT_WINDOWSIZECHANGED`: it gave a window's size. Not listed.
    pub window_size_changed: bool,
}

impl ClientFlags {
    /// The flags `server_client_get_flags` lists between `control-mode`
    /// and `suspended`, in its order: those before `read-only`.
    #[must_use]
    pub fn listed_before_read_only(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.ignore_size {
            out.push("ignore-size".to_owned());
        }
        if self.no_detach_on_destroy {
            out.push("no-detach-on-destroy".to_owned());
        }
        if self.no_output {
            out.push("no-output".to_owned());
        }
        if self.wait_exit {
            out.push("wait-exit".to_owned());
        }
        if self.new_layouts {
            out.push("new-layouts".to_owned());
        }
        if let Some(age) = self.pause_after {
            out.push(format!("pause-after={}", age / 1000));
        }
        out
    }

    /// `server_client_set_flags`: a comma list of flag names, each
    /// optionally `!`-negated. Control flags only count on a control
    /// client; an unknown name is skipped. A read-only client cannot clear
    /// its own flag this way (server-client.c:3149-3150).
    pub fn set(&mut self, control: bool, flags: &str) {
        for item in flags.split(',') {
            let (not, name) = match item.strip_prefix('!') {
                Some(rest) => (true, rest),
                None => (false, item),
            };
            let value = !not;
            match name {
                "read-only" => self.read_only |= value,
                "ignore-size" => self.ignore_size = value,
                "no-detach-on-destroy" => self.no_detach_on_destroy = value,
                _ if !control => {}
                "no-output" => self.no_output = value,
                "wait-exit" => self.wait_exit = value,
                "new-layouts" => self.new_layouts = value,
                "pause-after" => self.pause_after = value.then_some(0),
                _ => {
                    // sscanf "pause-after=%u": the age is set even when
                    // the flag is being cleared; the flag is not touched
                    // by a malformed number.
                    if let Some(n) = name
                        .strip_prefix("pause-after=")
                        .and_then(|n| n.parse::<u32>().ok())
                    {
                        self.pause_after = value.then_some(u64::from(n) * 1000);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference (tmux 3.8): `refresh-client -f read-only,ignore-size` on a
    /// control client lists `ignore-size,...,read-only`; `!read-only` does
    /// not clear it.
    #[test]
    fn read_only_cannot_be_cleared_by_flags() {
        let mut f = ClientFlags::default();
        f.set(false, "read-only,ignore-size,bogus");
        assert!(f.read_only && f.ignore_size);
        f.set(false, "!read-only,!ignore-size");
        assert!(f.read_only && !f.ignore_size);
    }

    #[test]
    fn control_flags_need_a_control_client() {
        let mut f = ClientFlags::default();
        f.set(false, "new-layouts,pause-after=3");
        assert_eq!(f, ClientFlags::default());
        f.set(true, "new-layouts,pause-after=3,wait-exit");
        assert_eq!(
            f.listed_before_read_only(),
            ["wait-exit", "new-layouts", "pause-after=3"]
        );
        f.set(true, "!new-layouts,!pause-after");
        assert_eq!(f.listed_before_read_only(), ["wait-exit"]);
    }
}
