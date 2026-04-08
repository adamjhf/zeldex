use std::collections::{BTreeMap, HashMap};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::status::AgentStatusKind;
use crate::status_file::{PaneStatusEntry, StatusSnapshot};

const WAIT_AFTER_TOOL_CALL: Duration = Duration::from_secs(3);
const THREAD_NAME_MAX: usize = 80;
const SECTION_MARKER: char = '\u{1e}';

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaneTarget {
    pub pane_id: String,
    pub cwd: PathBuf,
}

#[derive(Clone, Debug, Default)]
pub struct RefreshCache {
    pub bindings: BTreeMap<String, CachedPaneBinding>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CachedPaneBinding {
    pub pane_id: String,
    pub cwd: String,
    pub transcript_path: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexFile {
    pub kind: CodexFileKind,
    pub modified_at: u64,
    pub path: PathBuf,
    pub content: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodexFileKind {
    Index,
    Transcript,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ThreadSummary {
    transcript_path: PathBuf,
    thread_id: String,
    thread_name: Option<String>,
    project_dir: PathBuf,
    status: AgentStatusKind,
    updated_at: u64,
}

pub fn parse_refresh_output(stdout: &[u8]) -> Result<Vec<CodexFile>, String> {
    let text = String::from_utf8_lossy(stdout);
    let mut files = Vec::new();
    let mut current: Option<CodexFile> = None;

    for line in text.lines() {
        if let Some(header) = line.strip_prefix(SECTION_MARKER) {
            if let Some(file) = current.take() {
                files.push(file);
            }
            current = Some(parse_section_header(header)?);
            continue;
        }

        if let Some(file) = current.as_mut() {
            file.content.push_str(line);
            file.content.push('\n');
        }
    }

    if let Some(file) = current {
        files.push(file);
    }

    Ok(files)
}

pub fn collect_status_snapshot(
    panes: &[PaneTarget],
    files: &[CodexFile],
    cache: &mut RefreshCache,
    now: SystemTime,
) -> StatusSnapshot {
    let updated_at = unix_seconds(now);
    let thread_names = load_thread_index(files);
    let threads = files
        .iter()
        .filter(|file| file.kind == CodexFileKind::Transcript)
        .filter_map(|file| summarize_thread(file, &thread_names, now))
        .collect::<Vec<_>>();
    let threads_by_path = threads
        .iter()
        .cloned()
        .map(|thread| (thread.transcript_path.clone(), thread))
        .collect::<HashMap<_, _>>();

    let mut snapshot = StatusSnapshot {
        panes: BTreeMap::new(),
        updated_at,
    };

    for pane in panes {
        let resolved = resolve_pane_thread(pane, &threads, &threads_by_path, cache);
        if let Some(thread) = resolved {
            snapshot.panes.insert(
                pane.pane_id.clone(),
                PaneStatusEntry {
                    pane_id: pane.pane_id.clone(),
                    status: thread.status,
                    updated_at: thread.updated_at,
                    thread_id: Some(thread.thread_id.clone()),
                    thread_name: thread.thread_name.clone(),
                },
            );
            cache.bindings.insert(
                pane.pane_id.clone(),
                CachedPaneBinding {
                    pane_id: pane.pane_id.clone(),
                    cwd: pane.cwd.to_string_lossy().into_owned(),
                    transcript_path: thread.transcript_path.to_string_lossy().into_owned(),
                },
            );
        } else {
            cache.bindings.remove(&pane.pane_id);
        }
    }

    prune_bindings(cache, panes, &threads_by_path);
    snapshot
}

fn parse_section_header(header: &str) -> Result<CodexFile, String> {
    let mut parts = header.splitn(3, '\t');
    let kind = match parts.next() {
        Some("index") => CodexFileKind::Index,
        Some("transcript") => CodexFileKind::Transcript,
        Some(other) => return Err(format!("unknown codex file kind: {other}")),
        None => return Err("missing codex file kind".to_owned()),
    };
    let modified_at = parts
        .next()
        .ok_or_else(|| "missing modified_at".to_owned())?
        .parse::<u64>()
        .map_err(|_| "invalid modified_at".to_owned())?;
    let path = parts
        .next()
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| "missing codex file path".to_owned())?;

    Ok(CodexFile {
        kind,
        modified_at,
        path: normalize_path(&path),
        content: String::new(),
    })
}

fn load_thread_index(files: &[CodexFile]) -> HashMap<String, String> {
    let mut names = HashMap::new();

    for file in files
        .iter()
        .filter(|file| file.kind == CodexFileKind::Index)
    {
        for line in file.content.lines() {
            let Ok(value) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let Some(id) = value.get("id").and_then(Value::as_str) else {
                continue;
            };
            let Some(name) = value.get("thread_name").and_then(Value::as_str) else {
                continue;
            };
            names.insert(id.to_owned(), name.to_owned());
        }
    }

    names
}

fn resolve_pane_thread(
    pane: &PaneTarget,
    threads: &[ThreadSummary],
    threads_by_path: &HashMap<PathBuf, ThreadSummary>,
    cache: &RefreshCache,
) -> Option<ThreadSummary> {
    let bound = cache
        .bindings
        .get(&pane.pane_id)
        .filter(|binding| normalize_path(Path::new(&binding.cwd)) == pane.cwd)
        .and_then(|binding| {
            threads_by_path
                .get(&normalize_path(Path::new(&binding.transcript_path)))
                .cloned()
        });
    let best = select_best_thread(threads, &pane.cwd);

    match (bound, best) {
        (Some(bound), Some(best)) if thread_sort_key(&best) > thread_sort_key(&bound) => Some(best),
        (Some(bound), _) => Some(bound),
        (None, some_best) => some_best,
    }
}

fn prune_bindings(
    cache: &mut RefreshCache,
    panes: &[PaneTarget],
    threads_by_path: &HashMap<PathBuf, ThreadSummary>,
) {
    let visible_panes = panes
        .iter()
        .map(|pane| (pane.pane_id.as_str(), &pane.cwd))
        .collect::<HashMap<_, _>>();

    cache.bindings.retain(|pane_id, binding| {
        let Some(cwd) = visible_panes.get(pane_id.as_str()) else {
            return false;
        };

        normalize_path(Path::new(&binding.cwd)) == **cwd
            && threads_by_path.contains_key(&normalize_path(Path::new(&binding.transcript_path)))
    });
}

fn select_best_thread(threads: &[ThreadSummary], pane_cwd: &Path) -> Option<ThreadSummary> {
    let pane_cwd = normalize_path(pane_cwd);

    threads
        .iter()
        .filter_map(|thread| {
            match_score(&pane_cwd, &thread.project_dir).map(|score| {
                (
                    score,
                    depth(&thread.project_dir),
                    thread_sort_key(thread),
                    thread.clone(),
                )
            })
        })
        .max_by_key(|(score, depth, sort_key, _)| (*score, *depth, *sort_key))
        .map(|(_, _, _, thread)| thread)
}

fn thread_sort_key(thread: &ThreadSummary) -> (u64, usize) {
    (thread.updated_at, thread.status.priority())
}

fn summarize_thread(
    file: &CodexFile,
    thread_names: &HashMap<String, String>,
    now: SystemTime,
) -> Option<ThreadSummary> {
    let modified_at = UNIX_EPOCH + Duration::from_secs(file.modified_at);
    let age = now.duration_since(modified_at).unwrap_or_default();
    let thread_id = parse_thread_id(&file.path);
    let mut status = AgentStatusKind::Idle;
    let mut project_dir = None;
    let mut thread_name = thread_names.get(&thread_id).cloned();
    let mut last_entry_was_tool_call = false;

    for line in file.content.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if project_dir.is_none() {
            project_dir = extract_project_dir(&entry).map(|dir| normalize_path(Path::new(&dir)));
        }
        if thread_name.is_none() {
            thread_name = extract_thread_name(&entry);
        }
        if let Some(next_status) = determine_status(&entry) {
            status = next_status;
            last_entry_was_tool_call = is_tool_call_entry(&entry);
        }
    }

    let project_dir = project_dir?;
    if status == AgentStatusKind::Idle {
        return None;
    }
    if status == AgentStatusKind::Running && last_entry_was_tool_call && age >= WAIT_AFTER_TOOL_CALL
    {
        status = AgentStatusKind::Waiting;
    }

    Some(ThreadSummary {
        transcript_path: file.path.clone(),
        thread_id,
        thread_name,
        project_dir,
        status,
        updated_at: file.modified_at,
    })
}

fn parse_thread_id(path: &Path) -> String {
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    stem.rsplit('-')
        .take(5)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("-")
}

fn extract_project_dir(entry: &Value) -> Option<String> {
    let kind = entry.get("type")?.as_str()?;
    if !matches!(kind, "session_meta" | "turn_context") {
        return None;
    }
    entry
        .get("payload")?
        .get("cwd")?
        .as_str()
        .map(ToOwned::to_owned)
}

fn determine_status(entry: &Value) -> Option<AgentStatusKind> {
    match entry.get("type").and_then(Value::as_str) {
        Some("event_msg") => match entry.get("payload")?.get("type")?.as_str()? {
            "task_complete" | "turn_aborted" => Some(AgentStatusKind::Done),
            "task_started" | "user_message" => Some(AgentStatusKind::Running),
            "agent_message" => {
                let phase = entry.get("payload")?.get("phase").and_then(Value::as_str);
                Some(if phase == Some("final_answer") {
                    AgentStatusKind::Done
                } else {
                    AgentStatusKind::Running
                })
            }
            _ => None,
        },
        Some("response_item") => match entry.get("payload")?.get("type")?.as_str()? {
            "message" => {
                let payload = entry.get("payload")?;
                match payload.get("role").and_then(Value::as_str) {
                    Some("developer") => None,
                    Some("assistant") => {
                        let phase = payload.get("phase").and_then(Value::as_str);
                        Some(if phase == Some("final_answer") {
                            AgentStatusKind::Done
                        } else {
                            AgentStatusKind::Running
                        })
                    }
                    Some("user") => Some(AgentStatusKind::Running),
                    _ => None,
                }
            }
            "function_call"
            | "function_call_output"
            | "reasoning"
            | "custom_tool_call"
            | "custom_tool_call_output"
            | "web_search_call" => Some(AgentStatusKind::Running),
            _ => None,
        },
        Some("message") => match entry.get("role").and_then(Value::as_str) {
            Some("user") | Some("assistant") => Some(AgentStatusKind::Running),
            _ => None,
        },
        Some("function_call") | Some("function_call_output") | Some("reasoning") => {
            Some(AgentStatusKind::Running)
        }
        _ => None,
    }
}

fn is_tool_call_entry(entry: &Value) -> bool {
    if matches!(
        entry.get("type").and_then(Value::as_str),
        Some("function_call")
    ) {
        return true;
    }

    if !matches!(
        entry.get("type").and_then(Value::as_str),
        Some("response_item")
    ) {
        return false;
    }

    matches!(
        entry
            .get("payload")
            .and_then(|payload| payload.get("type"))
            .and_then(Value::as_str),
        Some("function_call") | Some("custom_tool_call") | Some("web_search_call")
    )
}

fn extract_thread_name(entry: &Value) -> Option<String> {
    if entry.get("type").and_then(Value::as_str) == Some("event_msg")
        && entry
            .get("payload")
            .and_then(|payload| payload.get("type"))
            .and_then(Value::as_str)
            == Some("user_message")
    {
        let message = entry
            .get("payload")
            .and_then(|payload| payload.get("message"))
            .and_then(Value::as_str)?;
        return normalize_thread_name(message);
    }

    let from_payload = entry
        .get("payload")
        .filter(|_| entry.get("type").and_then(Value::as_str) == Some("response_item"))
        .filter(|payload| payload.get("type").and_then(Value::as_str) == Some("message"))
        .filter(|payload| payload.get("role").and_then(Value::as_str) == Some("user"))
        .and_then(|payload| payload.get("content"))
        .and_then(Value::as_array)
        .map(|content| {
            content
                .iter()
                .filter(|item| item.get("type").and_then(Value::as_str) == Some("input_text"))
                .filter_map(|item| item.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .and_then(|text| normalize_thread_name(&text));
    if from_payload.is_some() {
        return from_payload;
    }

    entry
        .get("content")
        .filter(|_| entry.get("type").and_then(Value::as_str) == Some("message"))
        .filter(|_| entry.get("role").and_then(Value::as_str) == Some("user"))
        .and_then(Value::as_array)
        .map(|content| {
            content
                .iter()
                .filter(|item| item.get("type").and_then(Value::as_str) == Some("input_text"))
                .filter_map(|item| item.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .and_then(|text| normalize_thread_name(&text))
}

fn normalize_thread_name(text: &str) -> Option<String> {
    let line = text.lines().map(str::trim).find(|line| !line.is_empty())?;
    if line.starts_with("<")
        || line.starts_with("{")
        || line.starts_with("# AGENTS.md")
        || line.starts_with("<environment_context>")
        || line.starts_with("<codex reminder>")
        || line.starts_with("<permissions ")
        || line.starts_with("<app-context>")
        || line.starts_with("<collaboration_mode>")
        || line.starts_with("<turn_aborted>")
    {
        return None;
    }
    Some(line.chars().take(THREAD_NAME_MAX).collect())
}

fn match_score(pane_cwd: &Path, project_dir: &Path) -> Option<usize> {
    if pane_cwd == project_dir {
        return Some(10_000 + depth(project_dir));
    }
    if pane_cwd.starts_with(project_dir) || project_dir.starts_with(pane_cwd) {
        return Some(common_prefix_len(pane_cwd, project_dir));
    }
    None
}

fn common_prefix_len(left: &Path, right: &Path) -> usize {
    left.components()
        .zip(right.components())
        .take_while(|(left, right)| left == right)
        .count()
}

fn depth(path: &Path) -> usize {
    path.components()
        .filter(|component| {
            matches!(
                component,
                Component::Normal(_) | Component::RootDir | Component::Prefix(_)
            )
        })
        .count()
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn unix_seconds(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime};

    use serde_json::json;

    use crate::status::AgentStatusKind;

    use super::{
        collect_status_snapshot, determine_status, extract_thread_name, is_tool_call_entry,
        normalize_path, parse_refresh_output, select_best_thread, summarize_thread,
        CachedPaneBinding, CodexFile, CodexFileKind, PaneTarget, RefreshCache, ThreadSummary,
        WAIT_AFTER_TOOL_CALL,
    };

    #[test]
    fn parses_refresh_sections() {
        let files = parse_refresh_output(
            b"\x1eindex\t0\t/tmp/index.jsonl\n{\"id\":\"a\",\"thread_name\":\"Fix auth\"}\n\x1etranscript\t12\t/tmp/thread.jsonl\n{\"type\":\"session_meta\",\"payload\":{\"cwd\":\"/tmp/project\"}}\n",
        )
        .unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].kind, CodexFileKind::Index);
        assert_eq!(files[1].modified_at, 12);
    }

    #[test]
    fn maps_codex_final_answer_to_done() {
        let entry = json!({
            "type": "event_msg",
            "payload": { "type": "agent_message", "phase": "final_answer" }
        });
        assert_eq!(determine_status(&entry), Some(AgentStatusKind::Done));
    }

    #[test]
    fn skips_system_prompt_thread_names() {
        let entry = json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{ "type": "input_text", "text": "# AGENTS.md\nignored" }]
            }
        });
        assert_eq!(extract_thread_name(&entry), None);
    }

    #[test]
    fn prefers_exact_cwd_match() {
        let threads = vec![
            thread(
                "/tmp/project/transcript.jsonl",
                "/tmp/project",
                AgentStatusKind::Running,
                1,
            ),
            thread("/tmp/transcript.jsonl", "/tmp", AgentStatusKind::Done, 99),
        ];
        let best = select_best_thread(&threads, Path::new("/tmp/project")).unwrap();
        assert_eq!(best.thread_id, "transcript");
    }

    #[test]
    fn waiting_after_tool_call_pause() {
        let file = CodexFile {
            kind: CodexFileKind::Transcript,
            modified_at: 10,
            path: PathBuf::from(
                "/tmp/sessions/2026/04/01/rollout-2026-04-01T10-00-00-019d3333-aaaa-bbbb-cccc-000000000001.jsonl",
            ),
            content: format!(
                "{}\n{}\n",
                json!({ "type": "session_meta", "payload": { "cwd": "/tmp/project" } }),
                json!({ "type": "response_item", "payload": { "type": "function_call" } })
            ),
        };
        let now = SystemTime::UNIX_EPOCH
            + Duration::from_secs(10)
            + WAIT_AFTER_TOOL_CALL
            + Duration::from_secs(1);
        let summary = summarize_thread(&file, &Default::default(), now).unwrap();
        assert_eq!(summary.status, AgentStatusKind::Waiting);
    }

    #[test]
    fn thread_index_names_win() {
        let mut cache = RefreshCache::default();
        let files = vec![
            CodexFile {
                kind: CodexFileKind::Index,
                modified_at: 0,
                path: PathBuf::from("/tmp/index.jsonl"),
                content: "{\"id\":\"aaaa-bbbb-cccc-dddd-eeee\",\"thread_name\":\"Fix auth\"}\n".into(),
            },
            CodexFile {
                kind: CodexFileKind::Transcript,
                modified_at: 12,
                path: PathBuf::from(
                    "/tmp/sessions/2026/04/01/rollout-2026-04-01T10-00-00-aaaa-bbbb-cccc-dddd-eeee.jsonl",
                ),
                content: format!(
                    "{}\n{}\n",
                    json!({ "type": "session_meta", "payload": { "cwd": "/tmp/project" } }),
                    json!({ "type": "event_msg", "payload": { "type": "task_started" } })
                ),
            },
        ];

        let snapshot = collect_status_snapshot(
            &[PaneTarget {
                pane_id: "7".into(),
                cwd: PathBuf::from("/tmp/project"),
            }],
            &files,
            &mut cache,
            SystemTime::UNIX_EPOCH + Duration::from_secs(12),
        );

        assert_eq!(
            snapshot
                .panes
                .get("7")
                .and_then(|entry| entry.thread_name.as_deref()),
            Some("Fix auth")
        );
    }

    #[test]
    fn newer_match_replaces_old_binding() {
        let mut cache = RefreshCache::default();
        cache.bindings.insert(
            "7".into(),
            CachedPaneBinding {
                pane_id: "7".into(),
                cwd: "/tmp/project".into(),
                transcript_path: "/tmp/old.jsonl".into(),
            },
        );
        let files = vec![
            transcript("/tmp/old.jsonl", "/tmp/project", AgentStatusKind::Done, 5),
            transcript(
                "/tmp/new.jsonl",
                "/tmp/project",
                AgentStatusKind::Running,
                9,
            ),
        ];

        let snapshot = collect_status_snapshot(
            &[PaneTarget {
                pane_id: "7".into(),
                cwd: PathBuf::from("/tmp/project"),
            }],
            &files,
            &mut cache,
            SystemTime::UNIX_EPOCH + Duration::from_secs(9),
        );

        assert_eq!(
            snapshot.panes.get("7").map(|entry| entry.updated_at),
            Some(9)
        );
        assert_eq!(
            cache
                .bindings
                .get("7")
                .map(|binding| binding.transcript_path.as_str()),
            Some("/tmp/new.jsonl")
        );
    }

    #[test]
    fn custom_tool_calls_count_as_tool_entries() {
        let entry = json!({
            "type": "response_item",
            "payload": { "type": "custom_tool_call" }
        });
        assert!(is_tool_call_entry(&entry));
    }

    #[test]
    fn normalize_path_stays_lexical() {
        assert_eq!(
            normalize_path(Path::new("/tmp/project/../other")),
            PathBuf::from("/tmp/other")
        );
    }

    fn transcript(path: &str, cwd: &str, status: AgentStatusKind, modified_at: u64) -> CodexFile {
        let status_line = match status {
            AgentStatusKind::Running => {
                json!({ "type": "event_msg", "payload": { "type": "task_started" } })
            }
            AgentStatusKind::Done => {
                json!({ "type": "event_msg", "payload": { "type": "task_complete" } })
            }
            AgentStatusKind::Waiting => {
                json!({ "type": "response_item", "payload": { "type": "function_call" } })
            }
            AgentStatusKind::Idle => json!({}),
        };

        CodexFile {
            kind: CodexFileKind::Transcript,
            modified_at,
            path: PathBuf::from(path),
            content: format!(
                "{}\n{}\n",
                json!({ "type": "session_meta", "payload": { "cwd": cwd } }),
                status_line
            ),
        }
    }

    fn thread(path: &str, cwd: &str, status: AgentStatusKind, modified_at: u64) -> ThreadSummary {
        summarize_thread(
            &transcript(path, cwd, status, modified_at),
            &Default::default(),
            SystemTime::UNIX_EPOCH,
        )
        .unwrap()
    }
}
