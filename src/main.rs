use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use zeldex::codex::{collect_status_snapshot, PaneTarget, RefreshCache};
use zeldex::codex_fs::load_recent_codex_files;
use zeldex::render::{render_notice, render_sidebar, TabLine};
use zeldex::status::AgentStatusKind;
use zeldex::status_file::StatusSnapshot;
use zellij_tile::prelude::*;

const DEFAULT_POLL_SECS: f64 = 1.2;
const INITIAL_POLL_SECS: f64 = 0.1;
const HOST_CODEX_ROOT: &str = "/host";

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
    snapshot: StatusSnapshot,
    refresh_cache: RefreshCache,
    permission_state: PermissionState,
    codex_home: Option<PathBuf>,
    status_error: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum PermissionState {
    #[default]
    Pending,
    Granted,
    Denied,
}

register_plugin!(State);

impl ZellijPlugin for State {
    fn load(&mut self, configuration: BTreeMap<String, String>) {
        set_selectable(true);
        self.poll_secs = configuration
            .get("poll_secs")
            .and_then(|value| value.parse::<f64>().ok())
            .filter(|value| *value > 0.0)
            .unwrap_or(DEFAULT_POLL_SECS);
        self.codex_home = configuration
            .get("codex_home")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        if self.codex_home.is_none() {
            self.status_error = Some("missing codex_home config".to_owned());
        }

        subscribe(&[
            EventType::ModeUpdate,
            EventType::TabUpdate,
            EventType::PaneUpdate,
            EventType::Mouse,
            EventType::Timer,
            EventType::PermissionRequestResult,
            EventType::FailedToChangeHostFolder,
        ]);
        request_permission(&[
            PermissionType::ReadApplicationState,
            PermissionType::ChangeApplicationState,
            PermissionType::FullHdAccess,
        ]);
        set_timeout(INITIAL_POLL_SECS);
    }

    fn update(&mut self, event: Event) -> bool {
        match event {
            Event::ModeUpdate(mode_info) => {
                self.mode_info = mode_info;
                true
            }
            Event::PermissionRequestResult(PermissionStatus::Granted) => {
                self.permission_state = PermissionState::Granted;
                set_selectable(false);
                self.configure_codex_host();
                let changed = self.refresh_from_host();
                set_timeout(self.poll_secs);
                changed || self.should_render_notice()
            }
            Event::PermissionRequestResult(PermissionStatus::Denied) => {
                self.permission_state = PermissionState::Denied;
                set_selectable(true);
                set_timeout(self.poll_secs);
                true
            }
            Event::FailedToChangeHostFolder(error) => {
                self.status_error =
                    Some(error.unwrap_or_else(|| "failed to point /host at codex home".to_owned()));
                true
            }
            Event::TabUpdate(tabs) => {
                self.tabs = tabs;
                self.clear_active_unread();
                true
            }
            Event::PaneUpdate(pane_manifest) => {
                self.panes_by_tab = pane_manifest.panes.into_iter().collect();
                let changed = self.refresh_from_host();
                self.reconcile_tracked_panes();
                changed
            }
            Event::Mouse(mouse) => self.handle_mouse(mouse),
            Event::Timer(_) => {
                let changed = self.refresh_from_host();
                set_timeout(self.poll_secs);
                changed || self.should_render_notice()
            }
            _ => false,
        }
    }

