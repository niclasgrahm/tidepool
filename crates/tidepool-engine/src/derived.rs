//! Derived dictionaries (plan §4.4): a derived field over one
//! dictionary-encoded column, evaluated once per dictionary value instead of
//! once per row.
//!
//! The distinct results get codes of their own, so the field reads like a
//! stored dictionary column: `map[base code] → derived code`, and
//! `values[derived code] → value`. Derived code 0 is NULL. Base code 0 (a
//! NULL in the column, or a missing related row) maps like any other code:
//! `COALESCE(name, 'n/a')` gives 'n/a' for both, as SQL does.
//!
//! Dictionaries only ever grow and codes never change, so a derived
//! dictionary stays valid as its base grows; [`DerivedDict::extend`] adds
//! the new codes.

use std::collections::HashMap;

use tidepool_expr::eval::{Row, eval};
use tidepool_expr::{Expr, Scalar};
use tidepool_store::dict::DictSnapshot;
use tidepool_store::store::ColumnLayout;

use crate::exec::{Key, key};
use crate::fast::dict_value;

#[derive(Clone)]
pub(crate) struct DerivedDict {
    /// Base code → derived code.
    pub map: Vec<u32>,
    /// Derived code → value; entry 0 is NULL.
    pub values: Vec<Scalar>,
    index: HashMap<Key, u32>,
}

/// A row with one slot set: the base column's value.
struct OneSlot {
    slot: u32,
    value: Scalar,
}

impl Row for OneSlot {
    fn get(&self, slot: u32) -> Scalar {
        if slot == self.slot {
            self.value.clone()
        } else {
            Scalar::Null
        }
    }
}

impl DerivedDict {
    pub(crate) fn empty() -> Self {
        Self {
            map: Vec::new(),
            values: vec![Scalar::Null],
            index: HashMap::new(),
        }
    }

    /// A dictionary built elsewhere (a hierarchy's ancestors): `values` by
    /// code, `map` from base codes into them. Not to be extended.
    pub(crate) fn from_parts(map: Vec<u32>, values: Vec<Scalar>) -> Self {
        Self {
            map,
            values,
            index: HashMap::new(),
        }
    }

    /// `self` with every code of `dict` mapped, evaluating `expr` (whose only
    /// column is `slot`) for the codes it hasn't seen.
    pub(crate) fn extend(
        &self,
        expr: &Expr,
        slot: u32,
        layout: &ColumnLayout,
        dict: &DictSnapshot,
    ) -> Self {
        let mut next = self.clone();
        let codes = dict.len() + 1;
        for code in u32::try_from(next.map.len()).unwrap_or(u32::MAX)..codes {
            let value = eval(
                expr,
                &OneSlot {
                    slot,
                    value: dict_value(layout, Some(dict), code),
                },
            );
            let derived = if value.is_null() {
                0
            } else {
                let fresh = u32::try_from(next.values.len()).unwrap_or(u32::MAX);
                *next.index.entry(key(&value)).or_insert_with(|| {
                    next.values.push(value);
                    fresh
                })
            };
            next.map.push(derived);
        }
        next
    }

    /// Whether every code of `dict` is mapped.
    pub(crate) fn covers(&self, dict: &DictSnapshot) -> bool {
        self.map.len() == dict.len() as usize + 1
    }
}
