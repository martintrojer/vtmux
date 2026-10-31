//! Prompt history (prompt-history.c): one list per prompt type, oldest
//! first, capped by `prompt-history-limit`. A line equal to the newest is
//! not added again. Loading and saving `history-file` is not here yet.

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

    /// `prompt_history_clear`.
    pub fn clear(&mut self, ty: PromptType) {
        self.lists[ty.index()].clear();
    }
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
