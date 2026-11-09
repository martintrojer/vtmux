//! Hyperlinks as the outer terminal is given them: tmux's external ids
//! (hyperlinks.c `hyperlinks_put`) and URIs, for `tty_hyperlink` (tty.c).
//!
//! tmux keeps a tree of links per screen, a link being its URI and OSC 8
//! id (one without an id is its own), and gives each new one an external
//! id from a counter of the whole server, `tmux` and the number in hex
//! (`hyperlinks_next_external_id`): here a [`LinkNumbers`], which the
//! server shares with each of its panes. The outer terminal is told
//! `OSC 8 ; id=tmuxN ; URI`, so cells of one link are one link there too,
//! a link without an id included, across lines and wraps.
//!
//! vtmux's grid is Ghostty's, which keeps the URI and id of each cell's
//! link. A pane's [`Links`] maps each link to its [`Link`] the first time a
//! cell of it is read for drawing: tmux numbers a link when its OSC 8 is
//! parsed, so a link that scrolls away before anything draws it takes a
//! number in tmux and none here (D-068). A `link=` style's link is one per
//! URI (style.c `style_hyperlinks`, which uses the URI as the id):
//! [`LinkNumbers::format_link`].

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use libghostty_vt::screen::HyperlinkId;

/// One hyperlink as the outer terminal is given it.
#[derive(Debug, PartialEq, Eq, Hash)]
pub struct Link {
    /// The external id's number (`hyperlinks_next_external_id`).
    number: u64,
    /// The URI as `hyperlinks_put` keeps it: `utf8_stravis` with
    /// `VIS_OCTAL|VIS_CSTYLE` ([`crate::vis_link`]).
    uri: Box<str>,
}

impl Link {
    /// The external id the terminal is given (`tmux%llX`).
    #[must_use]
    pub fn id(&self) -> String {
        format!("tmux{:X}", self.number)
    }

    /// The external id's number.
    #[must_use]
    pub fn number(&self) -> u64 {
        self.number
    }

    /// The URI, escaped as tmux keeps it.
    #[must_use]
    pub fn uri(&self) -> &str {
        &self.uri
    }

    /// A link with a given number (a snapshot's), `uri` as [`Link::uri`]
    /// gave it. No counter moves: the [`LinkNumbers`] that gave the number
    /// is restored on its own ([`LinkNumbers::reach`]).
    #[must_use]
    pub fn restored(number: u64, uri: &str) -> Arc<Self> {
        Arc::new(Self {
            number,
            uri: uri.into(),
        })
    }
}

/// tmux's `hyperlinks_next_external_id`, one counter for the server, and
/// `style_hyperlinks`, the server's `link=` style links by URI
/// ([`LinkNumbers::format_link`]). A handle: clones share one counter. The
/// server holds one and gives each pane it makes or restores a clone
/// ([`crate::PaneTerm::number_links_from`]); a pane on its own numbers
/// from one of its own. Not a global, so two servers in one process (the
/// tests) number apart, and a snapshot reads it without moving it.
#[derive(Clone, Debug, Default)]
pub struct LinkNumbers(Rc<Numbers>);

#[derive(Debug)]
struct Numbers {
    next: Cell<u64>,
    format: RefCell<HashMap<String, Arc<Link>>>,
}

impl Default for Numbers {
    fn default() -> Self {
        Self {
            next: Cell::new(1),
            format: RefCell::default(),
        }
    }
}

impl LinkNumbers {
    /// The number the next link takes, for a snapshot.
    #[must_use]
    pub fn next(&self) -> u64 {
        self.0.next.get()
    }

    /// Move the counter on to at least `n` (a snapshot's, or one past a
    /// number a restored link holds): links made after a live upgrade
    /// never take a number one still on screen has.
    pub fn reach(&self, n: u64) {
        self.0.next.set(self.0.next.get().max(n));
    }

    fn take(&self) -> u64 {
        let n = self.0.next.get();
        self.0.next.set(n.saturating_add(1));
        n
    }

    /// Whether `self` and `other` are one counter.
    #[must_use]
    pub fn same(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }

