//! `set-option` and `set-window-option`: targets come from `cmdq_find_flag`;
//! option semantics live in the core (cmd-set-option.c `cmd_set_option_exec`).

use std::path::Path;
use std::rc::Rc;

use vtmux_core::cmd::{CommandCall, ConfigEnv};
use vtmux_core::format::{self, FormatContext, LoopItem, OptionScope, OptionsLoop};
use vtmux_core::options::{OptionError, OptionHost, OptionsStore, SetOutcome, Target, apply_set};

use super::Item;
use crate::format_ctx::{ClientView, Ctx, ServerInfo};
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
}

impl OptionHost for Host<'_> {
    fn expand(&mut self, store: &OptionsStore, template: &str, extra: &[(&str, String)]) -> String {
        let mut ctx = Ctx::new(self.model, self.info, self.env.0, self.found);
        ctx.options = store;
        ctx.clients = Rc::clone(&self.clients);
        ctx.client.clone_from(&self.client);
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
            env: GlobalEnv(&mut server.global_env),
            found,
            clients,
            client,
        },
    );
    server.model.options = options;
    result
}
