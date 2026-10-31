//! The paste buffer store (tmux paste.c): automatic buffers `buffer0`,
//! `buffer1`, ... evicted oldest-first at `buffer-limit`, named buffers
//! that never count against it, and the two `vis(3)` encodings the edge
//! needs (`buffer_sample`, and `paste-buffer` without `-S`).

use std::collections::BTreeMap;

use crate::model::now_epoch;
use crate::names::{check_name, clean_name};

/// One paste buffer. The data is bytes, not necessarily text.
#[derive(Clone, Debug)]
pub struct PasteBuffer {
    pub name: String,
    pub data: Vec<u8>,
    /// Made by `paste_add` (and not renamed or replaced since): only these
    /// count against `buffer-limit` and are the "top" buffer.
    pub automatic: bool,
    /// Creation order, the time sort key (`pb->order`).
    pub order: u64,
    /// Seconds since the epoch (`#{buffer_created}`).
    pub created: i64,
}

impl PasteBuffer {
    /// `paste_make_sample` (paste.c:313-330): the first 200 bytes through
    /// `vis`, then `...` when the data or its encoding is longer.
    #[must_use]
    pub fn sample(&self) -> String {
        const WIDTH: usize = 200;
        let len = self.data.len().min(WIDTH);
        let mut out = strvis(&self.data[..len], Vis::Sample);
        if self.data.len() > WIDTH || out.len() > WIDTH {
            out.truncate(WIDTH);
            out.extend_from_slice(b"...");
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    /// `format_cb_buffer_full`: `xstrndup` stops at a NUL.
    #[must_use]
    pub fn full(&self) -> String {
        let end = self
            .data
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(self.data.len());
        String::from_utf8_lossy(&self.data[..end]).into_owned()
    }
}

/// The server's set of paste buffers.
#[derive(Debug, Default)]
pub struct PasteStore {
    by_name: BTreeMap<String, PasteBuffer>,
    /// `paste_next_index`: automatic names keep counting up.
    next_index: u32,
    /// `paste_next_order`.
    next_order: u64,
}

impl PasteStore {
    /// `paste_get_name`: an empty name is no buffer.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&PasteBuffer> {
        if name.is_empty() {
            return None;
        }
        self.by_name.get(name)
    }

    /// `paste_get_top`: the most recent automatic buffer.
    #[must_use]
    pub fn top(&self) -> Option<&PasteBuffer> {
        self.by_name
            .values()
            .filter(|b| b.automatic)
            .max_by_key(|b| b.order)
    }

    /// `paste_walk`: most recent first.
    #[must_use]
    pub fn by_time(&self) -> Vec<&PasteBuffer> {
        let mut all: Vec<_> = self.by_name.values().collect();
        all.sort_by_key(|b| std::cmp::Reverse(b.order));
        all
    }

    /// `paste_free`.
    pub fn remove(&mut self, name: &str) {
        self.by_name.remove(name);
    }

    fn automatic_count(&self) -> usize {
        self.by_name.values().filter(|b| b.automatic).count()
    }

    fn next_order(&mut self) -> u64 {
        let order = self.next_order;
        self.next_order += 1;
        order
    }

    /// `paste_add` (paste.c:167-208): evict the oldest automatic buffers
    /// down to `limit`, then add `bufferN`. Empty data is dropped.
    pub fn add(&mut self, data: Vec<u8>, limit: i64) {
        self.add_prefixed(data, "buffer", limit);
    }

    /// `paste_add` with a name prefix: the buffer is `<prefix>N`.
    pub fn add_prefixed(&mut self, data: Vec<u8>, prefix: &str, limit: i64) {
        if data.is_empty() {
            return;
        }
        let mut oldest_first: Vec<_> = self
            .by_name
            .values()
            .map(|b| (b.order, b.name.clone()))
            .collect();
        oldest_first.sort();
        for (_, name) in oldest_first {
            if i64::try_from(self.automatic_count()).unwrap_or(i64::MAX) < limit {
                break;
            }
            if self.by_name.get(&name).is_some_and(|b| b.automatic) {
                self.by_name.remove(&name);
            }
        }
        let name = loop {
            let name = format!("{prefix}{}", self.next_index);
            self.next_index = self.next_index.wrapping_add(1);
            if !self.by_name.contains_key(&name) {
                break name;
            }
        };
        let order = self.next_order();
        self.by_name.insert(
            name.clone(),
            PasteBuffer {
                name,
                data,
                automatic: true,
                order,
                created: now_epoch(),
            },
        );
    }

    /// `paste_set` (paste.c:258-306): add an automatic buffer (no name) or
    /// set a named one, replacing any buffer of that name. The error is
    /// tmux's cause text.
    pub fn set(&mut self, data: Vec<u8>, name: Option<&str>, limit: i64) -> Result<(), String> {
        if data.is_empty() {
            return Ok(());
        }
        let Some(name) = name else {
            self.add(data, limit);
            return Ok(());
        };
        if name.is_empty() {
            return Err("empty buffer name".into());
        }
        if !check_name(name) {
            return Err(format!("invalid buffer name: {name}"));
        }
        let name = clean_name(name);
        let order = self.next_order();
        self.by_name.insert(
            name.clone(),
            PasteBuffer {
                name,
                data,
                automatic: false,
                order,
                created: now_epoch(),
            },
        );
        Ok(())
    }

    /// `paste_rename` (paste.c:211-255): the renamed buffer becomes named
    /// and keeps its order; a buffer already called `new` is replaced.
    pub fn rename(&mut self, old: &str, new: &str) -> Result<(), String> {
        if old.is_empty() {
            return Err("no buffer".into());
        }
        if new.is_empty() {
            return Err("new name is empty".into());
        }
        if !check_name(new) {
            return Err(format!("invalid buffer name: {new}"));
        }
        let new = clean_name(new);
        if !self.by_name.contains_key(old) {
            return Err(format!("no buffer {old}"));
        }
        if new == old {
            return Ok(());
        }
        self.by_name.remove(&new);
        if let Some(mut b) = self.by_name.remove(old) {
            b.name.clone_from(&new);
            b.automatic = false;
            self.by_name.insert(new, b);
        }
        Ok(())
    }
}

/// The two `vis(3)` flag sets the edge uses.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Vis {
    /// `VIS_OCTAL|VIS_CSTYLE|VIS_TAB|VIS_NL` (`paste_make_sample`).
    Sample,
    /// `VIS_SAFE|VIS_NOSLASH` (`cmd_paste_buffer_paste`).
    Paste,
}

