use std::collections::BTreeMap;
use std::env;
use std::io::{self, Write};

use anyhow::{anyhow, Result};
use zeldex::native::{now_unix_seconds, read_runtime, AppServerClient, LiveThreadStatus};
use zeldex::status::AgentStatusKind;
use zeldex::status_file::{PaneStatusEntry, StatusSnapshot};

fn main() -> Result<()> {
    let panes = parse_panes(env::args().skip(1))?;
    let updated_at = now_unix_seconds();
    let mut snapshot = StatusSnapshot {
        panes: BTreeMap::new(),
        updated_at,
    };

    for pane in panes {
        let Ok(runtime) = read_runtime(pane.pid) else {
            continue;
        };
        let Ok(mut client) = AppServerClient::connect_to(&runtime.ws_url) else {
            continue;
        };
        let Ok(live) = client.live_thread_status() else {
            continue;
        };
        let live = live.unwrap_or(LiveThreadStatus {
            thread_id: None,
            thread_name: None,
            status: AgentStatusKind::Running,
            updated_at,
        });
        snapshot.panes.insert(
            pane.id.clone(),
            PaneStatusEntry {
                pane_id: pane.id,
                pid: pane.pid,
                status: live.status,
                updated_at: live.updated_at,
                thread_id: live.thread_id,
                thread_name: live.thread_name,
            },
        );
    }

    let mut stdout = io::stdout().lock();
    serde_json::to_writer(&mut stdout, &snapshot)?;
    stdout.write_all(b"\n")?;
    Ok(())
}

fn parse_panes(args: impl IntoIterator<Item = String>) -> Result<Vec<PanePid>> {
    let mut args = args.into_iter();
    let mut panes = Vec::new();

    while let Some(arg) = args.next() {
        if arg != "--pane" {
            return Err(anyhow!("unexpected arg: {arg}"));
        }
        let value = args
            .next()
            .ok_or_else(|| anyhow!("missing value after --pane"))?;
        panes.push(parse_pane(&value)?);
    }

    Ok(panes)
}

fn parse_pane(value: &str) -> Result<PanePid> {
    let (id, pid) = value
        .split_once(':')
        .ok_or_else(|| anyhow!("invalid pane value: {value}"))?;
    Ok(PanePid {
        id: id.to_owned(),
        pid: pid.parse()?,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PanePid {
    id: String,
    pid: u32,
}

#[cfg(test)]
mod tests {
    use super::{parse_pane, parse_panes, PanePid};

    #[test]
    fn parses_repeated_pane_args() {
        let panes = parse_panes([
            "--pane".to_owned(),
            "7:123".to_owned(),
            "--pane".to_owned(),
            "9:456".to_owned(),
        ])
        .unwrap();
        assert_eq!(
            panes,
            vec![
                PanePid {
                    id: "7".to_owned(),
                    pid: 123,
                },
                PanePid {
                    id: "9".to_owned(),
                    pid: 456,
                }
            ]
        );
    }

    #[test]
    fn parses_single_pane_value() {
        let pane = parse_pane("12:345").unwrap();
        assert_eq!(
            pane,
            PanePid {
                id: "12".to_owned(),
                pid: 345,
            }
        );
    }
}
