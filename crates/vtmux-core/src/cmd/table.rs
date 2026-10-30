//! Command table: every command in tmux 3.8's `cmd_table[]` (cmd.c:123-143),
//! in table order, and the lookup that resolves a typed name to an entry.
//!
//! Each row was generated from the `cmd_entry` structs in `cmd-*.c`
//! (`.name`, `.alias`, `.args = { template, lower, upper }`, `.usage`,
//! `.target`, `.source`, `.flags & CMD_STARTSERVER`). The generated name,
//! alias and usage columns were diffed against the reference tmux 3.8's
//! `list-commands` output (cmd-list-commands.c prints
//! `name (alias) usage`): 92 rows, 0 differences.
//!
//! Only the commands vtmux implements (`implemented`) run; every other row
//! still parses (arity and flags are validated for the whole table, exactly
//! as tmux validates before it executes anything) and is rejected at
//! execution time with [`CmdError::NotImplemented`](super::CmdError).

use super::CmdError;
use super::target::TargetKind;
use crate::error::EdgeError;

/// Flags on a `-t`/`-s` target slot (tmux.h:1988-1994, the ones the table
/// uses).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FindFlags {
    /// A failed lookup is not an error (`CMD_FIND_CANFAIL`).
    pub can_fail: bool,
    /// The target is a window index that may not exist yet
    /// (`CMD_FIND_WINDOW_INDEX`).
    pub window_index: bool,
    /// With no target, use the marked pane (`CMD_FIND_DEFAULT_MARKED`).
    pub default_marked: bool,
}

impl FindFlags {
    pub const NONE: Self = Self {
        can_fail: false,
        window_index: false,
        default_marked: false,
    };
    pub const CANFAIL: Self = Self {
        can_fail: true,
        ..Self::NONE
    };
    pub const WINDOW_INDEX: Self = Self {
        window_index: true,
        ..Self::NONE
    };
    pub const DEFAULT_MARKED: Self = Self {
        default_marked: true,
        ..Self::NONE
    };
}

/// What a command's `-t` (or `-s`) names: the flag letter, the kind of
/// object the lookup is for, and its options. The kind is what decides how a
/// bare `-t a` is read (cmd-find.c:1098-1128).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FindSpec {
    pub flag: char,
    pub kind: TargetKind,
    pub flags: FindFlags,
}

/// One command: its names, argument template and arity, usage line.
#[derive(Debug, PartialEq, Eq)]
pub struct CmdEntry {
    pub name: &'static str,
    pub alias: Option<&'static str>,
    /// getopt-style template as in `.args` (`x` flag, `x:` flag with a
    /// value, `x::` flag with an optional value).
    pub template: &'static str,
    pub min_args: usize,
    /// `None` is unlimited (`-1` in tmux).
    pub max_args: Option<usize>,
    pub usage: &'static str,
    /// The `-t` slot, if the command resolves one as a pane/window/session.
    /// Client targets (`detach-client -t`) are not listed here.
    pub target: Option<FindSpec>,
    /// The `-s` slot.
    pub source: Option<FindSpec>,
    /// `CMD_STARTSERVER`: the command may start a server that is not running
    /// (client.c:265).
    pub starts_server: bool,
    /// Whether vtmux runs this command yet.
    pub implemented: bool,
}

#[allow(clippy::too_many_arguments)]
const fn cmd(
    name: &'static str,
    alias: Option<&'static str>,
    template: &'static str,
    min_args: usize,
    max_args: Option<usize>,
    usage: &'static str,
    target: Option<FindSpec>,
    source: Option<FindSpec>,
    starts_server: bool,
    implemented: bool,
) -> CmdEntry {
    CmdEntry {
        name,
        alias,
        template,
        min_args,
        max_args,
        usage,
        target,
        source,
        starts_server,
        implemented,
    }
}

const fn slot(flag: char, kind: TargetKind, flags: FindFlags) -> FindSpec {
    FindSpec { flag, kind, flags }
}

