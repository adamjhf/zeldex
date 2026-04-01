use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde_json::Value;

use crate::codex_cache::{load_codex_cache, save_codex_cache, CachedPaneBinding, CachedThreadRecord, CodexCache};
use crate::status::AgentStatusKind;
use crate::status_file::{PaneStatusEntry, StatusSnapshot};

const BINDING_TTL: Duration = Duration::from_secs(12 * 60 * 60);
const RECENT_DIR_WINDOW: Duration = Duration::from_secs(3 * 24 * 60 * 60);
const WAIT_AFTER_TOOL_CALL: Duration = Duration::from_secs(3);
const THREAD_NAME_MAX: usize = 80;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaneTarget {
    pub pane_id: String,
    pub pid: u32,
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

#[derive(Clone, Debug)]
struct PaneContext {
    pane_id: String,
    pid: u32,
    cwd: PathBuf,
    codex_pids: Vec<u32>,
    binding: Option<CachedPaneBinding>,
}

#[derive(Clone, Debug)]
struct PaneProbe {
    pane_id: String,
    pid: u32,
    cwd: PathBuf,
    codex_pids: Vec<u32>,
    binding: Option<CachedPaneBinding>,
}

#[derive(Clone, Debug)]
struct ProcessEntry {
    ppid: u32,
    comm: String,
    args: String,
}

pub fn collect_status_snapshot(panes: &[PaneTarget]) -> Result<StatusSnapshot> {
    let now = SystemTime::now();
    let updated_at = unix_seconds(now);
    let codex_home = codex_home();
    let sessions_dir = codex_home.join("sessions");
    let thread_names = load_thread_index(&codex_home);
    let mut cache = load_codex_cache();
    let processes = load_process_snapshot().unwrap_or_default();
    let pane_probes = panes
        .iter()
        .filter_map(|pane| probe_pane(pane, &processes, &cache))
        .collect::<Vec<_>>();
    let pane_contexts = pane_probes
        .iter()
        .filter_map(build_pane_context)
        .collect::<Vec<_>>();

    let mut snapshot = StatusSnapshot {
        panes: BTreeMap::new(),
        updated_at,
    };

    if pane_contexts.is_empty() {
        prune_cache(&mut cache, &pane_probes, updated_at);
        let _ = save_codex_cache(&cache);
        return Ok(snapshot);
    }

    refresh_discovered_threads(&mut cache, &sessions_dir, &thread_names, now)?;

    for pane in &pane_contexts {
        let exact_paths = open_transcript_paths_for_pids(&pane.codex_pids, &sessions_dir).unwrap_or_default();
        let resolved = resolve_pane_thread(&mut cache, pane, &exact_paths, &thread_names, now)?;

        if let Some(thread) = resolved {
            snapshot.panes.insert(
                pane.pane_id.clone(),
                PaneStatusEntry {
                    pane_id: pane.pane_id.clone(),
                    pid: pane.pid,
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
                    pid: pane.pid,
                    cwd: pane.cwd.to_string_lossy().into_owned(),
                    transcript_path: thread.transcript_path.to_string_lossy().into_owned(),
                    last_seen_at: updated_at,
                },
            );
        } else {
            cache.bindings.remove(&pane.pane_id);
        }
    }

    prune_cache(&mut cache, &pane_probes, updated_at);
    let _ = save_codex_cache(&cache);
    Ok(snapshot)
}

fn probe_pane(
    pane: &PaneTarget,
    processes: &HashMap<u32, ProcessEntry>,
    cache: &CodexCache,
) -> Option<PaneProbe> {
    let cwd = cwd_for_pid(pane.pid).ok()?;
    let binding = cache
        .bindings
        .get(&pane.pane_id)
        .cloned()
        .filter(|binding| binding.pid == pane.pid);
    let codex_pids = codex_descendant_pids(processes, pane.pid);

    Some(PaneProbe {
        pane_id: pane.pane_id.clone(),
        pid: pane.pid,
        cwd,
        codex_pids,
        binding,
    })
}

fn build_pane_context(pane: &PaneProbe) -> Option<PaneContext> {
    let binding = pane.binding.clone().filter(|binding| binding_matches_pane(binding, pane));
    if pane.codex_pids.is_empty() && binding.is_none() {
        return None;
    }

    Some(PaneContext {
        pane_id: pane.pane_id.clone(),
        pid: pane.pid,
        cwd: pane.cwd.clone(),
        codex_pids: pane.codex_pids.clone(),
        binding,
    })
}

fn resolve_pane_thread(
    cache: &mut CodexCache,
    pane: &PaneContext,
    exact_paths: &[PathBuf],
    thread_names: &HashMap<String, String>,
    now: SystemTime,
) -> Result<Option<ThreadSummary>> {
    let mut candidates = Vec::new();
    let mut seen_paths = HashSet::new();

    for path in exact_paths {
        let normalized = normalize_path(path);
        if seen_paths.insert(normalized.clone()) {
            if let Some(thread) = refresh_thread(cache, &normalized, thread_names, now)? {
                candidates.push(thread);
            }
        }
    }

    if let Some(binding) = &pane.binding {
        let path = normalize_path(Path::new(&binding.transcript_path));
        if seen_paths.insert(path.clone()) {
            if let Some(thread) = refresh_thread(cache, &path, thread_names, now)? {
                candidates.push(thread);
            }
        }
    }

    if let Some(best_exact) = candidates
        .into_iter()
        .max_by_key(|thread| (thread.updated_at, thread.status.priority()))
    {
        return Ok(Some(best_exact));
    }

    if pane.codex_pids.is_empty() && pane.binding.is_none() {
        return Ok(None);
    }

    Ok(select_best_cached_thread(cache, &pane.cwd))
}

fn prune_cache(cache: &mut CodexCache, panes: &[PaneProbe], now: u64) {
    let visible = panes
        .iter()
        .map(|pane| (pane.pane_id.as_str(), pane))
        .collect::<HashMap<_, _>>();
    cache.bindings.retain(|pane_id, binding| {
        if let Some(pane) = visible.get(pane_id.as_str()) {
            return binding_matches_pane(binding, pane);
        }
        now.saturating_sub(binding.last_seen_at) <= BINDING_TTL.as_secs()
    });
}

fn binding_matches_pane(binding: &CachedPaneBinding, pane: &PaneProbe) -> bool {
    binding.pid == pane.pid && normalize_path(Path::new(&binding.cwd)) == pane.cwd
}

fn refresh_discovered_threads(
    cache: &mut CodexCache,
    sessions_dir: &Path,
    thread_names: &HashMap<String, String>,
    now: SystemTime,
) -> Result<()> {
    let files = if cache.threads.is_empty() {
        collect_all_session_files(sessions_dir)?
    } else {
        collect_recent_session_files(sessions_dir, now)?
    };

    for path in files {
        let _ = refresh_thread(cache, &path, thread_names, now)?;
    }
    Ok(())
}

fn refresh_thread(
    cache: &mut CodexCache,
    path: &Path,
    thread_names: &HashMap<String, String>,
    now: SystemTime,
) -> Result<Option<ThreadSummary>> {
    let normalized = normalize_path(path);
    let key = normalized.to_string_lossy().into_owned();
    let metadata = match fs::metadata(&normalized) {
        Ok(metadata) => metadata,
        Err(_) => {
            cache.threads.remove(&key);
            return Ok(None);
        }
    };

    let modified_at = unix_seconds(metadata.modified().unwrap_or(UNIX_EPOCH));
    let file_size = metadata.len();

    if let Some(record) = cache.threads.get_mut(&key) {
        if record.file_size == file_size && record.modified_at == modified_at {
            if let Some(name) = thread_names.get(&record.thread_id) {
                record.thread_name = Some(name.clone());
            }
            return Ok(Some(thread_from_record(record)));
        }
    }

    let summary = summarize_thread(&normalized, thread_names, now)?;
    match summary {
        Some(summary) => {
            cache.threads.insert(key, record_from_thread(&summary, file_size, modified_at));
            Ok(Some(summary))
        }
        None => {
            cache.threads.remove(&normalized.to_string_lossy().into_owned());
            Ok(None)
        }
    }
}

fn collect_all_session_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    collect_session_files_recursive(dir, &mut files)?;
    Ok(files)
}

