//! `tidepool.yaml`: how the server runs. Not part of the data model, so
//! changing it never needs a plan beyond "restart" or "apply".

use std::net::SocketAddr;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::types::{HumanDuration, MemorySize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Durability {
    /// Acknowledge ingest after the write-ahead log is fsynced.
    #[default]
    Fsync,
    /// Acknowledge before the fsync: faster, and a crash can lose the last
    /// moments of acknowledged data.
    Async,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Address to listen on. `tidepool serve --listen` overrides it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen: Option<SocketAddr>,
    /// Where the server keeps its state. `--data` overrides it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_dir: Option<String>,
    /// How often new data becomes visible, at most.
    #[serde(default = "default_epoch_interval")]
    pub epoch_interval: HumanDuration,
    /// Memory for data and caches together.
    #[serde(default = "default_memory_budget")]
    pub memory_budget: MemorySize,
    #[serde(default)]
    pub durability: Durability,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: None,
            data_dir: None,
            epoch_interval: default_epoch_interval(),
            memory_budget: default_memory_budget(),
            durability: Durability::default(),
        }
    }
}

fn default_epoch_interval() -> HumanDuration {
    HumanDuration(std::time::Duration::from_millis(200))
}

fn default_memory_budget() -> MemorySize {
    MemorySize::Percent(75)
}
