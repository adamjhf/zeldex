use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::codex::{CodexFile, CodexFileKind};

const MAX_TRANSCRIPTS: usize = 12;
const MAX_INDEX_LINES: usize = 400;
const MAX_TRANSCRIPT_TAIL_LINES: usize = 200;
const RECENT_TRANSCRIPT_AGE: Duration = Duration::from_secs(3 * 24 * 60 * 60);

pub fn load_recent_codex_files(
    host_root: &Path,
    now: SystemTime,
) -> Result<Vec<CodexFile>, String> {
    let mut files = Vec::new();

    let index_path = host_root.join("session_index.jsonl");
    if index_path.is_file() {
        files.push(CodexFile {
            kind: CodexFileKind::Index,
            modified_at: modified_at_secs(&index_path)?,
            path: index_path.clone(),
            content: read_tail_lines(&index_path, MAX_INDEX_LINES)?,
        });
    }

    let sessions_dir = host_root.join("sessions");
    if sessions_dir.is_dir() {
        let mut transcripts = collect_recent_transcripts(&sessions_dir, now)?;
        transcripts.sort_by(|left, right| {
            right
                .modified_at
                .cmp(&left.modified_at)
                .then_with(|| left.path.cmp(&right.path))
        });
        transcripts.truncate(MAX_TRANSCRIPTS);
        transcripts.sort_by(|left, right| {
            left.modified_at
                .cmp(&right.modified_at)
                .then_with(|| left.path.cmp(&right.path))
        });
        files.extend(transcripts.into_iter().map(|transcript| transcript.file));
    }

    Ok(files)
}

struct TranscriptCandidate {
    modified_at: u64,
    path: PathBuf,
    file: CodexFile,
}

fn collect_recent_transcripts(
    sessions_dir: &Path,
    now: SystemTime,
) -> Result<Vec<TranscriptCandidate>, String> {
    let cutoff = now.checked_sub(RECENT_TRANSCRIPT_AGE).unwrap_or(UNIX_EPOCH);
    let mut stack = vec![sessions_dir.to_path_buf()];
    let mut transcripts = Vec::new();

    while let Some(dir) = stack.pop() {
        let entries =
            fs::read_dir(&dir).map_err(|err| format!("failed to read {}: {err}", dir.display()))?;
        for entry in entries {
            let entry =
                entry.map_err(|err| format!("failed to read entry in {}: {err}", dir.display()))?;
            let file_type = entry
                .file_type()
                .map_err(|err| format!("failed to inspect {}: {err}", entry.path().display()))?;
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                stack.push(entry.path());
                continue;
            }
            if !file_type.is_file() {
                continue;
            }

            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }

            let modified = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .map_err(|err| format!("failed to stat {}: {err}", path.display()))?;
            if modified < cutoff {
                continue;
            }

            let modified_at = unix_seconds(modified);
            transcripts.push(TranscriptCandidate {
                modified_at,
                path: path.clone(),
                file: CodexFile {
                    kind: CodexFileKind::Transcript,
                    modified_at,
                    path: path.clone(),
                    content: read_transcript_summary(&path)?,
                },
            });
        }
    }

    Ok(transcripts)
}

fn read_tail_lines(path: &Path, max_lines: usize) -> Result<String, String> {
    let file =
        File::open(path).map_err(|err| format!("failed to open {}: {err}", path.display()))?;
    let reader = BufReader::new(file);
    let lines = tail_lines(reader, max_lines, false)?;
    Ok(join_lines(lines))
}

fn read_transcript_summary(path: &Path) -> Result<String, String> {
    let file =
        File::open(path).map_err(|err| format!("failed to open {}: {err}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut first_line = String::new();
    reader
        .read_line(&mut first_line)
        .map_err(|err| format!("failed to read {}: {err}", path.display()))?;

    let mut summary = String::new();
    if let Some(cwd) = extract_cwd(first_line.trim_end()) {
        summary.push_str(
            &json!({
                "type": "session_meta",
                "payload": { "cwd": cwd }
            })
            .to_string(),
        );
        summary.push('\n');
    }

    let lines = tail_lines(reader, MAX_TRANSCRIPT_TAIL_LINES, true)?;
    summary.push_str(&join_lines(lines));
    Ok(summary)
}

fn tail_lines<R: BufRead>(
    reader: R,
    max_lines: usize,
    skip_empty: bool,
) -> Result<Vec<String>, String> {
    let mut tail = VecDeque::with_capacity(max_lines);
    for line in reader.lines() {
        let line = line.map_err(|err| format!("failed to read line: {err}"))?;
        if skip_empty && line.is_empty() {
            continue;
        }
        if tail.len() == max_lines {
            tail.pop_front();
        }
        tail.push_back(line);
    }
    Ok(tail.into_iter().collect())
}

fn join_lines(lines: Vec<String>) -> String {
    if lines.is_empty() {
        String::new()
    } else {
        format!("{}\n", lines.join("\n"))
    }
}

fn extract_cwd(line: &str) -> Option<String> {
    let entry = serde_json::from_str::<Value>(line).ok()?;
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

fn modified_at_secs(path: &Path) -> Result<u64, String> {
    let modified = fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .map_err(|err| format!("failed to stat {}: {err}", path.display()))?;
    Ok(unix_seconds(modified))
}

fn unix_seconds(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use super::load_recent_codex_files;
    use crate::codex::CodexFileKind;

    #[test]
    fn loads_bounded_index_and_transcript_content() {
        let root = temp_root("bounded");
        let sessions = root.join("sessions/2026/04/15");
        fs::create_dir_all(&sessions).unwrap();
        fs::write(
            root.join("session_index.jsonl"),
            (0..500)
                .map(|i| format!("{{\"id\":\"{i}\",\"thread_name\":\"n{i}\"}}"))
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        fs::write(
            sessions.join("rollout-2026-04-15T00-00-00-aaaa-bbbb-cccc-dddd-eeee.jsonl"),
            [
                "{\"type\":\"session_meta\",\"payload\":{\"cwd\":\"/tmp/project\"}}".to_owned(),
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\"}}".to_owned(),
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\"}}".to_owned(),
            ]
            .join("\n"),
        )
        .unwrap();

        let files = load_recent_codex_files(&root, SystemTime::now()).unwrap();

        assert_eq!(files.len(), 2);
        assert_eq!(files[0].kind, CodexFileKind::Index);
        assert_eq!(files[0].content.lines().count(), 400);
        assert_eq!(files[1].kind, CodexFileKind::Transcript);
        assert_eq!(
            files[1].content.lines().next(),
            Some("{\"payload\":{\"cwd\":\"/tmp/project\"},\"type\":\"session_meta\"}")
        );
        assert_eq!(files[1].content.lines().count(), 3);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn skips_old_transcripts() {
        let root = temp_root("old");
        let sessions = root.join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        fs::write(
            sessions.join("rollout-2026-04-01T00-00-00-aaaa-bbbb-cccc-dddd-eeee.jsonl"),
            "{\"type\":\"session_meta\",\"payload\":{\"cwd\":\"/tmp/project\"}}\n",
        )
        .unwrap();

        let files = load_recent_codex_files(
            &root,
            SystemTime::now() + Duration::from_secs(10 * 24 * 60 * 60),
        )
        .unwrap();

        assert!(files.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    fn temp_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "zeldex-codex-fs-{name}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }
}