fn collect_session_files_recursive(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Ok(());
    };

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            collect_session_files_recursive(&path, out)?;
            continue;
        }
        if file_type.is_file() && path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
            out.push(normalize_path(&path));
        }
    }

    Ok(())
}

fn collect_recent_session_files(dir: &Path, now: SystemTime) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    collect_recent_day_files(dir, 0, now, &mut files)?;
    Ok(files)
}

fn collect_recent_day_files(dir: &Path, depth: usize, now: SystemTime, out: &mut Vec<PathBuf>) -> Result<()> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Ok(());
    };

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }

        if depth == 2 {
            let modified = entry.metadata()?.modified().unwrap_or(UNIX_EPOCH);
            if now.duration_since(modified).unwrap_or_default() > RECENT_DIR_WINDOW {
                continue;
            }
            let Ok(day_entries) = fs::read_dir(&path) else {
                continue;
            };
            for day_entry in day_entries {
                let day_entry = day_entry?;
                let day_path = day_entry.path();
                let Ok(day_type) = day_entry.file_type() else {
                    continue;
                };
                if day_type.is_file()
                    && day_path.extension().and_then(|ext| ext.to_str()) == Some("jsonl")
                {
                    out.push(normalize_path(&day_path));
                }
            }
            continue;
        }

        collect_recent_day_files(&path, depth + 1, now, out)?;
    }

    Ok(())
}

