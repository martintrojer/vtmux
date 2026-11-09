//! One turn's window drawing and frame, shared between the clients that
//! show the same thing (docs/PERFORMANCE.md, *Many clients*).
//!
//! tmux writes each pane update to every client's terminal as it comes;
//! vtmux composes a whole frame per client (D14), so n clients on one
//! window would pay for its drawing n times. Within one turn (one
//! `render_clients`), clients that draw the same window from the same
//! inputs ([`ShareKey`]) take the first one's drawing ([`SharedDraw`]) and
//! the composed rows it left ([`Composition`]), and a client whose
//! terminal shows what another's did gets that client's frame bytes
//! ([`SharedFrame`]). What each client is sent stays exactly what it would
//! have been sent alone: a drawing is shared only when the client's own
//! would be equal, and a frame only when both the terminal's screen and
//! the new screen are equal, cell for cell. (A drawing that shows the
//! time shows it as of the first client's drawing, a moment earlier than
//! the others would have drawn their own.)

use std::cell::RefCell;
use std::rc::Rc;

use vtmux_core::ids::SessionId;
use vtmux_vt::Snapshot;

use super::blend::TermColours;
use super::compose::{ComposeCache, Draw};
use super::scene::{Scene, SceneCache};
use super::window::BorderShare;
use super::{Caps, Compositor};

/// The scene and composed rows of a window drawing, kept between turns.
/// Clients that take the same drawing share one; only the client that
/// draws it composes into it, the others copy its rows.
#[derive(Debug, Default)]
pub struct Composition {
    pub scene: SceneCache,
    pub compose: RefCell<ComposeCache>,
}

/// What a window drawing ([`super::window::Source::window`] and the blend)
/// depends on besides the model and the client's border styles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShareKey {
    pub session: SessionId,
    pub size: (u16, u16),
    /// The window cell at the drawing's top left (the client's view).
    pub origin: (u32, u32),
    pub caps: Caps,
    /// The terminal's colours, for the pane blend (D26).
    pub colours: TermColours,
    /// The colours a pane's dim (`window-style` `dim`) mixes towards
    /// ([`crate::term_queries::Reported::dim_defaults`]).
    pub dim: (vtmux_vt::Color, vtmux_vt::Color),
}

/// A client's window drawing, for others with the same [`ShareKey`].
#[derive(Debug)]
pub struct SharedDraw {
    pub composition: Rc<Composition>,
    pub scene: Rc<Scene>,
    pub draw: Draw,
    /// The border styles it was drawn with.
    pub borders: BorderShare,
    /// When it was drawn, in seconds: a drawing can show the time (clock
    /// mode, a pane status line's format), so debug builds check a client
    /// that takes it against its own only within the same second.
    #[cfg(debug_assertions)]
    pub at: i64,
}

/// A frame one client was sent this turn, before its own additions (mouse
/// modes, title, passthrough).
#[derive(Debug)]
pub struct SharedFrame {
    caps: Caps,
    size: (u16, u16),
    /// Whose compositor wrote it.
    pub leader: usize,
    /// The cells it had to write whole ([`Compositor::force`]).
    pub forced: Vec<(u16, u16, u16)>,
    pub frame: Vec<u8>,
}

/// Whether clients share drawings and frames: always, but a test compares
/// them with clients that draw alone.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Sharing {
    #[default]
    On,
    Off,
}

/// What this turn's clients have drawn so far.
#[derive(Debug, Default)]
pub struct Turn {
    draws: Vec<(ShareKey, Rc<SharedDraw>)>,
    /// The compositions drawn into this turn: no other client draws into
    /// one, so its rows are the drawing that went with it.
    claimed: Vec<Rc<Composition>>,
    frames: Vec<SharedFrame>,
}

