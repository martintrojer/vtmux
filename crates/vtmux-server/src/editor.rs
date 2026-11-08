//! The editor a mode's `e` opens (spawn.c `spawn_editor`,
//! `spawn_editor_finish`): customize-mode's current value or choose-buffer's
//! current buffer goes to a temporary file, and `editor file` runs in a
//! modal floating pane nine tenths of the window, centred, above a zoomed
//! pane. The file is read back when the pane's program exits 0; a pane
//! killed first, or an editor that fails, changes nothing.

use std::path::PathBuf;

use vtmux_core::ids::PaneId;
use vtmux_core::layout::{AddFlags, Geometry};
use vtmux_core::options::{TreeId, Value};

use crate::jobs::JobStatus;
use crate::server::{ClientId, PaneSpawn, Server};

/// `struct spawn_editor_state`: the file and what it edits, kept on the
/// editor's pane (`wp->editor`).
#[derive(Debug)]
pub struct Editor {
    pub path: PathBuf,
    /// Whose edit it is: the pane whose mode started it.
    pub pane: PaneId,
    /// Which edit, for the mode.
    pub edit: Edit,
}

/// What an [`Editor`] edits, by the mode that started it.
#[derive(Debug)]
pub enum Edit {
    /// `window_customize_editdata`.
    Customize(crate::mode::window_customize::Edit),
    /// `window_buffer_editdata`.
    Buffer(crate::mode::window_buffer::Edit),
}

impl Drop for Editor {
    /// `spawn_editor_free`: the file goes with it.
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// `mkstemp("/tmp/tmux.XXXXXXXX")` with `value` written to it: a new
/// file, mode 0600, under a name nothing else has.
fn temp_file(value: &[u8]) -> Option<PathBuf> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    for n in 0..100u128 {
        let tag = (seed ^ (u128::from(std::process::id()) << 64)).wrapping_add(n * 7919);
        let path = PathBuf::from(format!("/tmp/tmux.{:08x}", tag % 0xffff_ffff));
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path);
        match file {
            Ok(mut f) => {
                if f.write_all(value).is_err() {
                    let _ = std::fs::remove_file(&path);
                    return None;
                }
                return Some(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) => return None,
        }
    }
    None
}

impl Server {
    /// `spawn_editor` (spawn.c:703-788) for the mode on `mode_pane`, from
    /// `client`: in the current window of its session, unless it has
    /// a modal pane already, `editor file` runs in a new floating pane
    /// (`SPAWN_FLOATING|SPAWN_MODAL|SPAWN_FLOATOVERZOOM`) of nine tenths
    /// of the window, centred, with `/tmp/` its directory and
    /// `remain-on-exit` off. The window is unzoomed while the pane is made
    /// and zoomed again after. `false` if it could not start.
    pub(crate) fn spawn_editor(
        &mut self,
        client: ClientId,
        mode_pane: PaneId,
        value: &[u8],
        edit: Edit,
    ) -> bool {
        let Some(s) = self.client_session(client) else {
            return false;
        };
        let Some((index, w, active)) = self.model.current_of(s) else {
            return false;
        };
        if self.model.windows[&w].panes.modal.is_some() {
            return false;
        }
        let Some(path) = temp_file(value) else {
            return false;
        };
        let editor = self
            .model
            .options
            .get(TreeId::Server, "editor")
            .map(|e| e.value.to_text(e.def, None, false))
            .unwrap_or_default();
        let window = &self.model.windows[&w];
        let (wsx, wsy) = (window.sx(), window.sy());
        let (sx, sy) = (wsx * 9 / 10, wsy * 9 / 10);
        let half = |n: u32| i32::try_from(n / 2).unwrap_or(i32::MAX);
        let cell = Geometry::new(sx, sy, half(wsx) - half(sx), half(wsy) - half(sy));
        let was_zoomed = self.model.push_zoom(w, false, true);
        // layout_floating_pane(w, NULL): after the whole layout.
        let new = self.model.pane_ids.allocate();
        let window = self.model.windows.get_mut(&w).expect("current window");
        window.layout.add_floating(None, new, cell);
        window.panes.add(
            new,
            Some(active),
            AddFlags {
                floating: true,
                ..AddFlags::default()
            },
        );
        let window_rect = Geometry::new(window.sx(), window.sy(), 0, 0);
        let rect = window.layout.pane_rect(new).unwrap_or(window_rect);
        let session_env = self.model.sessions[&s].env.clone();
        let spawned = self.spawn_pane(&PaneSpawn {
            pane: new,
            window: w,
            session: s,
            session_env: &session_env,
            extra_env: &[],
            client_path: None,
            argv: vec![format!("{editor} {}", path.display())],
            cwd: PathBuf::from("/tmp/"),
            rect,
            shell: None,
            empty: false,
        });
        let Ok(mut pane) = spawned else {
            let window = self.model.windows.get_mut(&w).expect("current window");
            window.layout.close_pane(new);
            window.panes.remove(new);
            self.model.fix_panes(w);
            self.model.pop_zoom(w, was_zoomed);
            let _ = std::fs::remove_file(&path);
            return false;
        };
        let created = (window_rect.sx, window_rect.sy);
        let assigned = created != (rect.sx, rect.sy);
        if assigned {
            pane.resizes.push((created, (rect.sx, rect.sy)));
        }
        pane.over_zoom = true;
        pane.editor = Some(Editor {
            path,
            pane: mode_pane,
            edit,
        });
        self.model.options.add_pane(new, w);
        self.model.panes.insert(new, pane);
        self.model
            .fix_panes_resized_from(w, None, assigned.then_some((new, created)));
        self.fire_pane_created(s, index, new, false, false);
        // spawn.c:596-603.
        let window = self.model.windows.get_mut(&w).expect("current window");
        window.panes.modal_last = window.panes.active;
        window.panes.modal = Some(new);
        self.model.switch_active(w, new);
        self.model.layout_changed(w);
        self.model.pop_zoom(w, was_zoomed);
        // spawn.c:785.
        self.model
            .options
            .set_table_value(TreeId::Pane(new), "remain-on-exit", Value::Choice(0));
        true
    }

    /// The pid of the editor running for pane `p`'s mode, if one is
    /// (`spawn_get_editor_pid`).
    pub(crate) fn editor_pid(&self, p: PaneId) -> Option<i32> {
        self.model
            .panes
            .values()
            .find(|x| x.editor.as_ref().is_some_and(|e| e.pane == p))
            .map(|x| x.pid)
    }

    /// `spawn_editor_finish` (spawn.c:648-700) for the editor of a pane
    /// whose program exited with `status` or which is going: the file's
    /// contents if the program exited 0, else nothing (a pane destroyed
    /// before its status came in has the status of a hangup), go to the
    /// mode that started it, if it is still waiting for them.
    pub(crate) fn editor_finish(&mut self, editor: Editor, status: Option<JobStatus>) {
        let ok = matches!(status, Some(JobStatus::Exited(0)));
        let value = ok.then(|| std::fs::read(&editor.path).ok()).flatten();
        match &editor.edit {
            Edit::Customize(edit) => self.customize_edit_done(editor.pane, edit, value),
            Edit::Buffer(edit) => self.buffer_edit_done(editor.pane, edit, value),
        }
        // spawn_editor_free: the file goes with it.
        drop(editor);
    }
}