fn codex_descendant_pids(processes: &HashMap<u32, ProcessEntry>, root_pid: u32) -> Vec<u32> {
    let mut stack = vec![root_pid];
    let mut descendants = Vec::new();

    while let Some(pid) = stack.pop() {
        descendants.push(pid);
        for (child_pid, process) in processes {
            if process.ppid == pid {
                stack.push(*child_pid);
            }
        }
    }

    descendants
        .into_iter()
        .filter(|pid| processes.get(pid).map(is_codex_process).unwrap_or(false))
        .collect()
}

fn load_process_snapshot() -> Result<HashMap<u32, ProcessEntry>> {
    let output = Command::new("ps")
        .args(["-axo", "pid=,ppid=,comm=,args="])
        .output()
        .context("run ps for process snapshot")?;
    if !output.status.success() {
        anyhow::bail!("ps failed");
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut processes = HashMap::new();

    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut parts = trimmed.split_whitespace();
        let Some(pid) = parts.next().and_then(|value| value.parse::<u32>().ok()) else {
            continue;
        };
        let Some(ppid) = parts.next().and_then(|value| value.parse::<u32>().ok()) else {
            continue;
        };
        let Some(comm) = parts.next() else {
            continue;
        };
        let args = trimmed
            .splitn(4, char::is_whitespace)
            .nth(3)
            .unwrap_or(comm)
            .to_owned();
        processes.insert(
            pid,
            ProcessEntry {
                ppid,
                comm: comm.to_owned(),
                args,
            },
        );
    }

    Ok(processes)
}

fn is_codex_process(process: &ProcessEntry) -> bool {
    let comm = Path::new(&process.comm)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(process.comm.as_str());
    if comm == "codex" {
        return true;
    }

    process
        .args
        .split_whitespace()
        .next()
        .map(|value| {
            Path::new(value)
                .file_name()
                .and_then(|name| name.to_str())
                .map(|name| name == "codex")
                .unwrap_or(false)
        })
        .unwrap_or(false)
}

