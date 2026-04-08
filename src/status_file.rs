use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::status::AgentStatusKind;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StatusSnapshot {
    #[serde(default)]
    pub panes: BTreeMap<String, PaneStatusEntry>,
    pub updated_at: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PaneStatusEntry {
    pub pane_id: String,
    pub status: AgentStatusKind,
    pub updated_at: u64,
    #[serde(default)]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub thread_name: Option<String>,
}
