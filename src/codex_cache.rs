use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::status::AgentStatusKind;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CodexCache {
    #[serde(default)]
    pub threads: BTreeMap<String, CachedThreadRecord>,
    #[serde(default)]
    pub bindings: BTreeMap<String, CachedPaneBinding>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CachedThreadRecord {
    pub transcript_path: String,
    pub file_size: u64,
    pub modified_at: u64,
    pub thread_id: String,
    #[serde(default)]
    pub thread_name: Option<String>,
    pub project_dir: String,
    pub status: AgentStatusKind,
    pub updated_at: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CachedPaneBinding {
    pub pane_id: String,
    pub pid: u32,
    pub cwd: String,
    pub transcript_path: String,
    pub last_seen_at: u64,
}

pub fn load_codex_cache() -> CodexCache {
    let path = codex_cache_path();
    let Ok(text) = fs::read_to_string(path) else {
        return CodexCache::default();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

pub fn save_codex_cache(cache: &CodexCache) -> Result<()> {
    let path = codex_cache_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    write_json_atomic(&path, cache)
}

pub fn codex_cache_path() -> PathBuf {
    state_dir().join("codex-cache.json")
}

fn state_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".local/state/zeldex")
}

fn write_json_atomic(path: &Path, value: &impl Serialize) -> Result<()> {
    let temp = path.with_extension("json.tmp");
    let mut file = fs::File::create(&temp)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.flush()?;
    fs::rename(temp, path)?;
    Ok(())
}
