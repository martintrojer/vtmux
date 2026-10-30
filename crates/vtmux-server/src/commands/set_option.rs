//! `set-option` and `set-window-option`: targets come from `cmdq_find_flag`;
//! option semantics live in the core (cmd-set-option.c `cmd_set_option_exec`).

use std::path::Path;

use vtmux_core::cmd::{CommandCall, ConfigEnv};
use vtmux_core::format::{self, FormatContext, Tm};
use vtmux_core::options::{OptionError, OptionHost, OptionsStore, Target, apply_set};

use super::Item;
use crate::format_ctx::{Ctx, ServerInfo};
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

    fn time(&self, name: &str) -> Option<i64> {
        self.ctx.time(name)
    }

    fn local_time(&self, epoch: i64) -> Option<Tm> {
        self.ctx.local_time(epoch)
    }
}

impl OptionHost for Host<'_> {
    fn expand(&mut self, store: &OptionsStore, template: &str, extra: &[(&str, String)]) -> String {
        let mut ctx = Ctx::new(self.model, self.info, self.env.0, self.found);
        ctx.options = store;
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
    _item: &mut Item,
    call: &CommandCall,
    found: Found,
) -> Result<(), OptionError> {
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
        },
    );
    server.model.options = options;
    result.map(|_| ())
}
