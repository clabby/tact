//! Stable identities for the primary and optional forked sessions.

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum PaneId {
    Main,
    Fork(u64),
}
