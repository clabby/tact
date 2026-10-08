//! Stable identities for the live sessions one process runs.
//!
//! An identity is never reused within a process, so late completions for a closed pane cannot
//! reach a newer one.

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum PaneId {
    /// The session Tact started with.
    Main,
    /// A fork of another live session.
    Fork(u64),
    /// A new or resumed session opened beside the others.
    Opened(u64),
}