    fn render(&mut self, rows: usize, cols: usize) {
        if rows == 0 || cols == 0 {
            return;
        }
        if self.should_render_notice() || self.tabs.is_empty() {
            self.clickable_rows = render_notice(rows, cols, &self.mode_info, &self.notice_lines());
            return;
        }

        let tab_lines = self
            .tabs
            .iter()
            .map(|tab| TabLine {
                position: tab.position,
                name: self.tab_name(tab),
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

    fn configure_codex_host(&mut self) {
        let Some(codex_home) = self.codex_home.clone() else {
            return;
        };
        self.status_error = None;
        change_host_folder(codex_home);
    }

    fn refresh_from_host(&mut self) -> bool {
        if self.permission_state != PermissionState::Granted {
            return false;
        }
        let targets = self.visible_pane_targets();
        if targets.is_empty() {
            self.snapshot = StatusSnapshot::default();
            self.refresh_cache.bindings.clear();
            self.tracked_panes.clear();
            return false;
        }

        match load_recent_codex_files(Path::new(HOST_CODEX_ROOT), std::time::SystemTime::now()) {
            Ok(files) => {
                self.status_error = None;
                let snapshot = collect_status_snapshot(
                    &targets,
                    &files,
                    &mut self.refresh_cache,
                    std::time::SystemTime::now(),
                );
                self.apply_snapshot(snapshot)
            }
            Err(error) => {
                let changed = self.status_error.as_deref() != Some(error.as_str());
                self.status_error = Some(error);
                changed
            }
        }
    }

    fn notice_lines(&self) -> Vec<String> {
        if let Some(error) = &self.status_error {
            return vec!["codex read failed".to_owned(), error.clone()];
        }

        match self.permission_state {
            PermissionState::Pending => vec![
                "waiting for plugin permissions".to_owned(),
                "needs app-state + full disk access".to_owned(),
            ],
            PermissionState::Denied => vec![
                "plugin permissions denied".to_owned(),
                "grant access, then reopen zellij".to_owned(),
            ],
            PermissionState::Granted => vec!["waiting for tab data".to_owned()],
        }
    }

    fn should_render_notice(&self) -> bool {
        self.permission_state != PermissionState::Granted || self.status_error.is_some()
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

    fn visible_pane_targets(&self) -> Vec<PaneTarget> {
        let mut pane_targets = Vec::new();
        for panes in self.panes_by_tab.values() {
            for pane in panes {
                if pane.is_plugin {
                    continue;
                }
                let pane_id = PaneId::Terminal(pane.id);
                let Ok(cwd) = get_pane_cwd(pane_id) else {
                    continue;
                };
                pane_targets.push(PaneTarget {
                    pane_id: pane.id.to_string(),
                    cwd: normalize_cwd(cwd),
                });
            }
        }
        pane_targets
    }

    fn clear_active_unread(&mut self) {
        if let Some(active_tab) = self.active_tab_position() {
            self.unread_tabs.remove(&active_tab);
        }
    }

    fn tab_name(&self, tab: &TabInfo) -> String {
        self.panes_by_tab
            .get(&tab.position)
            .and_then(|panes| {
                panes
                    .iter()
                    .filter(|pane| !pane.is_plugin)
                    .find(|pane| pane.is_focused)
                    .or_else(|| panes.iter().find(|pane| !pane.is_plugin))
            })
            .and_then(|pane| get_pane_cwd(PaneId::Terminal(pane.id)).ok())
            .and_then(|cwd| folder_name(&normalize_cwd(cwd)))
            .unwrap_or_else(|| {
                if tab.name.is_empty() {
                    format!("tab-{}", tab.position + 1)
                } else {
                    tab.name.clone()
                }
            })
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

fn normalize_cwd(path: PathBuf) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn folder_name(path: &Path) -> Option<String> {
    path.file_name()
        .or_else(|| {
            path.components().next_back().and_then(|component| {
                let text = component.as_os_str().to_string_lossy();
                if text.is_empty() || text == std::path::MAIN_SEPARATOR_STR {
                    None
                } else {
                    Some(component.as_os_str())
                }
            })
        })
        .map(|name| name.to_string_lossy().into_owned())
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
    use super::{folder_name, tracked_pane_set_changed, TrackedPane};
    use std::collections::HashMap;
    use std::path::Path;
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

    #[test]
    fn folder_name_uses_final_path_component() {
        assert_eq!(
            folder_name(Path::new("/Users/adam/Projects/dotnix")),
            Some("dotnix".into())
        );
        assert_eq!(folder_name(Path::new("/")), None);
    }
}
