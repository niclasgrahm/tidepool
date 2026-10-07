//! Serde types for every Tidepool config file: `tidepool.yaml`, `tables/*.yaml`,
//! `model.ossie.yaml` (Apache Ossie + the `tidepool` extensions) and
//! `dashboards/*.yaml`, plus their JSON Schemas and span-carrying parse errors.
//!
//! Pure data and parsing only, no validation across files: that is
//! `tidepool-model`. Apache-2.0 so that tools generating or checking Tidepool
//! config (Kayak, editors, agents) can depend on it freely. Lands in M1.