/// Every tmux 3.8 command, in `cmd_table` order (the order matters: it is the
/// order of `ambiguous command` candidates).
#[rustfmt::skip]
pub static TMUX_COMMANDS: [CmdEntry; 92] = [
    cmd("attach-session", Some("attach"), "c:dEf:rt:x", 0, Some(0), "[-dErx] [-c working-directory] [-f flags] [-t target-session]", None, None, true, true),
    cmd("bind-key", Some("bind"), "nrN:T:", 1, None, "[-nr] [-T key-table] [-N note] key [command [argument ...]]", None, None, false, true),
    cmd("break-pane", Some("breakp"), "abdPF:n:s:t:Wx:X:y:Y:", 0, Some(0), "[-abdPW] [-F format] [-n window-name] [-s src-pane] [-t dst-window] [-x width] [-y height] [-X x-position] [-Y y-position]", Some(slot('t', TargetKind::Window, FindFlags::WINDOW_INDEX)), Some(slot('s', TargetKind::Pane, FindFlags::NONE)), false, true),
    cmd("capture-pane", Some("capturep"), "ab:CeE:FHIJLMNpPqRS:Tt:", 0, Some(0), "[-aCeFHIJLMNpPqRT] [-b buffer-name] [-E end-line] [-S start-line] [-t target-pane]", Some(slot('t', TargetKind::Pane, FindFlags::NONE)), None, false, true),
    cmd("choose-buffer", None, "F:f:K:kNO:rt:yZ", 0, Some(1), "[-kNrZ] [-F format] [-f filter] [-K key-format] [-O sort-order] [-t target-pane] [template]", Some(slot('t', TargetKind::Pane, FindFlags::NONE)), None, false, false),
    cmd("choose-client", None, "F:f:hiK:kNO:rt:yZ", 0, Some(1), "[-hikNrZ] [-F format] [-f filter] [-K key-format] [-O sort-order] [-t target-pane] [template]", Some(slot('t', TargetKind::Pane, FindFlags::NONE)), None, false, false),
    cmd("choose-tree", None, "F:f:GhK:kNO:rst:wyZ", 0, Some(1), "[-GhkNrswZ] [-F format] [-f filter] [-K key-format] [-O sort-order] [-t target-pane] [template]", Some(slot('t', TargetKind::Pane, FindFlags::NONE)), None, false, false),
    cmd("clear-history", Some("clearhist"), "Ht:", 0, Some(0), "[-H] [-t target-pane]", Some(slot('t', TargetKind::Pane, FindFlags::NONE)), None, false, false),
    cmd("clear-prompt-history", Some("clearphist"), "T:", 0, Some(0), "[-T prompt-type]", None, None, false, false),
    cmd("clock-mode", None, "t:", 0, Some(0), "[-t target-pane]", Some(slot('t', TargetKind::Pane, FindFlags::NONE)), None, false, false),
    cmd("command-prompt", None, "1CbeFiklI:NPp:t:T:", 0, Some(1), "[-1CbeFiklNP] [-I inputs] [-p prompts] [-t target-client] [-T prompt-type] [template]", None, None, false, false),
    cmd("confirm-before", Some("confirm"), "bc:p:t:y", 1, Some(1), "[-by] [-c confirm-key] [-p prompt] [-t target-client] command", None, None, false, false),
    cmd("copy-mode", None, "dekHMqSs:t:u", 0, Some(0), "[-dekHMqSu] [-s src-pane] [-t target-pane]", Some(slot('t', TargetKind::Pane, FindFlags::NONE)), Some(slot('s', TargetKind::Pane, FindFlags::NONE)), false, false),
    cmd("customize-mode", None, "F:f:kNt:yZ", 0, Some(0), "[-kNZ] [-F format] [-f filter] [-t target-pane]", Some(slot('t', TargetKind::Pane, FindFlags::NONE)), None, false, false),
    cmd("delete-buffer", Some("deleteb"), "b:", 0, Some(0), "[-b buffer-name]", None, None, false, false),
    cmd("detach-client", Some("detach"), "aE:s:t:P", 0, Some(0), "[-aP] [-E shell-command] [-s target-session] [-t target-client]", None, Some(slot('s', TargetKind::Session, FindFlags::CANFAIL)), false, true),
    cmd("display-menu", Some("menu"), "b:c:C:H:s:S:MOt:T:x:y:", 1, None, "[-MO] [-b border-lines] [-c target-client] [-C starting-choice] [-H selected-style] [-s style] [-S border-style] [-t target-pane] [-T title] [-x position] [-y position] name [key] [command] ...", Some(slot('t', TargetKind::Pane, FindFlags::NONE)), None, false, false),
    cmd("display-message", Some("display"), "aCc:d:jlINpt:F:v", 0, Some(1), "[-aCIjlNpv] [-c target-client] [-d delay] [-F format] [-t target-pane] [message]", Some(slot('t', TargetKind::Pane, FindFlags::CANFAIL)), None, false, true),
    cmd("display-popup", Some("popup"), "Bb:Cc:d:e:Eh:kNs:S:t:T:w:x:y:", 0, None, "[-BCEkN] [-b border-lines] [-c target-client] [-d start-directory] [-e environment] [-h height] [-s style] [-S border-style] [-t target-pane] [-T title] [-w width] [-x position] [-y position] [shell-command [argument ...]]", Some(slot('t', TargetKind::Pane, FindFlags::NONE)), None, false, false),
    cmd("display-panes", Some("displayp"), "d:kNs:t:Z", 0, Some(1), "[-kNZ] [-d duration] [-s source-window] [-t target-pane] [template]", Some(slot('t', TargetKind::Pane, FindFlags::NONE)), Some(slot('s', TargetKind::Window, FindFlags::NONE)), false, false),
    cmd("find-window", Some("findw"), "CiNrt:TZ", 1, Some(1), "[-CiNrTZ] [-t target-pane] match-string", Some(slot('t', TargetKind::Pane, FindFlags::NONE)), None, false, false),
    cmd("has-session", Some("has"), "t:", 0, Some(0), "[-t target-session]", Some(slot('t', TargetKind::Session, FindFlags::NONE)), None, false, true),
    cmd("if-shell", Some("if"), "bFt:", 2, Some(3), "[-bF] [-t target-pane] shell-command command [command]", Some(slot('t', TargetKind::Pane, FindFlags::CANFAIL)), None, false, false),
    cmd("join-pane", Some("joinp"), "bdfhvp:l:s:t:", 0, Some(0), "[-bdfhv] [-l size] [-s src-pane] [-t dst-pane]", Some(slot('t', TargetKind::Pane, FindFlags::NONE)), Some(slot('s', TargetKind::Pane, FindFlags::DEFAULT_MARKED)), false, true),
    cmd("kill-pane", Some("killp"), "af:t:", 0, Some(0), "[-a] [-f filter] [-t target-pane]", Some(slot('t', TargetKind::Pane, FindFlags::NONE)), None, false, true),
    cmd("kill-server", None, "", 0, Some(0), "", None, None, false, true),
    cmd("kill-session", None, "aCgf:t:", 0, Some(0), "[-aCg] [-f filter] [-t target-session]", Some(slot('t', TargetKind::Session, FindFlags::NONE)), None, false, true),
    cmd("kill-window", Some("killw"), "af:t:", 0, Some(0), "[-a] [-f filter] [-t target-window]", Some(slot('t', TargetKind::Window, FindFlags::NONE)), None, false, true),
    cmd("last-pane", Some("lastp"), "det:Z", 0, Some(0), "[-deZ] [-t target-window]", Some(slot('t', TargetKind::Window, FindFlags::NONE)), None, false, true),
    cmd("last-window", Some("last"), "t:", 0, Some(0), "[-t target-session]", Some(slot('t', TargetKind::Session, FindFlags::NONE)), None, false, true),
    cmd("link-window", Some("linkw"), "abdks:t:", 0, Some(0), "[-abdk] [-s src-window] [-t dst-window]", None, Some(slot('s', TargetKind::Window, FindFlags::NONE)), false, false),
    cmd("list-buffers", Some("lsb"), "F:f:O:r", 0, Some(0), "[-F format] [-f filter] [-O order]", None, None, false, false),
    cmd("list-clients", Some("lsc"), "F:f:O:rt:", 0, Some(0), "[-F format] [-f filter] [-O order][-t target-session]", Some(slot('t', TargetKind::Session, FindFlags::NONE)), None, false, true),
    cmd("list-commands", Some("lscm"), "F:", 0, Some(1), "[-F format] [command]", None, None, true, false),
    cmd("list-keys", Some("lsk"), "1aF:NO:P:rT:", 0, Some(1), "[-1aNr] [-F format] [-O order] [-P prefix-string][-T key-table] [key]", None, None, true, true),
    cmd("list-panes", Some("lsp"), "aF:f:O:rst:", 0, Some(0), "[-asr] [-F format] [-f filter] [-O order][-t target-window]", Some(slot('t', TargetKind::Window, FindFlags::NONE)), None, false, true),
    cmd("list-sessions", Some("ls"), "F:f:O:r", 0, Some(0), "[-r] [-F format] [-f filter] [-O order]", None, None, false, true),
    cmd("list-windows", Some("lsw"), "aF:f:O:rt:", 0, Some(0), "[-ar] [-F format] [-f filter] [-O order][-t target-session]", Some(slot('t', TargetKind::Session, FindFlags::NONE)), None, false, true),
    cmd("load-buffer", Some("loadb"), "b:t:w", 1, Some(1), "[-b buffer-name] [-t target-client] path", None, None, false, false),
    cmd("lock-client", Some("lockc"), "t:", 0, Some(0), "[-t target-client]", None, None, false, false),
    cmd("lock-server", Some("lock"), "", 0, Some(0), "", None, None, false, false),
    cmd("lock-session", Some("locks"), "t:", 0, Some(0), "[-t target-session]", Some(slot('t', TargetKind::Session, FindFlags::NONE)), None, false, false),
    cmd("move-pane", Some("movep"), "bdD::fhMvl:L::P:R::s:t:U::X:Y:z:", 0, Some(0), "[-bdfhMv] [-D lines] [-l size] [-L columns] [-P position] [-R columns] [-s src-pane] [-t dst-pane] [-U lines] [-X x-position] [-Y y-position] [-z z-index]", Some(slot('t', TargetKind::Pane, FindFlags::NONE)), Some(slot('s', TargetKind::Pane, FindFlags::DEFAULT_MARKED)), false, true),
    cmd("move-window", Some("movew"), "abdkrs:t:", 0, Some(0), "[-abdkr] [-s src-window] [-t dst-window]", None, Some(slot('s', TargetKind::Window, FindFlags::NONE)), false, false),
    cmd("new-pane", Some("newp"), "AbB:Cc:Dde:EfF:hIkl:KLMm:Op:PR:s:S:t:T:vWx:X:y:Y:Z", 0, None, "[-AbCDefhIkKLMOPvWZ] [-B border-lines] [-c start-directory] [-e environment] [-F format] [-l size] [-m message] [-p percentage] [-s style] [-S active-border-style] [-R inactive-border-style] [-T title] [-x width] [-y height] [-X x-position] [-Y y-position] [-t target-pane] [shell-command [argument ...]]", Some(slot('t', TargetKind::Pane, FindFlags::NONE)), None, false, false),
    cmd("new-session", Some("new"), "Ac:dDe:EF:f:n:Ps:t:x:Xy:", 0, None, "[-AdDEPX] [-c start-directory] [-e environment] [-F format] [-f flags] [-n window-name] [-s session-name] [-t target-session] [-x width] [-y height] [shell-command [argument ...]]", Some(slot('t', TargetKind::Session, FindFlags::CANFAIL)), None, true, true),
    cmd("new-window", Some("neww"), "abc:de:EF:kn:PSt:", 0, None, "[-abdEkPS] [-c start-directory] [-e environment] [-F format] [-n window-name] [-t target-window] [shell-command [argument ...]]", Some(slot('t', TargetKind::Window, FindFlags::WINDOW_INDEX)), None, false, true),
    cmd("next-layout", Some("nextl"), "t:", 0, Some(0), "[-t target-window]", Some(slot('t', TargetKind::Window, FindFlags::NONE)), None, false, true),
    cmd("next-window", Some("next"), "at:", 0, Some(0), "[-a] [-t target-session]", Some(slot('t', TargetKind::Session, FindFlags::NONE)), None, false, true),
    cmd("paste-buffer", Some("pasteb"), "db:prSs:t:", 0, Some(0), "[-dprS] [-s separator] [-b buffer-name] [-t target-pane]", Some(slot('t', TargetKind::Pane, FindFlags::NONE)), None, false, false),
    cmd("pipe-pane", Some("pipep"), "IOot:", 0, Some(1), "[-IOo] [-t target-pane] [shell-command]", Some(slot('t', TargetKind::Pane, FindFlags::NONE)), None, false, false),
    cmd("previous-layout", Some("prevl"), "t:", 0, Some(0), "[-t target-window]", Some(slot('t', TargetKind::Window, FindFlags::NONE)), None, false, true),
    cmd("previous-window", Some("prev"), "at:", 0, Some(0), "[-a] [-t target-session]", Some(slot('t', TargetKind::Session, FindFlags::NONE)), None, false, true),
    cmd("refresh-client", Some("refresh"), "A:B:cC:Df:r:F:lLRSt:U", 0, Some(1), "[-cDlLRSU] [-A pane:state] [-B name:what:format] [-C XxY] [-f flags] [-r pane:report] [-t target-client] [adjustment]", None, None, false, false),
    cmd("rename-session", Some("rename"), "t:", 1, Some(1), "[-t target-session] new-name", Some(slot('t', TargetKind::Session, FindFlags::NONE)), None, false, false),
    cmd("rename-window", Some("renamew"), "t:", 1, Some(1), "[-t target-window] new-name", Some(slot('t', TargetKind::Window, FindFlags::NONE)), None, false, true),
    cmd("resize-pane", Some("resizep"), "D::L::MR::Tt:U::x:y:Z", 0, Some(1), "[-MTZ] [-D lines] [-L columns] [-R columns] [-U lines] [-x width] [-y height] [-t target-pane]", Some(slot('t', TargetKind::Pane, FindFlags::NONE)), None, false, true),
    cmd("resize-window", Some("resizew"), "aADLRt:Ux:y:", 0, Some(1), "[-aADLRU] [-x width] [-y height] [-t target-window] [adjustment]", Some(slot('t', TargetKind::Window, FindFlags::NONE)), None, false, false),
    cmd("respawn-pane", Some("respawnp"), "c:e:Ekt:", 0, None, "[-Ek] [-c start-directory] [-e environment] [-t target-pane] [shell-command [argument ...]]", Some(slot('t', TargetKind::Pane, FindFlags::NONE)), None, false, true),
    cmd("respawn-window", Some("respawnw"), "c:e:Ekt:", 0, None, "[-Ek] [-c start-directory] [-e environment] [-t target-window] [shell-command [argument ...]]", Some(slot('t', TargetKind::Window, FindFlags::NONE)), None, false, false),
    cmd("rotate-window", Some("rotatew"), "Dt:UZ", 0, Some(0), "[-DUZ] [-t target-window]", Some(slot('t', TargetKind::Window, FindFlags::NONE)), None, false, false),
    cmd("run-shell", Some("run"), "bd:Ct:Es:c:", 0, None, "[-bCE] [-c start-directory] [-d delay] [-t target-pane] [shell-command [argument ...]]", Some(slot('t', TargetKind::Pane, FindFlags::CANFAIL)), None, false, false),
    cmd("save-buffer", Some("saveb"), "ab:", 1, Some(1), "[-a] [-b buffer-name] path", None, None, false, false),
    cmd("select-layout", Some("selectl"), "Enopt:", 0, Some(1), "[-Enop] [-t target-pane] [layout-name]", Some(slot('t', TargetKind::Pane, FindFlags::NONE)), None, false, true),
    cmd("select-pane", Some("selectp"), "DdegLlMmP:RT:t:UZ", 0, Some(0), "[-DdeLlMmRUZ] [-T title] [-t target-pane]", Some(slot('t', TargetKind::Pane, FindFlags::NONE)), None, false, true),
    cmd("select-window", Some("selectw"), "lnpTt:", 0, Some(0), "[-lnpT] [-t target-window]", Some(slot('t', TargetKind::Window, FindFlags::NONE)), None, false, true),
    cmd("send-keys", Some("send"), "c:FHKlMN:Rt:X", 0, None, "[-FHKlMRX] [-c target-client] [-N repeat-count] [-t target-pane] [key ...]", Some(slot('t', TargetKind::Pane, FindFlags::NONE)), None, false, true),
    cmd("send-prefix", None, "2t:", 0, Some(0), "[-2] [-t target-pane]", Some(slot('t', TargetKind::Pane, FindFlags::NONE)), None, false, true),
    cmd("server-access", None, "adglrw", 0, Some(1), "[-adglrw] [-t target-pane] [user|group]", None, None, false, false),
    cmd("set-buffer", Some("setb"), "ab:t:n:w", 0, Some(1), "[-aw] [-b buffer-name] [-n new-buffer-name] [-t target-client] [data]", None, None, false, false),
    cmd("set-environment", Some("setenv"), "Fhgrt:u", 1, Some(2), "[-Fhgru] [-t target-session] variable [value]", Some(slot('t', TargetKind::Session, FindFlags::CANFAIL)), None, false, true),
    cmd("set-hook", None, "agpERTt:uB:w", 0, Some(2), "[-agpERTuw] [-B name:what:format] [-t target-pane] [hook] [command]", Some(slot('t', TargetKind::Pane, FindFlags::CANFAIL)), None, false, false),
    cmd("set-option", Some("set"), "aFgopqst:uUw", 1, Some(2), "[-aFgopqsuUw] [-t target-pane] option [value]", Some(slot('t', TargetKind::Pane, FindFlags::CANFAIL)), None, false, true),
    cmd("set-window-option", Some("setw"), "aFgoqt:u", 1, Some(2), "[-aFgoqu] [-t target-window] option [value]", Some(slot('t', TargetKind::Window, FindFlags::CANFAIL)), None, false, true),
    cmd("show-buffer", Some("showb"), "b:", 0, Some(0), "[-b buffer-name]", None, None, false, false),
    cmd("show-environment", Some("showenv"), "hgst:", 0, Some(1), "[-hgs] [-t target-session] [variable]", Some(slot('t', TargetKind::Session, FindFlags::CANFAIL)), None, false, true),
    cmd("show-hooks", None, "BF:gpt:w", 0, Some(1), "[-Bgpw] [-F format] [-t target-pane] [hook]", Some(slot('t', TargetKind::Pane, FindFlags::CANFAIL)), None, false, false),
    cmd("show-messages", Some("showmsgs"), "JTt:", 0, Some(0), "[-JT] [-t target-client]", None, None, false, false),
    cmd("show-options", Some("show"), "AgF:Hpqst:vw", 0, Some(1), "[-AgHpqsvw] [-F format] [-t target-pane] [option]", Some(slot('t', TargetKind::Pane, FindFlags::CANFAIL)), None, false, true),
    cmd("show-prompt-history", Some("showphist"), "T:", 0, Some(0), "[-T prompt-type]", None, None, false, false),
    cmd("show-window-options", Some("showw"), "F:gvt:", 0, Some(1), "[-gv] [-F format] [-t target-window] [option]", Some(slot('t', TargetKind::Window, FindFlags::CANFAIL)), None, false, true),
    cmd("source-file", Some("source"), "t:Fnqv", 1, None, "[-Fnqv] [-t target-pane] path ...", Some(slot('t', TargetKind::Pane, FindFlags::CANFAIL)), None, false, true),
    cmd("split-window", Some("splitw"), "bB:c:de:EfF:hIkl:m:p:PR:s:S:t:T:vWZ", 0, None, "[-bdefhIklPvWZ] [-B border-lines] [-c start-directory] [-e environment] [-F format] [-l size] [-m message] [-p percentage] [-s style] [-S active-border-style] [-R inactive-border-style] [-T title] [-t target-pane] [shell-command [argument ...]]", Some(slot('t', TargetKind::Pane, FindFlags::NONE)), None, false, true),
    cmd("start-server", Some("start"), "", 0, Some(0), "", None, None, true, true),
    cmd("suspend-client", Some("suspendc"), "t:", 0, Some(0), "[-t target-client]", None, None, false, false),
    cmd("swap-pane", Some("swapp"), "dDs:t:UZ", 0, Some(0), "[-dDUZ] [-s src-pane] [-t dst-pane]", Some(slot('t', TargetKind::Pane, FindFlags::NONE)), Some(slot('s', TargetKind::Pane, FindFlags::DEFAULT_MARKED)), false, true),
    cmd("swap-window", Some("swapw"), "ds:t:", 0, Some(0), "[-d] [-s src-window] [-t dst-window]", Some(slot('t', TargetKind::Window, FindFlags::NONE)), Some(slot('s', TargetKind::Window, FindFlags::DEFAULT_MARKED)), false, false),
    cmd("switch-client", Some("switchc"), "c:EFlnO:pt:rT:Z", 0, Some(0), "[-ElnprZ] [-c target-client] [-t target-session] [-T key-table] [-O order]", None, None, false, true),
    cmd("switch-mode", None, "F:kst:wZ", 0, Some(1), "[-kswZ] [-F format] [-t target-pane] [command]", Some(slot('t', TargetKind::Pane, FindFlags::NONE)), None, false, false),
    cmd("unbind-key", Some("unbind"), "anqT:", 0, Some(1), "[-anq] [-T key-table] key", None, None, false, true),
    cmd("unlink-window", Some("unlinkw"), "kt:", 0, Some(0), "[-k] [-t target-window]", Some(slot('t', TargetKind::Window, FindFlags::NONE)), None, false, false),
    cmd("wait-for", Some("wait"), "EF:LSUlvw:", 1, Some(1), "[-ELSUlv] [-F format] [-w waiter] name", None, None, false, true),
];

