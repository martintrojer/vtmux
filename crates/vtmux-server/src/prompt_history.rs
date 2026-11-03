//! Prompt history (prompt-history.c): one list per prompt type, oldest
//! first, capped by `prompt-history-limit`. A line equal to the newest is
//! not added again. `history-file` is read when the config has run and
//! written when the server exits (cfg.c:59, server.c:261).

use std::path::{Path, PathBuf};

/// `enum prompt_type`: 3.8 has only these two (`PROMPT_NTYPES` 2,
/// prompt.c:1712-1736).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PromptType {
    #[default]
    Command,
    Search,
}

impl PromptType {
    pub const ALL: [Self; 2] = [Self::Command, Self::Search];

    /// `prompt_type`: `None` is `PROMPT_TYPE_INVALID`.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.name() == s)
    }

    /// `prompt_type_string`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Command => "command",
            Self::Search => "search",
        }
    }

    fn index(self) -> usize {
        match self {
            Self::Command => 0,
            Self::Search => 1,
        }
    }
}

/// Where a prompt is in each history list (`pr->hindex`): 0 is the line
/// being typed, `n` the `n`th newest entry.
pub type Position = [usize; 2];

/// Every type's history (`prompt_hlist`, `prompt_hsize`).
#[derive(Debug, Default)]
pub struct History {
    lists: [Vec<String>; 2],
}

impl History {
    /// `prompt_add_history` with `prompt-history-limit` `limit`.
    pub fn add(&mut self, line: &str, ty: PromptType, limit: usize) {
        let list = &mut self.lists[ty.index()];
        let new = list.last().is_none_or(|l| l != line);
        if limit > list.len() {
            if new {
                list.push(line.to_owned());
            }
            return;
        }
        let free = (list.len() + usize::from(new) - limit).min(list.len());
        if free == 0 {
            return;
        }
        list.drain(..free);
        if new && limit > 0 {
            list.push(line.to_owned());
        }
    }

    /// `prompt_up_history`: the next older line, `None` at the oldest.
    pub fn up(&self, at: &mut Position, ty: PromptType) -> Option<&str> {
        let list = &self.lists[ty.index()];
        let i = &mut at[ty.index()];
        if list.is_empty() || *i == list.len() {
            return None;
        }
        *i += 1;
        Some(&list[list.len() - *i])
    }

    /// `prompt_down_history`: the next newer line; past the newest, the
    /// empty line.
    pub fn down(&self, at: &mut Position, ty: PromptType) -> &str {
        let list = &self.lists[ty.index()];
        let i = &mut at[ty.index()];
        if list.is_empty() || *i == 0 {
            return "";
        }
        *i -= 1;
        if *i == 0 {
            return "";
        }
        &list[list.len() - *i]
    }

    /// The lines of one type, oldest first.
    #[must_use]
    pub fn lines(&self, ty: PromptType) -> &[String] {
        &self.lists[ty.index()]
    }

    /// `prompt_add_typed_history`: a loaded `type:line`; a line without a
    /// known type prefix is a command line (an old file).
    fn add_typed(&mut self, line: &str, limit: usize) {
        let typed = line
            .split_once(':')
            .and_then(|(t, rest)| Some((PromptType::parse(t)?, rest)));
        match typed {
            Some((ty, rest)) => self.add(rest, ty, limit),
            None => self.add(line, PromptType::Command, limit),
        }
    }

    /// `prompt_load_history`: add the lines of `path`; a file that cannot
    /// be read adds nothing.
    pub fn load(&mut self, path: &Path, limit: usize) {
        let Ok(data) = std::fs::read(path) else {
            return;
        };
        // getline: every line counts, an empty one too (it is an empty
        // command line); the piece after the last newline is no line.
        let mut lines: Vec<&[u8]> = data.split(|&b| b == b'\n').collect();
        if lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }
        for line in lines {
            self.add_typed(&String::from_utf8_lossy(line), limit);
        }
    }

    /// `prompt_save_history`: every type's lines as `type:line`.
    pub fn save(&self, path: &Path) {
        let mut out = String::new();
        for ty in PromptType::ALL {
            for line in self.lines(ty) {
                out.push_str(ty.name());
                out.push(':');
                out.push_str(line);
                out.push('\n');
            }
        }
        let _ = std::fs::write(path, out);
    }

    /// `prompt_history_clear`.
    pub fn clear(&mut self, ty: PromptType) {
        self.lists[ty.index()].clear();
    }
}

/// `prompt_find_history_file`: an absolute `history-file`, or one that
/// starts `~/`; anything else (and empty) is no file.
#[must_use]
pub fn history_path(option: &str, home: Option<&Path>) -> Option<PathBuf> {
    if option.starts_with('/') {
        return Some(PathBuf::from(option));
    }
    let rest = option.strip_prefix("~/")?;
    Some(PathBuf::from(format!("{}/{rest}", home?.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_skips_a_repeat_and_keeps_the_limit() {
        let mut h = History::default();
        for l in ["a", "b", "b", "c"] {
            h.add(l, PromptType::Command, 2);
        }
        assert_eq!(h.lines(PromptType::Command), ["b", "c"]);
        assert_eq!(h.lines(PromptType::Search), [] as [String; 0]);
        // A lower limit trims the oldest at the next add, even a repeat.
        h.add("c", PromptType::Command, 1);
        assert_eq!(h.lines(PromptType::Command), ["c"]);
        h.add("d", PromptType::Command, 0);
        assert_eq!(h.lines(PromptType::Command), [] as [String; 0]);
    }

    #[test]
    fn file_round_trip_and_old_format() {
        let dir = std::env::temp_dir().join(format!("vtmux-hist-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("h");
        std::fs::write(&path, "command:a\nsearch:b:c\nplain\nfoo:bar\n\n").unwrap();
        let mut h = History::default();
        h.load(&path, 100);
        assert_eq!(h.lines(PromptType::Command), ["a", "plain", "foo:bar", ""]);
        assert_eq!(h.lines(PromptType::Search), ["b:c"]);
        let out = dir.join("o");
        h.save(&out);
        assert_eq!(
            std::fs::read_to_string(out).unwrap(),
            "command:a\ncommand:plain\ncommand:foo:bar\ncommand:\nsearch:b:c\n"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn history_path_forms() {
        let home = Path::new("/h");
        assert_eq!(history_path("", Some(home)), None);
        assert_eq!(history_path("rel", Some(home)), None);
        assert_eq!(history_path("/x/y", None), Some("/x/y".into()));
        assert_eq!(history_path("~/z", Some(home)), Some("/h/z".into()));
        assert_eq!(history_path("~/z", None), None);
    }

    #[test]
    fn up_and_down_walk_the_list() {
        let mut h = History::default();
        h.add("a", PromptType::Search, 100);
        h.add("b", PromptType::Search, 100);
        let mut at = Position::default();
        assert_eq!(h.up(&mut at, PromptType::Search), Some("b"));
        assert_eq!(h.up(&mut at, PromptType::Search), Some("a"));
        assert_eq!(h.up(&mut at, PromptType::Search), None);
        assert_eq!(h.down(&mut at, PromptType::Search), "b");
        assert_eq!(h.down(&mut at, PromptType::Search), "");
        assert_eq!(h.down(&mut at, PromptType::Search), "");
        assert_eq!(h.up(&mut at, PromptType::Command), None);
    }
}
