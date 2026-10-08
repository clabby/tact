//! Read-only access to version-controlled checkouts and their changes.
//!
//! This crate shells out to `git` (and `jj` for jj workspaces) and knows nothing about sessions or
//! front-ends. Nothing it runs writes to the user's repository: jj runs with
//! `--ignore-working-copy`, and a jj workspace without `.git` is read through a private,
//! temporary git index.
//!
//! - [`Checkout`] resolves the checkout that contains a directory and the other checkouts of the
//!   same repository ([`Checkout::family`], [`family_paths`]).
//! - [`ReviewContext`] lists the reviewable points of a checkout and captures the change between
//!   any two of them as a [`DiffSnapshot`].
//! - [`WorkspaceVersion`] detects when a checkout changed underneath an open review.
//! - [`FilePatch`] parses a captured patch into files and hunks.

mod checkout;
mod diff;
mod error;
mod patch;
#[cfg(test)]
mod testing;

pub use checkout::{Checkout, CheckoutKind, FamilyMember, family_paths};
pub use diff::{
    DiffSnapshot, OverviewContext, OverviewRange, PatchSide, ReviewContext, ReviewRange,
    ReviewTarget, ReviewTargetKind, WorkspaceVersion,
};
pub use error::VcsError;
pub use patch::{FilePatch, Hunk, LineSpan};
