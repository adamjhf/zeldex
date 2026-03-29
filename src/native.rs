use std::fs;
use std::io::Write;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{connect, Message, WebSocket};

use crate::status::AgentStatusKind;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RuntimeRecord {
    pub pid: u32,
    pub ws_url: String,
    pub cwd: String,
    pub started_at: u64,
    pub app_server_pid: u32,
}

#[derive(Clone, Debug)]
pub struct LiveThreadStatus {
    pub thread_id: Option<String>,
    pub thread_name: Option<String>,
    pub status: AgentStatusKind,
    pub updated_at: u64,
}

pub struct AppServerClient {
    websocket: WebSocket<MaybeTlsStream<TcpStream>>,
    next_id: u64,
}

impl AppServerClient {
    pub fn connect_to(ws_url: &str) -> Result<Self> {
        let (websocket, _) = connect(ws_url)?;
        let mut client = Self {
            websocket,
            next_id: 1,
        };
        client.initialize()?;
        Ok(client)
    }

    pub fn live_thread_status(&mut self) -> Result<Option<LiveThreadStatus>> {
        let loaded = self.request(
            "thread/loaded/list",
            json!({
                "limit": 16,
            }),
        )?;
        let Some(thread_ids) = loaded.get("data").and_then(Value::as_array).cloned() else {
            return Ok(None);
        };

        let mut best: Option<LiveThreadStatus> = None;
        for thread_id in thread_ids
            .into_iter()
            .filter_map(|value| value.as_str().map(ToOwned::to_owned))
        {
            let thread = self.request(
                "thread/read",
                json!({
                    "threadId": thread_id,
                    "includeTurns": false,
                }),
            )?;
            let Some(thread) = thread.get("thread") else {
                continue;
            };

            let updated_at = thread.get("updatedAt").and_then(Value::as_u64).unwrap_or(0);
            let status = map_thread_status(thread.get("status"));
            let candidate = LiveThreadStatus {
                thread_id: thread
                    .get("id")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                thread_name: thread
                    .get("name")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
                    .or_else(|| {
                        thread
                            .get("preview")
                            .and_then(Value::as_str)
                            .map(|preview| preview.chars().take(48).collect())
                    }),
                status,
                updated_at,
            };
            if best
                .as_ref()
                .map(|current| current.updated_at < candidate.updated_at)
                .unwrap_or(true)
            {
                best = Some(candidate);
            }
        }
        Ok(best)
    }

    fn initialize(&mut self) -> Result<()> {
        self.request(
            "initialize",
            json!({
                "clientInfo": {
                    "name": "zeldex",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "capabilities": {
                    "experimentalApi": true,
                    "optOutNotificationMethods": [
                        "thread/started",
                        "turn/started",
                        "turn/completed",
                        "thread/status/changed",
                    ],
                },
            }),
        )?;
        Ok(())
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.websocket
            .send(Message::Text(request.to_string()))
            .context("write app-server request")?;

        loop {
            let message = self.websocket.read().context("read app-server response")?;
            let Message::Text(text) = message else {
                continue;
            };
            let payload: Value = serde_json::from_str(&text).context("decode app-server json")?;
            if payload.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = payload.get("error") {
                bail!("app-server error for {method}: {error}");
            }
            return payload
                .get("result")
                .cloned()
                .ok_or_else(|| anyhow!("missing result for {method}"));
        }
    }
}

pub fn runtime_dir() -> PathBuf {
    home_dir().join(".local/state/zeldex/pids")
}

pub fn runtime_path_for_pid(pid: u32) -> PathBuf {
    runtime_dir().join(format!("{pid}.json"))
}

pub fn write_runtime(record: &RuntimeRecord) -> Result<()> {
    let path = runtime_path_for_pid(record.pid);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    write_json_atomic(&path, record)
}

pub fn read_runtime(pid: u32) -> Result<RuntimeRecord> {
    let path = runtime_path_for_pid(pid);
    let text = fs::read_to_string(&path)
        .with_context(|| format!("read runtime record {}", path.display()))?;
    Ok(serde_json::from_str(&text)?)
}

pub fn remove_runtime(pid: u32) {
    let _ = fs::remove_file(runtime_path_for_pid(pid));
}

pub fn now_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn write_json_atomic(path: &Path, value: &impl Serialize) -> Result<()> {
    let temp = path.with_extension("json.tmp");
    let mut file = fs::File::create(&temp)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.flush()?;
    fs::rename(temp, path)?;
    Ok(())
}

fn map_thread_status(status: Option<&Value>) -> AgentStatusKind {
    let Some(status) = status else {
        return AgentStatusKind::Running;
    };
    let Some(kind) = status.get("type").and_then(Value::as_str) else {
        return AgentStatusKind::Running;
    };
    match kind {
        "active" => {
            let waiting = status
                .get("activeFlags")
                .and_then(Value::as_array)
                .map(|flags| {
                    flags.iter().any(|flag| {
                        matches!(
                            flag.as_str(),
                            Some("waitingOnUserInput") | Some("waitingOnApproval")
                        )
                    })
                })
                .unwrap_or(false);
            if waiting {
                AgentStatusKind::Waiting
            } else {
                AgentStatusKind::Running
            }
        }
        "idle" | "notLoaded" | "systemError" => AgentStatusKind::Done,
        _ => AgentStatusKind::Running,
    }
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::map_thread_status;
    use crate::status::AgentStatusKind;

    #[test]
    fn maps_waiting_flags_to_waiting() {
        let status = json!({
            "type": "active",
            "activeFlags": ["waitingOnUserInput"],
        });
        assert_eq!(map_thread_status(Some(&status)), AgentStatusKind::Waiting);
    }

    #[test]
    fn maps_active_without_flags_to_running() {
        let status = json!({
            "type": "active",
            "activeFlags": [],
        });
        assert_eq!(map_thread_status(Some(&status)), AgentStatusKind::Running);
    }

    #[test]
    fn maps_idle_to_done() {
        let status = json!({
            "type": "idle",
        });
        assert_eq!(map_thread_status(Some(&status)), AgentStatusKind::Done);
    }
}