fn open_transcript_paths_for_pids(pids: &[u32], sessions_dir: &Path) -> Result<Vec<PathBuf>> {
    if pids.is_empty() {
        return Ok(Vec::new());
    }

    let mut args = vec!["-Fn".to_owned()];
    for pid in pids {
        args.push("-p".to_owned());
        args.push(pid.to_string());
    }

    let output = Command::new("lsof")
        .args(args.iter().map(String::as_str))
        .output()
        .context("run lsof for codex descendants")?;
    if !output.status.success() {
        return Ok(Vec::new());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut paths = Vec::new();
    for line in stdout.lines() {
        let Some(path) = line.strip_prefix('n') else {
            continue;
        };
        if !path.ends_with(".jsonl") {
            continue;
        }
        let candidate = normalize_path(Path::new(path));
        if candidate.starts_with(sessions_dir) {
            paths.push(candidate);
        }
    }
    Ok(paths)
}

fn select_best_cached_thread(cache: &CodexCache, pane_cwd: &Path) -> Option<ThreadSummary> {
    let pane_cwd = normalize_path(pane_cwd);
    cache.threads
        .values()
        .filter_map(|record| {
            let thread = thread_from_record(record);
            match_score(&pane_cwd, &thread.project_dir).map(|score| {
                (
                    score,
                    depth(&thread.project_dir),
                    thread.updated_at,
                    thread,
                )
            })
        })
        .max_by_key(|(score, depth, updated_at, _)| (*score, *depth, *updated_at))
        .map(|(_, _, _, thread)| thread)
}

fn thread_from_record(record: &CachedThreadRecord) -> ThreadSummary {
    ThreadSummary {
        transcript_path: normalize_path(Path::new(&record.transcript_path)),
        thread_id: record.thread_id.clone(),
        thread_name: record.thread_name.clone(),
        project_dir: normalize_path(Path::new(&record.project_dir)),
        status: record.status,
        updated_at: record.updated_at,
    }
}

fn record_from_thread(summary: &ThreadSummary, file_size: u64, modified_at: u64) -> CachedThreadRecord {
    CachedThreadRecord {
        transcript_path: summary.transcript_path.to_string_lossy().into_owned(),
        file_size,
        modified_at,
        thread_id: summary.thread_id.clone(),
        thread_name: summary.thread_name.clone(),
        project_dir: summary.project_dir.to_string_lossy().into_owned(),
        status: summary.status,
        updated_at: summary.updated_at,
    }
}

fn codex_home() -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))
        .unwrap_or_else(|| PathBuf::from(".codex"))
}