    /// A link of its own with the next external id; `uri` as Ghostty
    /// holds it.
    #[must_use]
    pub fn fresh(&self, uri: &str) -> Arc<Link> {
        Arc::new(Link {
            number: self.take(),
            uri: crate::vis_link(uri).into(),
        })
    }

    /// The link of a `link=URI` style (format-draw.c `format_draw`): the
    /// URI is its id, so every cell with it is one link. tmux puts the URI
    /// in two trees, each giving it an external id: `style_parse`'s
    /// `style_hyperlinks`, which only the parse sees, then the drawn
    /// screen's, whose id the terminal is told; so the first draw of a URI
    /// takes two numbers. tmux's screens (each client's status line, each
    /// pane's border line, ...) each number it once more; here it is one
    /// link everywhere.
    #[must_use]
    pub fn format_link(&self, uri: &str) -> Arc<Link> {
        let mut links = self.0.format.borrow_mut();
        if let Some(link) = links.get(uri) {
            return Arc::clone(link);
        }
        if links.len() >= PRUNE_AT {
            links.retain(|_, l| Arc::strong_count(l) > 1);
        }
        self.take();
        let link = self.fresh(uri);
        links.insert(uri.to_owned(), Arc::clone(&link));
        link
    }
}

/// How tmux tells a link's cells apart from another's: its id, or for one
/// without, the OSC 8 that opened it (Ghostty's implicit id, counted per
/// screen).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum LinkKeyId {
    Explicit(Vec<u8>),
    Implicit { alternate: bool, n: u32 },
}

/// A pane's links: Ghostty's (URI, id) to the [`Link`] drawn for it.
#[derive(Debug, Default)]
pub struct Links {
    map: HashMap<(String, LinkKeyId), Arc<Link>>,
    /// The counter new links take their numbers from.
    pub(crate) numbers: LinkNumbers,
}

/// Entries past which [`Links`] drops the links no cell holds any more
/// (tmux keeps at most `MAX_HYPERLINKS`, 5000, in the whole server).
const PRUNE_AT: usize = 5000;

impl Links {
    /// The link of a cell with Ghostty's `uri` and `id`, read from the
    /// alternate screen or not.
    pub(crate) fn get(
        &mut self,
        uri: &str,
        id: &HyperlinkId,
        alternate: bool,
    ) -> Option<Arc<Link>> {
        let id = match id {
            HyperlinkId::None => return None,
            HyperlinkId::Explicit(b) => LinkKeyId::Explicit(b.clone()),
            HyperlinkId::Implicit(n) => LinkKeyId::Implicit { alternate, n: *n },
        };
        let key = (uri.to_owned(), id);
        if let Some(link) = self.map.get(&key) {
            return Some(Arc::clone(link));
        }
        if self.map.len() >= PRUNE_AT {
            self.map.retain(|_, l| Arc::strong_count(l) > 1);
        }
        let link = self.numbers.fresh(uri);
        self.map.insert(key, Arc::clone(&link));
        Some(link)
    }

    /// The one link drawn for `uri` (as Ghostty holds it), if there is
    /// just one.
    #[must_use]
    pub fn by_uri(&self, uri: &str) -> Option<Arc<Link>> {
        let mut found = self.map.iter().filter(|((u, _), _)| u == uri);
        let (_, link) = found.next()?;
        found.next().is_none().then(|| Arc::clone(link))
    }

    /// Every link, for a snapshot: URI, id and the external id's number.
    #[must_use]
    pub fn export(&self) -> Vec<(String, LinkKeyId, u64)> {
        let mut out: Vec<_> = self
            .map
            .iter()
            .map(|((uri, id), l)| (uri.clone(), id.clone(), l.number))
            .collect();
        out.sort_by_key(|e| e.2);
        out
    }

