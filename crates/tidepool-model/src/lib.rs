//! The compiled, validated model (Model IR), the catalog of stable ids, and
//! diffing two models into a plan.
//!
//! [`compile`] turns a parsed [`ConfigSet`](tidepool_config::ConfigSet) into a
//! [`Model`], checking everything that spans files; [`plan::diff`] compares
//! two compiled models and classifies each change (plan §8.2).

pub mod catalog;
mod compile;
pub mod ir;
pub mod plan;

pub use catalog::Catalog;
pub use compile::{Compiled, compile};
pub use ir::Model;