impl Turn {
    /// The drawing another client made this turn with `key`.
    #[must_use]
    pub fn draw(&self, key: &ShareKey) -> Option<Rc<SharedDraw>> {
        self.draws
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, d)| Rc::clone(d))
    }

    /// Offer a client's drawing to the clients after it with `key`.
    pub fn publish(&mut self, key: ShareKey, draw: Rc<SharedDraw>) {
        if self.draw(&key).is_none() {
            self.draws.push((key, draw));
        }
    }

    /// A client draws into `composition`: `false` when another one already
    /// did this turn, and the client must draw into one of its own.
    pub fn claim(&mut self, composition: &Rc<Composition>) -> bool {
        if self.claimed.iter().any(|c| Rc::ptr_eq(c, composition)) {
            return false;
        }
        self.claimed.push(Rc::clone(composition));
        true
    }

    /// The model changed under the drawings (a pane's synchronized output
    /// ended): none of them is what a client would draw now.
    pub fn forget_draws(&mut self) {
        self.draws.clear();
    }

    /// The frames of this turn a client with `caps` and `size` may take.
    pub fn frames(&self, caps: Caps, size: (u16, u16)) -> impl Iterator<Item = &SharedFrame> {
        self.frames
            .iter()
            .filter(move |f| f.caps == caps && f.size == size)
    }

    /// Offer the frame client `leader`'s compositor just wrote, with the
    /// cells it had forced.
    pub fn publish_frame(
        &mut self,
        leader: usize,
        caps: Caps,
        size: (u16, u16),
        forced: Vec<(u16, u16, u16)>,
        frame: &[u8],
    ) {
        if self.frames(caps, size).next().is_none() {
            self.frames.push(SharedFrame {
                caps,
                size,
                leader,
                forced,
                frame: frame.to_vec(),
            });
        }
    }
}

/// Whether two screens hold the same cells, by their row tags where both
/// carry a composed row ([`Compositor::buffer`]), else cell by cell. Two
/// rows of different composed rows count as different without a look,
/// even when their cells happen to be equal: a client that is told no
/// draws its own frame, which is never wrong.
fn same_rows(a: &Snapshot, a_tags: &[u64], b: &Snapshot, b_tags: &[u64]) -> bool {
    if (a.cols, a.rows, a.cursor, a.cursor_visible) != (b.cols, b.rows, b.cursor, b.cursor_visible)
        || a.wrapped != b.wrapped
        || a.cells.len() != b.cells.len()
    {
        return false;
    }
    let tag = |tags: &[u64], y: usize| tags.get(y).copied().unwrap_or(0);
    let rows = usize::from(a.rows);
    let tagged = |y| tag(a_tags, y) != 0 && tag(b_tags, y) != 0;
    if (0..rows).any(|y| tagged(y) && tag(a_tags, y) != tag(b_tags, y)) {
        return false;
    }
    let width = usize::from(a.cols).max(1);
    a.cells
        .chunks(width)
        .zip(b.cells.chunks(width))
        .enumerate()
        .all(|(y, (ra, rb))| tagged(y) || ra == rb)
}

/// Another client's compositor after its frame of this turn, the cells it
/// had forced and the frame ([`Compositor::shows_as`]).
pub type Shown<'a> = (&'a Compositor, &'a [(u16, u16, u16)], &'a [u8]);

impl Compositor {
    /// The cells the next frame writes whole ([`Self::force`]).
    #[must_use]
    pub fn forced(&self) -> &[(u16, u16, u16)] {
        &self.forced
    }

