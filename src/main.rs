use std::collections::{BTreeMap, BTreeSet, HashMap};
use zeldex::render::{render_sidebar, TabLine};
use zeldex::status::AgentStatusKind;
use zeldex::status_file::StatusSnapshot;
use zellij_tile::prelude::*;

const DEFAULT_POLL_SECS: f64 = 1.2;
const DEFAULT_STATUS_CMD: &str = "zeldex-status";

#[derive(Clone, Debug)]
struct TrackedPane {
    tab_position: usize,
    status: AgentStatusKind,
}

#[derive(Default)]
struct State {
    tabs: Vec<TabInfo>,
    panes_by_tab: BTreeMap<usize, Vec<PaneInfo>>,
    tracked_panes: HashMap<PaneId, TrackedPane>,
    unread_tabs: BTreeSet<usize>,
    clickable_rows: Vec<Option<usize>>,
    mode_info: ModeInfo,
    poll_secs: f64,
    status_cmd: String,
    snapshot: StatusSnapshot,
    refresh_nonce: u64,
    refresh_in_flight: bool,
    permissions_granted: bool,
}

register_plugin!(State);

impl ZellijPlugin for State {
    fn load(&mut self, configuration: BTreeMap<String, String>) {
        set_selectable(false);
        request_permission(&[
            PermissionType::ReadApplicationState,
            PermissionType::ChangeApplicationState,
            PermissionType::RunCommands,
        ]);
        subscribe(&[
            EventType::ModeUpdate,
            EventType::TabUpdate,
            EventType::PaneUpdate,
            EventType::Mouse,
            EventType::Timer,
            EventType::RunCommandResult,
            EventType::PermissionRequestResult,
        ]);

        self.poll_secs = configuration
            .get("poll_secs")
            .and_then(|value| value.parse::<f64>().ok())
            .filter(|value| *value > 0.0)
            .unwrap_or(DEFAULT_POLL_SECS);
        self.status_cmd = configuration
            .get("status_cmd")
            .cloned()
            .unwrap_or_else(|| DEFAULT_STATUS_CMD.to_owned());
    }

    fn update(&mut self, event: Event) -> bool {
        match event {
            Event::ModeUpdate(mode_info) => {
                self.mode_info = mode_info;
                true
            }
            Event::PermissionRequestResult(PermissionStatus::Granted) => {
                self.permissions_granted = true;
                self.start_status_refresh();
                set_timeout(self.poll_secs);
                true
            }
            Event::PermissionRequestResult(PermissionStatus::Denied) => {
                self.permissions_granted = false;
                set_timeout(self.poll_secs);
                true
            }
            Event::TabUpdate(tabs) => {
                self.tabs = tabs;
                self.clear_active_unread();
                true
            }
            Event::PaneUpdate(pane_manifest) => {
                self.panes_by_tab = pane_manifest.panes.into_iter().collect();
                self.reconcile_tracked_panes();
                self.start_status_refresh();
                true
            }
            Event::RunCommandResult(exit_code, stdout, _stderr, context) => {
                self.handle_status_refresh(exit_code, stdout, context)
            }
            Event::Mouse(mouse) => self.handle_mouse(mouse),
            Event::Timer(_) => {
                self.start_status_refresh();
                set_timeout(self.poll_secs);
                false
            }
            _ => false,
        }
    }

    fn render(&mut self, rows: usize, cols: usize) {
        if rows == 0 || cols == 0 || self.tabs.is_empty() {
            return;
        }

        let tab_lines = self
            .tabs
            .iter()
            .map(|tab| TabLine {
                position: tab.position,
                name: if tab.name.is_empty() {
                    format!("tab-{}", tab.position + 1)
                } else {
                    tab.name.clone()
                },
                active: tab.active,
                unread: self.unread_tabs.contains(&tab.position),
                tracked_agents: self.agent_count_for_tab(tab.position),
                status: self.status_for_tab(tab.position),
            })
            .collect::<Vec<_>>();

        self.clickable_rows = render_sidebar(rows, cols, &self.mode_info, &tab_lines);
    }
}

impl State {
    fn handle_mouse(&mut self, mouse: Mouse) -> bool {
        match mouse {
            Mouse::LeftClick(row, _) => {
                if let Some(Some(tab_position)) = usize::try_from(row)
                    .ok()
                    .and_then(|row| self.clickable_rows.get(row))
                {
                    switch_tab_to((tab_position + 1) as u32);
                }
                false
            }
            Mouse::ScrollUp(_) => {
                if let Some(active) = self.active_tab_position() {
                    let next = (active + 2).min(self.tabs.len());
                    switch_tab_to(next as u32);
                }
                false
            }
            Mouse::ScrollDown(_) => {
                if let Some(active) = self.active_tab_position() {
                    let prev = (active + 1).saturating_sub(1).max(1);
                    switch_tab_to(prev as u32);
                }
                false
            }
            _ => false,
        }
    }

