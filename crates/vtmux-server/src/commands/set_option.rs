//! `set-option` and `set-window-option`: targets come from `cmdq_find_flag`;
//! option semantics live in the core (cmd-set-option.c `cmd_set_option_exec`).

use std::path::Path;
use std::rc::Rc;

use vtmux_core::cmd::{CommandCall, ConfigEnv};
use vtmux_core::format::{self, FormatContext, LoopItem, OptionScope, OptionsLoop};
use vtmux_core::options::{OptionError, OptionHost, OptionsStore, SetOutcome, Target, apply_set};

use super::Item;
use crate::format_ctx::{ClientView, Ctx, ServerInfo};
use crate::format_jobs::JobHandle;
use crate::model::Model;
use crate::server::{GlobalEnv, Server, check_shell};
use crate::target::Found;

pub(super) fn option_target(found: Found) -> Target {
    Target {
        session: found.session,
        window: found.window,
        pane: found.pane,
    }
}

/// Borrow the non-option model while the core mutates the option store.
/// Its expansion context reads the store supplied by `apply_set`, so -F
/// sees changes made earlier in the same command list.
pub(super) struct Host<'a> {
    pub model: &'a Model,
    pub info: &'a ServerInfo,
    pub env: GlobalEnv<'a>,
    pub found: Found,
    /// The attached clients and the target client (`format_single_from_target`).
    pub clients: Rc<[ClientView]>,
    pub client: Option<ClientView>,
    /// A hook's formats (`cmdq_merge_formats`).
    pub formats: Vec<(String, String)>,
    /// The running command's client's `#()` jobs (`format_single_from_target`).
    pub jobs: Option<JobHandle>,
}

/// The attached clients and the command's target client, for [`Host`].
pub(super) fn client_parts(
    server: &Server,
    item: &Item,
    found: Found,
) -> (Rc<[ClientView]>, Option<ClientView>) {
    let tc = server.target_client(item.client, item.origin);
    let ctx = server.format_ctx(found, tc);
    (ctx.clients, ctx.client)
}

struct Vars<'a, 'b> {
    ctx: Ctx<'a>,
    extra: &'b [(&'b str, String)],
}

impl FormatContext for Vars<'_, '_> {
    fn get(&self, name: &str) -> Option<String> {
        self.ctx
            .options
            .format_value(name, &option_target(self.ctx.target))
            .or_else(|| {
                self.extra
                    .iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| v.clone())
            })
            .or_else(|| self.ctx.get(name))
    }

    fn get_without_environ(&self, name: &str) -> Option<String> {
        self.ctx
            .options
            .format_value(name, &option_target(self.ctx.target))
            .or_else(|| {
                self.extra
                    .iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| v.clone())
            })
            .or_else(|| self.ctx.get_without_environ(name))
    }

    fn time(&self, name: &str) -> Option<i64> {
        self.ctx.time(name)
    }

    // The loops: children are fresh contexts (format_create), so the extras
    // stay behind.
    fn sessions(&self) -> Vec<LoopItem<'_>> {
        self.ctx.sessions()
    }

    fn windows(&self) -> Option<Vec<LoopItem<'_>>> {
        self.ctx.windows()
    }

    fn panes(&self) -> Option<Vec<LoopItem<'_>>> {
        self.ctx.panes()
    }

    fn clients(&self) -> Vec<LoopItem<'_>> {
        self.ctx.clients()
    }

    fn options(&self, scope: OptionScope) -> Option<OptionsLoop<'_>> {
        self.ctx.options(scope)
    }

    fn job(&self, raw: &str, expanded: &str) -> String {
        self.ctx.job(raw, expanded)
    }
}

impl OptionHost for Host<'_> {
    fn expand(&mut self, store: &OptionsStore, template: &str, extra: &[(&str, String)]) -> String {
        let mut ctx = Ctx::new(self.model, self.info, self.env.env, self.found);
        ctx.options = store;
        ctx.clients = Rc::clone(&self.clients);
        ctx.client.clone_from(&self.client);
        ctx.jobs.clone_from(&self.jobs);
        ctx.extra.extend(
            self.formats
                .iter()
                .map(|(k, v)| (k.clone().into(), v.clone())),
        );
        format::expand_str(template, &Vars { ctx, extra })
    }

    fn config_env(&mut self) -> &mut dyn ConfigEnv {
        &mut self.env
    }

    fn usable_shell(&self, path: &str) -> bool {
        check_shell(Path::new(path))
    }
}

