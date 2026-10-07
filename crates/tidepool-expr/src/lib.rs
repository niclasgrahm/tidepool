//! SQL expressions: parsing and analysis now ([`syntax`]); name resolution
//! against a schema, type checking and the vectorised kernels that evaluate
//! expressions arrive in M3.
//!
//! This crate deliberately doesn't depend on `tidepool-model`: the model
//! compiler uses [`syntax`] to resolve names, and later type checking will take
//! the schema through a trait the model implements.

pub mod syntax;