fn cwd_for_pid(pid: u32) -> Result<PathBuf> {
    let output = Command::new("lsof")
        .args(["-a", "-d", "cwd", "-Fn", "-p", &pid.to_string()])
        .output()
        .with_context(|| format!("run lsof for pid {pid}"))?;
    if !output.status.success() {
        anyhow::bail!("lsof failed for pid {pid}");
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let path = stdout
        .lines()
        .find_map(|line| line.strip_prefix('n'))
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .context("missing cwd from lsof output")?;
    Ok(normalize_path(&path))
}

fn load_thread_index(codex_home: &Path) -> HashMap<String, String> {
    let path = codex_home.join("session_index.jsonl");
    let Ok(text) = fs::read_to_string(path) else {
        return HashMap::new();
    };

    let mut names = HashMap::new();
    for line in text.lines() {
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

    names
}

fn summarize_thread(
    path: &Path,
    thread_names: &HashMap<String, String>,
    now: SystemTime,
) -> Result<Option<ThreadSummary>> {
    let metadata = fs::metadata(path)?;
    let modified_at = metadata.modified().unwrap_or(UNIX_EPOCH);
    let age = now.duration_since(modified_at).unwrap_or_default();
    let text = fs::read_to_string(path)?;
    let thread_id = parse_thread_id(path);
    let mut status = AgentStatusKind::Idle;
    let mut project_dir = None;
    let mut thread_name = thread_names.get(&thread_id).cloned();
    let mut last_entry_was_tool_call = false;

    for line in text.lines() {
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

    let Some(project_dir) = project_dir else {
        return Ok(None);
    };
    if status == AgentStatusKind::Idle {
        return Ok(None);
    }
    if status == AgentStatusKind::Running && last_entry_was_tool_call && age >= WAIT_AFTER_TOOL_CALL {
        status = AgentStatusKind::Waiting;
    }

    Ok(Some(ThreadSummary {
        transcript_path: normalize_path(path),
        thread_id,
        thread_name,
        project_dir,
        status,
        updated_at: unix_seconds(modified_at),
    }))
}

fn parse_thread_id(path: &Path) -> String {
    let stem = path.file_stem().and_then(|value| value.to_str()).unwrap_or_default();
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
    entry.get("payload")?.get("cwd")?.as_str().map(ToOwned::to_owned)
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
    if matches!(entry.get("type").and_then(Value::as_str), Some("function_call")) {
        return true;
    }

    if !matches!(entry.get("type").and_then(Value::as_str), Some("response_item")) {
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

    entry.get("content")
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
    let source = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mut normalized = PathBuf::new();
    for component in source.components() {
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
    time.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

#[cfg(test)]
mod tests {
    use std::fs::{self, File};
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime};

    use serde_json::json;

    use crate::codex_cache::{CachedPaneBinding, CachedThreadRecord, CodexCache};
    use crate::status::AgentStatusKind;

    use super::{
        binding_matches_pane, determine_status, extract_thread_name, is_codex_process, is_tool_call_entry,
        load_thread_index, normalize_path, select_best_cached_thread, summarize_thread, PaneProbe,
        ProcessEntry, RECENT_DIR_WINDOW, WAIT_AFTER_TOOL_CALL,
    };

    fn temp_dir(name: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "zeldex-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        fs::create_dir_all(&base).unwrap();
        base
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
    fn prefers_exact_cwd_match_from_cache() {
        let mut cache = CodexCache::default();
        cache.threads.insert(
            "/tmp/project/transcript.jsonl".into(),
            CachedThreadRecord {
                transcript_path: "/tmp/project/transcript.jsonl".into(),
                file_size: 1,
                modified_at: 1,
                thread_id: "a".into(),
                thread_name: None,
                project_dir: "/tmp/project".into(),
                status: AgentStatusKind::Running,
                updated_at: 1,
            },
        );
        cache.threads.insert(
            "/tmp/transcript.jsonl".into(),
            CachedThreadRecord {
                transcript_path: "/tmp/transcript.jsonl".into(),
                file_size: 1,
                modified_at: 1,
                thread_id: "b".into(),
                thread_name: None,
                project_dir: "/tmp".into(),
                status: AgentStatusKind::Done,
                updated_at: 99,
            },
        );
        let best = select_best_cached_thread(&cache, Path::new("/tmp/project")).unwrap();
        assert_eq!(best.thread_id, "a");
    }

    #[test]
    fn waiting_after_tool_call_pause() {
        let root = temp_dir("waiting");
        let sessions = root.join("sessions/2026/04/01");
        fs::create_dir_all(&sessions).unwrap();
        let transcript = sessions.join("rollout-2026-04-01T10-00-00-019d3333-aaaa-bbbb-cccc-000000000001.jsonl");
        let mut file = File::create(&transcript).unwrap();
        writeln!(
            file,
            "{}",
            json!({ "type": "session_meta", "payload": { "cwd": "/tmp/project" } })
        )
        .unwrap();
        writeln!(
            file,
            "{}",
            json!({ "type": "response_item", "payload": { "type": "function_call" } })
        )
        .unwrap();
        let now = SystemTime::now() + WAIT_AFTER_TOOL_CALL + Duration::from_secs(1);
        let summary = summarize_thread(&transcript, &Default::default(), now)
            .unwrap()
            .unwrap();
        assert_eq!(summary.status, AgentStatusKind::Waiting);
    }

    #[test]
    fn waiting_after_custom_tool_call_pause() {
        let root = temp_dir("custom-waiting");
        let sessions = root.join("sessions/2026/04/01");
        fs::create_dir_all(&sessions).unwrap();
        let transcript = sessions.join("rollout-2026-04-01T10-00-00-019d3333-aaaa-bbbb-cccc-000000000111.jsonl");
        let mut file = File::create(&transcript).unwrap();
        writeln!(
            file,
            "{}",
            json!({ "type": "session_meta", "payload": { "cwd": "/tmp/project" } })
        )
        .unwrap();
        writeln!(
            file,
            "{}",
            json!({ "type": "response_item", "payload": { "type": "custom_tool_call" } })
        )
        .unwrap();
        let now = SystemTime::now() + WAIT_AFTER_TOOL_CALL + Duration::from_secs(1);
        let summary = summarize_thread(&transcript, &Default::default(), now)
            .unwrap()
            .unwrap();
        assert_eq!(summary.status, AgentStatusKind::Waiting);
    }

    #[test]
    fn old_transcript_still_summarized() {
        let root = temp_dir("old");
        let sessions = root.join("sessions/2026/03/01");
        fs::create_dir_all(&sessions).unwrap();
        let transcript = sessions.join("rollout-2026-03-01T10-00-00-019d3333-aaaa-bbbb-cccc-000000000001.jsonl");
        fs::write(
            &transcript,
            format!(
                "{}\n{}\n",
                json!({ "type": "session_meta", "payload": { "cwd": "/tmp/project" } }),
                json!({ "type": "event_msg", "payload": { "type": "task_complete" } })
            ),
        )
        .unwrap();
        let now = SystemTime::now() + RECENT_DIR_WINDOW + Duration::from_secs(60);
        let summary = summarize_thread(&transcript, &Default::default(), now)
            .unwrap()
            .unwrap();
        assert_eq!(summary.status, AgentStatusKind::Done);
    }

    #[test]
    fn thread_index_loaded() {
        let root = temp_dir("index");
        fs::write(
            root.join("session_index.jsonl"),
            "{\"id\":\"abc\",\"thread_name\":\"Fix auth\"}\n",
        )
        .unwrap();
        let names = load_thread_index(&root);
        assert_eq!(names.get("abc"), Some(&"Fix auth".to_string()));
    }

    #[test]
    fn identifies_codex_processes() {
        let process = ProcessEntry {
            ppid: 1,
            comm: "/opt/homebrew/bin/codex".into(),
            args: "/opt/homebrew/bin/codex exec".into(),
        };
        assert!(is_codex_process(&process));
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
    fn stale_binding_drops_after_pane_leaves_bound_cwd() {
        let binding = CachedPaneBinding {
            pane_id: "7".into(),
            pid: 123,
            cwd: "/tmp/project".into(),
            transcript_path: "/tmp/project/transcript.jsonl".into(),
            last_seen_at: 1,
        };
        let pane = PaneProbe {
            pane_id: "7".into(),
            pid: 123,
            cwd: normalize_path(Path::new("/tmp/elsewhere")),
            codex_pids: Vec::new(),
            binding: Some(binding.clone()),
        };
        assert!(!binding_matches_pane(&binding, &pane));
    }

    #[test]
    fn cached_binding_shape_stays_serializable() {
        let binding = CachedPaneBinding {
            pane_id: "7".into(),
            pid: 123,
            cwd: normalize_path(Path::new("/tmp/project")).to_string_lossy().into_owned(),
            transcript_path: "/tmp/project/transcript.jsonl".into(),
            last_seen_at: 1,
        };
        let json = serde_json::to_string(&binding).unwrap();
        assert!(json.contains("\"pane_id\":\"7\""));
    }
}