/// Look up a typed command name. A port of `cmd_find` (cmd.c:469-506): walk
/// the table in order; an exact alias wins at once; otherwise a name that
/// starts with the typed text is a candidate, and an exact name match stops
/// the walk. Two candidates are ambiguous and the message lists every name
/// with that prefix in table order.
pub fn find(name: &str) -> Result<&'static CmdEntry, EdgeError> {
    let mut found: Option<&'static CmdEntry> = None;
    let mut ambiguous = false;
    for entry in &TMUX_COMMANDS {
        if entry.alias == Some(name) {
            ambiguous = false;
            found = Some(entry);
            break;
        }
        if !entry.name.starts_with(name) {
            continue;
        }
        if found.is_some() {
            ambiguous = true;
        }
        found = Some(entry);
        if entry.name == name {
            break;
        }
    }
    if ambiguous {
        let candidates = TMUX_COMMANDS
            .iter()
            .filter(|e| e.name.starts_with(name))
            .map(|e| e.name.to_owned())
            .collect();
        return Err(EdgeError::AmbiguousCommand {
            name: name.to_owned(),
            candidates,
        });
    }
    found.ok_or_else(|| EdgeError::UnknownCommand(name.to_owned()))
}

/// The default `command-alias` server option (options-table.c:322-331). An
/// alias is replaced by its text before lookup (cmd-parse.y
/// `cmd_parse_expand_alias`), and only an exact name matches.
/// User-configured aliases are an options feature (M1).
pub const DEFAULT_COMMAND_ALIASES: [(&str, &str); 6] = [
    ("split-pane", "split-window"),
    ("splitp", "split-window"),
    ("server-info", "show-messages -JT"),
    ("info", "show-messages -JT"),
    ("choose-window", "choose-tree -w"),
    ("choose-session", "choose-tree -s"),
];

