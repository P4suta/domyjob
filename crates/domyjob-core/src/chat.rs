//! The effect-free AI chat domain.
//!
//! Identities, events, the single admission rule, local authoring policy, the exchange round,
//! and an in-memory reference ledger.

pub mod card;
pub mod event;
pub mod exchange;
#[cfg(test)]
pub(crate) mod fixtures;
pub mod id;
pub mod ledger;
pub mod model;
pub mod policy;
