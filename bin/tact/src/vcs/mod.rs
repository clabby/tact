//! Read-only access to version-controlled checkouts and their changes.
//!
//! These modules shell out to git and know nothing about sessions or front-ends: [`checkout`]
//! resolves the checkouts that share a repository, and [`diff`] captures the changes in one of
//! them for review.

pub(crate) mod checkout;
pub(crate) mod diff;