    /// The screen `leader`'s frame of this turn left, with its tags, if
    /// this terminal shows what `leader`'s showed before that frame and the
    /// frame would force the same cells: this compositor's frame of that
    /// screen is then `leader`'s, as a frame is a function of the screen
    /// the terminal shows, the new one, the forced cells and the caps.
    fn after<'a>(&self, (leader, forced, frame): Shown<'a>) -> Option<(&'a Snapshot, &'a [u64])> {
        if self.forced != forced || self.blocked {
            return None;
        }
        // frame_tagged keeps an unchanged screen as the spare, a changed
        // one as `last` and the screen before it as the spare.
        let ((before, before_tags), (now, now_tags)) = if frame.is_empty() {
            (
                (&leader.last, &leader.last_tags),
                (&leader.spare, &leader.spare_tags),
            )
        } else {
            (
                (&leader.spare, &leader.spare_tags),
                (&leader.last, &leader.last_tags),
            )
        };
        let shows = match (&self.last, before) {
            (None, None) => true,
            (Some(a), Some(b)) => same_rows(a, &self.last_tags, b, before_tags),
            _ => false,
        };
        shows.then_some((now.as_deref()?, now_tags.as_slice()))
    }

    /// Whether [`Self::frame_tagged`] of `screen` (with `tags`) would write
    /// what `shown`'s compositor just did ([`Self::after`]): `screen` is
    /// what that one's frame left.
    #[must_use]
    pub fn writes_as(&self, shown: Shown<'_>, screen: &Snapshot, tags: &[u64]) -> bool {
        self.after(shown)
            .is_some_and(|(now, now_tags)| same_rows(screen, tags, now, now_tags))
    }

    /// [`Self::writes_as`] of a plain screen not drawn yet, known by its
    /// row tags (each a composed or status row, none 0) and its cursor:
    /// the screen the frame left has the same tags and cursor (and no
    /// wrapped rows, which a plain screen has none of).
    #[must_use]
    pub fn shows_as(
        &self,
        shown: Shown<'_>,
        tags: &[u64],
        cursor: (Option<(u16, u16)>, bool),
    ) -> bool {
        !tags.contains(&0)
            && self.after(shown).is_some_and(|(now, now_tags)| {
                now_tags == tags
                    && usize::from(now.rows) == tags.len()
                    && (now.cursor, now.cursor_visible) == cursor
                    && now.wrapped.is_empty()
            })
    }

    /// The screen the last frame left, to share with a client that takes
    /// the frame ([`Self::take_shown`]).
    #[must_use]
    pub fn clone_shown(&self) -> Option<Rc<Snapshot>> {
        self.last.clone()
    }

    /// Take the `frame` another client's compositor wrote, which left
    /// `shown` with row `tags`, when [`Self::shows_as`] said this one would
    /// write it: the same state afterwards as [`Self::frame_tagged`], but
    /// the terminal's screen shared. The screen before stays as the next
    /// frame's buffer unless another client shows it too.
    pub fn take_shown(
        &mut self,
        shown: Option<Rc<Snapshot>>,
        tags: Vec<u64>,
        frame: &[u8],
    ) -> Vec<u8> {
        // An empty frame changes nothing: the terminal shows it already.
        if frame.is_empty() {
            return Vec::new();
        }
        self.forced.clear();
        let before = std::mem::replace(&mut self.last, shown);
        let before_tags = std::mem::replace(&mut self.last_tags, tags);
        if let Some(before) = before.filter(|b| Rc::strong_count(b) == 1) {
            self.spare = Some(before);
            self.spare_tags = before_tags;
        }
        self.sent = Some(std::time::Instant::now());
        frame.to_vec()
    }

    /// [`Self::frame_tagged`] of `screen` when [`Self::writes_as`] said it
    /// writes `frame`: the same state afterwards, without encoding it.
    pub fn take_frame(&mut self, screen: Snapshot, tags: Vec<u64>, frame: &[u8]) -> Vec<u8> {
        if frame.is_empty() {
            self.spare = Some(Rc::new(screen));
            self.spare_tags = tags;
            return Vec::new();
        }
        self.forced.clear();
        self.spare = self.last.replace(Rc::new(screen));
        self.spare_tags = std::mem::replace(&mut self.last_tags, tags);
        self.sent = Some(std::time::Instant::now());
        frame.to_vec()
    }
}