    fn apply_snapshot(&mut self, snapshot: StatusSnapshot) -> bool {
        let previous = self
            .tracked_panes
            .iter()
            .map(|(pane_id, tracked)| (*pane_id, tracked.status))
            .collect::<HashMap<_, _>>();

        self.snapshot = snapshot;
        self.reconcile_tracked_panes();

        let active_tab = self.active_tab_position();
        let mut changed = tracked_pane_set_changed(&previous, &self.tracked_panes);
        for (pane_id, tracked) in &self.tracked_panes {
            let previous_status = previous
                .get(pane_id)
                .copied()
                .unwrap_or(AgentStatusKind::Idle);
            if previous_status != tracked.status {
                if Some(tracked.tab_position) != active_tab && tracked.status.is_attention() {
                    self.unread_tabs.insert(tracked.tab_position);
                }
                changed = true;
            }
        }

        self.clear_active_unread();
        changed
    }

    fn start_status_refresh(&mut self) {
        if !self.permissions_granted || self.refresh_in_flight {
            return;
        }

        let mut args = vec![self.status_cmd.clone()];
        for (pane_id, pid) in self.visible_pane_pids() {
            args.push("--pane".to_owned());
            args.push(format!("{pane_id}:{pid}"));
        }

        self.refresh_nonce += 1;
        self.refresh_in_flight = true;

        let mut context = BTreeMap::new();
        context.insert("kind".to_owned(), "status-refresh".to_owned());
        context.insert("nonce".to_owned(), self.refresh_nonce.to_string());

        let refs = args.iter().map(String::as_str).collect::<Vec<_>>();
        run_command(&refs, context);
    }

    fn handle_status_refresh(
        &mut self,
        exit_code: Option<i32>,
        stdout: Vec<u8>,
        context: BTreeMap<String, String>,
    ) -> bool {
        if context.get("kind").map(String::as_str) != Some("status-refresh") {
            return false;
        }
        let nonce = context
            .get("nonce")
            .and_then(|nonce| nonce.parse::<u64>().ok());
        if nonce != Some(self.refresh_nonce) {
            return false;
        }

        self.refresh_in_flight = false;
        if exit_code != Some(0) {
            return false;
        }

        serde_json::from_slice::<StatusSnapshot>(&stdout)
            .ok()
            .map(|snapshot| self.apply_snapshot(snapshot))
            .unwrap_or(false)
    }

    fn reconcile_tracked_panes(&mut self) {
        let mut next = HashMap::new();

        for (tab_position, panes) in &self.panes_by_tab {
            for pane in panes {
                if pane.is_plugin {
                    continue;
                }
                let key = pane.id.to_string();
                let Some(entry) = self.snapshot.panes.get(&key) else {
                    continue;
                };
                let pane_id = PaneId::Terminal(pane.id);
                next.insert(
                    pane_id,
                    TrackedPane {
                        tab_position: *tab_position,
                        status: entry.status,
                    },
                );
            }
        }

        self.tracked_panes = next;
        self.clear_active_unread();
    }

    fn visible_pane_pids(&self) -> Vec<(u32, u32)> {
        let mut pane_pids = Vec::new();
        for panes in self.panes_by_tab.values() {
            for pane in panes {
                if pane.is_plugin {
                    continue;
                }
                let pane_id = PaneId::Terminal(pane.id);
                let Ok(pid) = get_pane_pid(pane_id) else {
                    continue;
                };
                if pid > 0 {
                    pane_pids.push((pane.id, pid as u32));
                }
            }
        }
        pane_pids
    }

    fn clear_active_unread(&mut self) {
        if let Some(active_tab) = self.active_tab_position() {
            self.unread_tabs.remove(&active_tab);
        }
    }

    fn active_tab_position(&self) -> Option<usize> {
        self.tabs
            .iter()
            .find(|tab| tab.active)
            .map(|tab| tab.position)
    }

    fn agent_count_for_tab(&self, tab_position: usize) -> usize {
        self.tracked_panes
            .values()
            .filter(|tracked| tracked.tab_position == tab_position)
            .count()
    }

    fn status_for_tab(&self, tab_position: usize) -> AgentStatusKind {
        self.tracked_panes
            .values()
            .filter(|tracked| tracked.tab_position == tab_position)
            .map(|tracked| tracked.status)
            .max_by_key(|status| status.priority())
            .unwrap_or(AgentStatusKind::Idle)
    }
}

fn tracked_pane_set_changed(
    previous: &HashMap<PaneId, AgentStatusKind>,
    current: &HashMap<PaneId, TrackedPane>,
) -> bool {
    previous.len() != current.len()
        || previous
            .keys()
            .any(|pane_id| !current.contains_key(pane_id))
}

#[cfg(test)]
mod tests {
    use super::{tracked_pane_set_changed, TrackedPane};
    use std::collections::HashMap;
    use zeldex::status::AgentStatusKind;
    use zellij_tile::prelude::PaneId;

    #[test]
    fn removal_counts_as_change() {
        let pane_id = PaneId::Terminal(7);
        let previous = HashMap::from([(pane_id, AgentStatusKind::Running)]);
        let current = HashMap::new();
        assert!(tracked_pane_set_changed(&previous, &current));
    }

    #[test]
    fn stable_set_is_not_change() {
        let pane_id = PaneId::Terminal(7);
        let previous = HashMap::from([(pane_id, AgentStatusKind::Running)]);
        let current = HashMap::from([(
            pane_id,
            TrackedPane {
                tab_position: 0,
                status: AgentStatusKind::Running,
            },
        )]);
        assert!(!tracked_pane_set_changed(&previous, &current));
    }
}
