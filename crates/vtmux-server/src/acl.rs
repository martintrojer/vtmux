//! Who may connect to the server (server-acl.c): users and groups, each
//! writable or read-only. The server's owner (and root) are in it from
//! the start; a client whose uid, then gid, has no entry is turned away,
//! and one whose entry is read-only gets the read-only flag.

use std::collections::BTreeMap;

/// An entry's key. Users sort before groups, then by id
/// (`server_acl_cmp`), which is the order `server-access -l` lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Subject {
    User(u32),
    Group(u32),
}

/// What a client's ids give it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Denied,
    ReadOnly,
    Write,
}

/// The entries, each with its read-only flag.
#[derive(Clone, Debug, Default)]
pub struct Acl(BTreeMap<Subject, bool>);

impl Acl {
    /// A list of these entries (a snapshot's, D27).
    #[must_use]
    pub fn from_entries(entries: impl IntoIterator<Item = (Subject, bool)>) -> Self {
        Self(entries.into_iter().collect())
    }

    /// `server_acl_init`: root and `owner`, writable.
    #[must_use]
    pub fn new(owner: u32) -> Self {
        let mut acl = Self::default();
        acl.allow(Subject::User(0));
        acl.allow(Subject::User(owner));
        acl
    }

    #[must_use]
    pub fn contains(&self, s: Subject) -> bool {
        self.0.contains_key(&s)
    }

    /// `server_acl_allow`: added writable; an existing entry is kept.
    pub fn allow(&mut self, s: Subject) {
        self.0.entry(s).or_insert(false);
    }

    /// `server_acl_deny`: removed.
    pub fn deny(&mut self, s: Subject) {
        self.0.remove(&s);
    }

    /// `server_acl_allow_write` / `server_acl_deny_write` on an existing
    /// entry.
    pub fn set_read_only(&mut self, s: Subject, read_only: bool) {
        if let Some(r) = self.0.get_mut(&s) {
            *r = read_only;
        }
    }

    /// `server_acl_check`: the uid's entry, else the gid's; no ids (the
    /// system could not say) is no access.
    #[must_use]
    pub fn check(&self, ids: Option<(u32, u32)>) -> Access {
        let Some((uid, gid)) = ids else {
            return Access::Denied;
        };
        match self
            .0
            .get(&Subject::User(uid))
            .or_else(|| self.0.get(&Subject::Group(gid)))
        {
            None => Access::Denied,
            Some(true) => Access::ReadOnly,
            Some(false) => Access::Write,
        }
    }

    /// The entries in listing order, with their read-only flags.
    pub fn entries(&self) -> impl Iterator<Item = (Subject, bool)> + '_ {
        self.0.iter().map(|(&s, &r)| (s, r))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_and_root_start_writable_and_others_are_denied() {
        let acl = Acl::new(1000);
        assert_eq!(acl.check(Some((1000, 1000))), Access::Write);
        assert_eq!(acl.check(Some((0, 0))), Access::Write);
        assert_eq!(acl.check(Some((1001, 1000))), Access::Denied);
        assert_eq!(acl.check(None), Access::Denied);
    }

    #[test]
    fn the_user_entry_beats_the_group_entry() {
        let mut acl = Acl::new(1000);
        acl.allow(Subject::Group(50));
        acl.set_read_only(Subject::Group(50), true);
        assert_eq!(acl.check(Some((7, 50))), Access::ReadOnly);
        acl.allow(Subject::User(7));
        assert_eq!(acl.check(Some((7, 50))), Access::Write);
        acl.deny(Subject::User(7));
        acl.set_read_only(Subject::Group(50), false);
        assert_eq!(acl.check(Some((7, 50))), Access::Write);
    }

    #[test]
    fn users_list_before_groups_by_id() {
        let mut acl = Acl::new(1000);
        acl.allow(Subject::Group(1));
        acl.allow(Subject::User(65534));
        let order: Vec<_> = acl.entries().map(|(s, _)| s).collect();
        assert_eq!(
            order,
            [
                Subject::User(0),
                Subject::User(1000),
                Subject::User(65534),
                Subject::Group(1)
            ]
        );
    }
}