impl CmdEntry {
    /// The error for running a command vtmux has not implemented.
    pub fn ensure_implemented(&self) -> Result<(), CmdError> {
        if self.implemented {
            Ok(())
        } else {
            Err(CmdError::NotImplemented(self.name.to_owned()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(name: &str) -> &'static str {
        find(name).expect("found").name
    }

    fn err(name: &str) -> String {
        find(name).expect_err("not found").to_string()
    }

    /// The M0 set by name and alias. Source: the `.name`/`.alias` of the
    /// `cmd_*_entry` structs in cmd-attach-session.c, cmd-capture-pane.c,
    /// cmd-detach-client.c, cmd-display-message.c, cmd-new-session.c
    /// (has-session), cmd-kill-server.c (start-server), cmd-kill-session.c,
    /// cmd-list-sessions.c, cmd-send-keys.c. Reference: `tmux
    /// list-commands` prints `name (alias) usage` for each, and `tmux <alias>`
    /// runs the command.
    #[test]
    fn every_m0_command_resolves_by_name_and_alias() {
        let m0 = [
            ("attach-session", Some("attach")),
            ("capture-pane", Some("capturep")),
            ("detach-client", Some("detach")),
            ("display-message", Some("display")),
            ("has-session", Some("has")),
            ("kill-server", None),
            ("kill-session", None),
            ("list-sessions", Some("ls")),
            ("new-session", Some("new")),
            ("send-keys", Some("send")),
            ("start-server", Some("start")),
            ("source-file", Some("source")),
            ("set-option", Some("set")),
            ("set-window-option", Some("setw")),
            ("show-options", Some("show")),
            ("show-window-options", Some("showw")),
            ("split-window", Some("splitw")),
            ("kill-pane", Some("killp")),
            ("select-pane", Some("selectp")),
            ("last-pane", Some("lastp")),
            ("list-panes", Some("lsp")),
            ("resize-pane", Some("resizep")),
            ("new-window", Some("neww")),
            ("kill-window", Some("killw")),
            ("select-window", Some("selectw")),
            ("next-window", Some("next")),
            ("previous-window", Some("prev")),
            ("last-window", Some("last")),
            ("rename-window", Some("renamew")),
            ("list-windows", Some("lsw")),
            ("select-layout", Some("selectl")),
            ("next-layout", Some("nextl")),
            ("previous-layout", Some("prevl")),
            ("swap-pane", Some("swapp")),
            ("break-pane", Some("breakp")),
            ("join-pane", Some("joinp")),
            ("move-pane", Some("movep")),
            ("respawn-pane", Some("respawnp")),
        ];
        for (name, alias) in m0 {
            assert_eq!(found(name), name);
            let entry = find(name).unwrap();
            assert_eq!(entry.alias, alias, "{name}");
            assert!(entry.implemented, "{name}");
            if let Some(alias) = alias {
                assert_eq!(found(alias), name, "alias {alias}");
            }
        }
        assert_eq!(TMUX_COMMANDS.iter().filter(|e| e.implemented).count(), 47);
    }

    /// cmd.c:469-506: an exact alias beats prefix matching, so `lsw` is
    /// list-windows, and `new`, which prefixes new-pane, new-session and
    /// new-window, is new-session. Reference: `tmux lsw`, `tmux new -d` run
    /// list-windows and new-session.
    #[test]
    fn alias_wins_over_prefix() {
        assert_eq!(found("lsw"), "list-windows");
        assert_eq!(found("new"), "new-session");
        assert_eq!(found("last"), "last-window");
        assert_eq!(found("if"), "if-shell");
        assert_eq!(found("set"), "set-option");
        assert_eq!(found("display"), "display-message");
    }

    /// cmd.c:469-506: a unique prefix resolves, an exact name stops the walk
    /// even when it prefixes a later name. Reference: `tmux kill-ser` runs
    /// kill-server; `tmux list-s` runs list-sessions.
    #[test]
    fn unique_prefix_and_exact_name() {
        assert_eq!(found("kill-ser"), "kill-server");
        assert_eq!(found("list-s"), "list-sessions");
        assert_eq!(found("display-message"), "display-message");
        assert_eq!(found("show-options"), "show-options");
    }

    /// Goldens from the reference tmux 3.8 (`tmux displ`, `tmux d`): the
    /// candidates are every name with the prefix, in `cmd_table` order, not
    /// sorted (display-panes comes last, cmd.c:123-143).
    #[test]
    fn ambiguous_lists_candidates_in_table_order() {
        assert_eq!(
            err("displ"),
            "ambiguous command: displ, could be: display-menu, display-message, display-popup, display-panes"
        );
        assert_eq!(
            err("d"),
            "ambiguous command: d, could be: delete-buffer, detach-client, display-menu, display-message, display-popup, display-panes"
        );
        // The empty name prefixes every command: tmux prints all 92.
        let all = err("");
        assert!(all.starts_with("ambiguous command: , could be: attach-session, bind-key,"));
        assert!(all.ends_with(", unlink-window, wait-for"));
    }

    /// cmd.c:496-499. Reference: `tmux nosuchcmd` ->
    /// `unknown command: nosuchcmd`.
    #[test]
    fn unknown_command() {
        assert_eq!(err("nosuchcmd"), "unknown command: nosuchcmd");
        assert_eq!(err("display-messagex"), "unknown command: display-messagex");
    }

    /// The table is `cmd_table` (cmd.c:123-143): 92 entries, names unique, in
    /// the order `list-commands` prints them, and the arity bounds are sane.
    #[test]
    fn table_shape() {
        assert_eq!(TMUX_COMMANDS.len(), 92);
        assert_eq!(TMUX_COMMANDS[0].name, "attach-session");
        assert_eq!(TMUX_COMMANDS[91].name, "wait-for");
        for (i, a) in TMUX_COMMANDS.iter().enumerate() {
            for b in &TMUX_COMMANDS[i + 1..] {
                assert_ne!(a.name, b.name);
                assert!(
                    a.alias.is_none() || a.alias != b.alias,
                    "{}",
                    a.alias.unwrap()
                );
            }
            assert!(a.max_args.is_none_or(|m| m >= a.min_args), "{}", a.name);
        }
    }

    /// Target slots from `.target`/`.source`. Reference source:
    /// cmd-display-message.c (pane, CANFAIL), cmd-new-session.c has-session
    /// (session) and new-session (session, CANFAIL), cmd-swap-pane.c.
    #[test]
    fn target_slots() {
        let t = |n: &str| find(n).unwrap().target.unwrap();
        assert_eq!(t("display-message").kind, TargetKind::Pane);
        assert!(t("display-message").flags.can_fail);
        assert_eq!(t("has-session").kind, TargetKind::Session);
        assert!(!t("has-session").flags.can_fail);
        assert_eq!(t("new-session").kind, TargetKind::Session);
        assert!(t("new-session").flags.can_fail);
        assert_eq!(t("capture-pane").kind, TargetKind::Pane);
        assert_eq!(t("send-keys").kind, TargetKind::Pane);
        assert_eq!(t("kill-session").kind, TargetKind::Session);
        // detach-client's -t is a client, not a find target.
        assert!(find("detach-client").unwrap().target.is_none());
        let s = find("swap-pane").unwrap().source.unwrap();
        assert!(s.flags.default_marked);
    }

    /// client.c:265: only new-session, attach-session, start-server,
    /// list-commands and list-keys start a server.
    #[test]
    fn starts_server_set() {
        let mut names: Vec<_> = TMUX_COMMANDS
            .iter()
            .filter(|e| e.starts_server)
            .map(|e| e.name)
            .collect();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "attach-session",
                "list-commands",
                "list-keys",
                "new-session",
                "start-server"
            ]
        );
    }

    #[test]
    fn non_m0_commands_are_not_implemented() {
        let e = find("swap-window").unwrap();
        assert_eq!(
            e.ensure_implemented().unwrap_err().to_string(),
            "not implemented in vtmux yet: swap-window"
        );
        assert!(find("new-session").unwrap().ensure_implemented().is_ok());
    }
}
