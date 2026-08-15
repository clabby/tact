//! Stable identities for sessions that can move between primary and fork roles.

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum PaneId {
    Main,
    Fork(u64),
}
