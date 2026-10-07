//! SQL expressions: parsing and reference analysis ([`syntax`]), type
//! checking against a scope of named columns ([`check`]), the typed tree
//! ([`ir`]), and evaluation ([`eval`]).
//!
//! This crate deliberately doesn't depend on `tidepool-model`: the model and
//! the engine resolve names by implementing [`check::Scope`], and slots in
//! the typed tree mean whatever the caller decided.

pub mod check;
pub mod eval;
pub mod ir;
pub mod scalar;
pub mod syntax;
pub mod temporal;

pub use check::{Scope, check_aggregation, check_row};
pub use ir::{AggCall, AggFunc, Aggregation, Expr};
pub use scalar::{Scalar, Type};