pub fn exec(
    server: &mut Server,
    item: &mut Item,
    call: &CommandCall,
    found: Found,
) -> Result<SetOutcome, OptionError> {
    let (clients, client) = client_parts(server, item, found);
    let jobs = server.job_handle(item.client);
    // Split ownership temporarily rather than cloning the full store for
    // every set. Always restore it, including on validation errors.
    let mut options = std::mem::replace(
        &mut server.model.options,
        OptionsStore::new(&vtmux_core::options::Defaults::default()),
    );
    let result = apply_set(
        &mut options,
        call,
        &option_target(found),
        &mut Host {
            model: &server.model,
            info: &server.info,
            env: GlobalEnv::new(&mut server.global_env, &server.model, &server.info),
            found,
            clients,
            client,
            formats: server.cmd_formats.clone(),
            jobs: Some(jobs),
        },
    );
    server.model.options = options;
    result
}

/// Run `f` over the option store with a [`Host`] for target `found` and
/// no client (what a mode's prompt callback has).
pub(crate) fn with_option_host<R>(
    server: &mut Server,
    found: Found,
    f: impl FnOnce(&mut OptionsStore, &mut dyn OptionHost) -> R,
) -> R {
    let clients = server.client_views();
    let mut options = std::mem::replace(
        &mut server.model.options,
        OptionsStore::new(&vtmux_core::options::Defaults::default()),
    );
    let result = f(
        &mut options,
        &mut Host {
            model: &server.model,
            info: &server.info,
            env: GlobalEnv::new(&mut server.global_env, &server.model, &server.info),
            found,
            clients,
            client: None,
            formats: Vec::new(),
            jobs: None,
        },
    );
    server.model.options = options;
    result
}

impl Server {
    /// `utf8_update_width_cache` (utf8.c:412): the server's widths follow
    /// `codepoint-widths` (server.c:212 at start, then on every change).
    pub(crate) fn update_width_cache(&self) {
        let items = self.array_option(vtmux_core::options::TreeId::Server, "codepoint-widths");
        vtmux_core::width::set_codepoint_widths(&items);
        vtmux_vt::set_width_overrides(&vtmux_core::width::codepoint_width_ranges());
    }

    /// `options_push_changes` (options.c:1359-1475) for option `name`,
    /// the parts vtmux has.
    pub(crate) fn options_push_changes(&mut self, name: &str) {
        let windows: Vec<_> = self.model.windows.keys().copied().collect();
        if name == "automatic-rename" {
            // options.c:1380-1386: windows with it on look again.
            for &w in &windows {
                self.model.mark_for_rename(w);
            }
        }
        if name == "key-table" {
            // options.c:1399-1401: every client goes back to the (new)
            // default key table.
            let ids: Vec<_> = self.clients.keys().copied().collect();
            for id in ids {
                self.set_key_table(id, None);
            }
        }
        if name == "status" || name == "status-interval" {
            // options.c:1409-1411.
            self.status_timer_start_all();
        }
        if name == "codepoint-widths" {
            // options.c:1460-1461: utf8_update_width_cache.
            self.update_width_cache();
        }
        if matches!(name, "window-style" | "window-active-style") {
            // options.c:1425-1429: PANE_STYLECHANGED|PANE_THEMECHANGED.
            for pane in self.model.panes.values_mut() {
                pane.requests.theme_changed();
            }
        }
        if name == "monitor-silence" {
            // options.c:1422: alerts_reset_all.
            self.model.alerts_reset_all();
        }
        if name == "pane-scrollbars" {
            // options.c:1448-1451: every auto-hidden bar hides.
            let panes: Vec<_> = self.model.panes.keys().copied().collect();
            for p in panes {
                self.model.scrollbar_hide(p);
            }
        }
        if matches!(
            name,
            "pane-border-status"
                | "pane-scrollbars"
                | "pane-scrollbars-position"
                | "pane-scrollbars-style"
        ) {
            // options.c:1437-1446, 1452-1458: layout_fix_panes everywhere.
            for &w in &windows {
                self.model.fix_panes(w);
            }
        }
        if name == "command-alias" {
            // cmd_get_alias reads the option at every parse.
            vtmux_core::cmd::set_command_aliases(
                self.array_option(vtmux_core::options::TreeId::Server, "command-alias"),
            );
        }
        if name == "history-limit" {
            // options.c:1464-1466: session_update_history.
            self.model.update_history();
        }
        self.push_option_changes();
    }
}