/// `vis` (compat/vis.c:56-135) for one byte; `next` is the byte after it.
fn vis_byte(out: &mut Vec<u8>, c: u8, next: u8, f: Vis) {
    let sample = f == Vis::Sample;
    let visible = (0x21..=0x7e).contains(&c)
        || c == b' '
        || (!sample && matches!(c, b'\t' | b'\n' | 0x08 | 0x07 | b'\r'));
    if visible {
        if c == b'\\' && sample {
            out.push(b'\\');
        }
        out.push(c);
        return;
    }
    if sample {
        let esc = match c {
            b'\n' => Some(b'n'),
            b'\r' => Some(b'r'),
            0x08 => Some(b'b'),
            0x07 => Some(b'a'),
            0x0b => Some(b'v'),
            b'\t' => Some(b't'),
            0x0c => Some(b'f'),
            0 => Some(b'0'),
            _ => None,
        };
        if let Some(e) = esc {
            out.extend_from_slice(&[b'\\', e]);
            if c == 0 && (b'0'..=b'7').contains(&next) {
                out.extend_from_slice(b"00");
            }
            return;
        }
    }
    if c & 0x7f == b' ' || sample {
        out.extend_from_slice(&[
            b'\\',
            (c >> 6 & 7) + b'0',
            (c >> 3 & 7) + b'0',
            (c & 7) + b'0',
        ]);
        return;
    }
    if sample {
        out.push(b'\\');
    }
    let mut c = c;
    if c & 0x80 != 0 {
        c &= 0x7f;
        out.push(b'M');
    }
    if c < 0x20 || c == 0x7f {
        out.push(b'^');
        out.push(if c == 0x7f { b'?' } else { c + b'@' });
    } else {
        out.extend_from_slice(&[b'-', c]);
    }
}

/// `utf8_strvis` (utf8.c:760-795): complete valid UTF-8 characters pass
/// through, every other byte goes through [`vis_byte`].
fn strvis(data: &[u8], f: Vis) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        let size = match data[i] {
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf4 => 4,
            _ => 0,
        };
        if size != 0
            && let Some(chunk) = data.get(i..i + size)
            && std::str::from_utf8(chunk).is_ok()
        {
            out.extend_from_slice(chunk);
            i += size;
            continue;
        }
        let next = data.get(i + 1).copied().unwrap_or(0);
        vis_byte(&mut out, data[i], next, f);
        i += 1;
    }
    out
}

/// `cmd_paste_buffer_paste`: one line of a buffer as the pane's input.
#[must_use]
pub fn vis_paste(data: &[u8]) -> Vec<u8> {
    strvis(data, Vis::Paste)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buf(data: &[u8]) -> PasteBuffer {
        PasteBuffer {
            name: "b".into(),
            data: data.to_vec(),
            automatic: true,
            order: 0,
            created: 0,
        }
    }

    #[test]
    fn automatic_names_evict_oldest_and_keep_named() {
        let mut s = PasteStore::default();
        s.set(b"keep".to_vec(), Some("keepme"), 3).unwrap();
        for d in ["a1", "a2", "a3", "a4"] {
            s.set(d.as_bytes().to_vec(), None, 3).unwrap();
        }
        let got: Vec<_> = s.by_time().iter().map(|b| b.name.clone()).collect();
        assert_eq!(got, ["buffer3", "buffer2", "buffer1", "keepme"]);
        assert_eq!(s.top().map(|b| b.name.as_str()), Some("buffer3"));
    }

    #[test]
    fn rename_makes_a_named_buffer_and_replaces() {
        let mut s = PasteStore::default();
        s.add(b"x".to_vec(), 50);
        s.set(b"y".to_vec(), Some("n"), 50).unwrap();
        s.rename("buffer0", "n").unwrap();
        assert!(s.top().is_none());
        assert_eq!(s.get("n").map(|b| b.data.clone()), Some(b"x".to_vec()));
        assert_eq!(s.rename("n", ""), Err("new name is empty".into()));
        assert_eq!(s.rename("zz", "q"), Err("no buffer zz".into()));
    }

    /// Reference: `set-buffer` of these bytes, `list-buffers -F '#{buffer_sample}'`.
    #[test]
    fn sample_escapes() {
        assert_eq!(buf(b"a\tb\n\\\x01\xff").sample(), "a\\tb\\n\\\\\\001\\377");
        assert_eq!(buf("é".as_bytes()).sample(), "é");
        assert_eq!(
            buf(&[b'x'; 201]).sample(),
            format!("{}...", "x".repeat(200))
        );
    }

    #[test]
    fn paste_encoding() {
        assert_eq!(
            vis_paste(b"a\x1b[0m\x7f\xa1\xa0\xc3\xa9\t\\"),
            b"a^[[0m^?M-!\\240\xc3\xa9\t\\"
        );
    }
}