    /// The links [`Links::export`] gave; the counter moves past them.
    pub fn import(&mut self, links: &[(String, LinkKeyId, u64)]) {
        self.map = links
            .iter()
            .map(|(uri, id, n)| {
                self.numbers.reach(n.saturating_add(1));
                let link = Link::restored(*n, &crate::vis_link(uri));
                ((uri.clone(), id.clone()), link)
            })
            .collect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A link and its id are one link; one without an id is its own per
    /// OSC 8, and per screen; an id is in tmux's form.
    #[test]
    fn links_are_told_apart_as_tmux_does() {
        let mut l = Links::default();
        let a = l
            .get("http://a", &HyperlinkId::Explicit(b"x".to_vec()), false)
            .unwrap();
        let a2 = l
            .get("http://a", &HyperlinkId::Explicit(b"x".to_vec()), true)
            .unwrap();
        let b = l
            .get("http://b", &HyperlinkId::Explicit(b"x".to_vec()), false)
            .unwrap();
        let i0 = l.get("http://a", &HyperlinkId::Implicit(0), false).unwrap();
        let i1 = l.get("http://a", &HyperlinkId::Implicit(1), false).unwrap();
        let alt0 = l.get("http://a", &HyperlinkId::Implicit(0), true).unwrap();
        assert!(Arc::ptr_eq(&a, &a2));
        let numbers = [&a, &b, &i0, &i1, &alt0].map(|l| l.number());
        assert!(numbers.windows(2).all(|w| w[0] < w[1]), "{numbers:?}");
        assert!(l.get("http://a", &HyperlinkId::None, false).is_none());
        assert_eq!(a.id(), format!("tmux{:X}", a.number()));
        assert_eq!(
            l.get("a\x01b", &HyperlinkId::Implicit(9), false)
                .unwrap()
                .uri(),
            "a\\001b"
        );
    }

    /// Past the limit, links no cell holds are dropped; held ones stay.
    #[test]
    fn unheld_links_are_pruned() {
        let mut l = Links::default();
        let held = l
            .get("http://held", &HyperlinkId::Implicit(0), false)
            .unwrap();
        for n in 1..=u32::try_from(PRUNE_AT).unwrap() {
            let _ = l.get("http://x", &HyperlinkId::Implicit(n), false);
        }
        assert_eq!(l.map.len(), 2);
        let again = l
            .get("http://held", &HyperlinkId::Implicit(0), false)
            .unwrap();
        assert!(Arc::ptr_eq(&held, &again));
    }

    /// An imported link keeps its number and the counter moves past it.
    #[test]
    fn import_keeps_numbers() {
        let mut l = Links::default();
        let far = l.numbers.next() + 1000;
        l.import(&[("http://a".into(), LinkKeyId::Explicit(b"i".to_vec()), far)]);
        assert_eq!(
            l.export(),
            [("http://a".into(), LinkKeyId::Explicit(b"i".to_vec()), far)]
        );
        let a = l
            .get("http://a", &HyperlinkId::Explicit(b"i".to_vec()), false)
            .unwrap();
        assert_eq!(a.number(), far);
        assert_eq!(l.numbers.next(), far + 1);
    }

    /// One link per URI, and the first draw of one takes two numbers, as
    /// in tmux: `tmux2`, then `tmux4`.
    #[test]
    fn a_format_link_is_one_per_uri() {
        let n = LinkNumbers::default();
        let f = n.format_link("https://f");
        assert_eq!(f.number(), 2);
        assert!(Arc::ptr_eq(&f, &n.format_link("https://f")));
        assert_eq!(n.format_link("https://g").number(), 4);
        assert_eq!(n.next(), 5);
    }

    /// Clones are one counter; another counter numbers on its own, and a
    /// read moves none.
    #[test]
    fn link_numbers_are_shared_by_clones_only() {
        let server = LinkNumbers::default();
        let pane = server.clone();
        assert!(pane.same(&server));
        assert_eq!(server.fresh("http://a").number(), 1);
        assert_eq!(pane.fresh("http://b").number(), 2);
        let other = LinkNumbers::default();
        assert!(!other.same(&server));
        assert_eq!(other.fresh("http://c").number(), 1);
        assert_eq!((server.next(), server.next()), (3, 3));
        server.reach(10);
        server.reach(7);
        assert_eq!(pane.next(), 10);
    }
}
