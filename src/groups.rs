//! The container list: compose-project grouping, filtering and selection.

use std::collections::BTreeMap;

use crate::backend::is_running;
use crate::model::{Container, Model, Profile};

pub const STANDALONE: &str = "standalone";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ContainerGroup {
    pub name: String,
    pub indices: Vec<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListItem {
    pub group: String,
    pub group_header: bool,
    pub container_index: usize,
}

impl Model {
    pub fn selected_container(&self) -> Option<&Container> {
        let item = self.selected_item()?;
        if item.group_header { None } else { self.containers.get(item.container_index) }
    }

    pub fn selected_item(&self) -> Option<ListItem> {
        self.list_items().into_iter().nth(self.container_index)
    }

    pub fn selected_group_name(&self) -> String {
        match self.selected_item() {
            Some(item) if item.group_header => item.group,
            _ => String::new(),
        }
    }

    pub fn container_groups(&self) -> Vec<ContainerGroup> {
        let mut by_name: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
        for (index, c) in self.containers.iter().enumerate() {
            if !self.matches_filter(c) {
                continue;
            }
            let name = if c.compose_project.is_empty() { STANDALONE } else { &c.compose_project };
            by_name.entry(name).or_default().push(index);
        }
        let standalone = by_name.remove(STANDALONE);
        by_name
            .into_iter()
            .map(|(name, indices)| (name.to_string(), indices))
            .chain(standalone.map(|indices| (STANDALONE.to_string(), indices)))
            .map(|(name, indices)| ContainerGroup { name, indices })
            .collect()
    }

    pub fn list_items(&self) -> Vec<ListItem> {
        let mut items = Vec::with_capacity(self.containers.len());
        for group in self.container_groups() {
            let entry = |index| ListItem { group: group.name.clone(), group_header: false, container_index: index };
            if group.name == STANDALONE {
                items.extend(group.indices.iter().map(|&i| entry(i)));
                continue;
            }
            items.push(ListItem { group: group.name.clone(), group_header: true, container_index: 0 });
            if self.is_expanded(&group.name) {
                items.extend(group.indices.iter().map(|&i| entry(i)));
            }
        }
        items
    }

    pub fn is_expanded(&self, name: &str) -> bool {
        if !self.search_query.trim().is_empty() {
            return true;
        }
        self.expanded.get(name).copied().unwrap_or(true)
    }

    pub fn sync_expanded(&mut self) {
        for group in self.container_groups() {
            self.expanded.entry(group.name).or_insert(true);
        }
    }

    pub fn find_container_item(&self, id: &str) -> Option<usize> {
        if id.is_empty() {
            return None;
        }
        self.list_items().iter().position(|item| !item.group_header && self.containers[item.container_index].id == id)
    }

    pub fn find_group_item(&self, name: &str) -> Option<usize> {
        self.list_items().iter().position(|item| item.group_header && item.group == name)
    }

    pub fn first_container_item(&self) -> usize {
        self.list_items().iter().position(|item| !item.group_header).unwrap_or(0)
    }

    pub fn selected_group(&self) -> Option<ContainerGroup> {
        let name = self.selected_group_name();
        if name.is_empty() {
            return None;
        }
        self.container_groups().into_iter().find(|group| group.name == name)
    }

    pub fn selected_group_by_name(&self, name: &str) -> ContainerGroup {
        self.container_groups()
            .into_iter()
            .find(|group| group.name == name)
            .unwrap_or_else(|| ContainerGroup { name: name.to_string(), indices: Vec::new() })
    }

    pub fn toggle_selected_group(&mut self) {
        let name = self.selected_group_name();
        if name.is_empty() {
            return;
        }
        self.sync_expanded();
        let expanded = self.expanded.entry(name).or_insert(false);
        *expanded = !*expanded;
        self.err = None;
        self.status = "ready".to_string();
    }

    pub fn selected_id(&self) -> String {
        self.selected_container().map(|c| c.id.clone()).unwrap_or_default()
    }

    pub fn current_profile(&self) -> Option<&Profile> {
        self.profiles.get(self.profile_index)
    }

    pub fn current_profile_name(&self) -> String {
        self.current_profile().map_or_else(|| "default".to_string(), |p| p.name.clone())
    }

    /// A query is a case-insensitive substring of any searchable field.
    pub fn matches_filter(&self, c: &Container) -> bool {
        if self.running_only && c.state.to_lowercase() != "running" {
            return false;
        }
        let query = self.search_query.trim().to_lowercase();
        if query.is_empty() {
            return true;
        }
        [&c.name, &c.image, &c.compose_project, &c.compose_service, &c.state]
            .iter()
            .any(|value| value.to_lowercase().contains(&query))
    }

    pub fn matching_count(&self) -> usize {
        self.containers.iter().filter(|c| self.matches_filter(c)).count()
    }
}

pub fn group_summary(group: &ContainerGroup, containers: &[Container]) -> String {
    let running = group.indices.iter().filter(|&&i| is_running(&containers[i].state)).count();
    format!("{running}/{} running", group.indices.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tea::{Key, KeyCode};
    use crate::testutil::{container, model};
    use crate::update::shortcut_key;

    fn search_model() -> Model {
        let mut m = model();
        m.containers = vec![
            Container {
                compose_project: "Shop".into(),
                compose_service: "web".into(),
                image: "nginx:alpine".into(),
                ..container("a", "Api-One", "running")
            },
            Container {
                compose_project: "Shop".into(),
                image: "postgres".into(),
                ..container("b", "database", "exited")
            },
            Container { image: "redis".into(), ..container("c", "worker", "running") },
        ];
        m.container_index = m.first_container_item();
        m
    }

    #[test]
    fn compose_groups() {
        let mut m = model();
        m.containers = vec![
            Container {
                compose_project: "ides".into(),
                compose_service: "postgres".into(),
                ..container("1", "ides-postgres-1", "running")
            },
            Container {
                compose_project: "ides".into(),
                compose_service: "api".into(),
                ..container("2", "ides-api-1", "exited")
            },
            container("3", "other", "running"),
        ];
        m.sync_expanded();
        let items = m.list_items();
        assert_eq!(items.len(), 4);
        assert!(items[0].group_header && items[0].group == "ides");
        assert_eq!((items[1].container_index, items[2].container_index), (0, 1));
        assert_eq!(items[3].group, STANDALONE);
        assert_eq!(group_summary(&m.selected_group_by_name("ides"), &m.containers), "1/2 running");
    }

    #[test]
    fn enter_toggles_compose_group() {
        let mut m = model();
        m.containers = vec![Container {
            compose_project: "ides".into(),
            compose_service: "postgres".into(),
            ..container("1", "ides-postgres-1", "")
        }];
        m.expanded.insert("ides".into(), true);
        m.update(crate::model::Msg::Key(Key::new(KeyCode::Enter)));
        assert!(!m.expanded["ides"]);
        let items = m.list_items();
        assert!(items.len() == 1 && items[0].group_header);
    }

    #[test]
    fn search_fields_and_running_intersection() {
        for (query, running, count) in [
            ("API-ONE", false, 1),
            ("alpine", false, 1),
            ("shop", false, 2),
            ("exited", false, 1),
            ("web", false, 1),
            ("shop", true, 1),
            ("exited", true, 0),
            ("", true, 2),
            ("   ", false, 3),
            ("absent", false, 0),
        ] {
            let mut m = search_model();
            m.search_query = query.into();
            m.running_only = running;
            assert_eq!(m.matching_count(), count, "{query:?} running={running}");
        }
    }

    #[test]
    fn search_editing_does_not_dispatch_shortcuts() {
        let mut m = search_model();
        m.key(shortcut_key("/"));
        // The only command allowed is the log reload for the re-filtered selection.
        let cmd = m.key(shortcut_key("d"));
        assert!(
            matches!(cmd.map(crate::testutil::run_one), None | Some(crate::model::Msg::Logs(_)))
                && !m.confirm_delete
                && m.active_actions.is_empty()
                && m.search_query == "d",
            "search dispatched shortcut"
        );
        m.key(Key::new(KeyCode::Esc));
        assert!(m.search_query.is_empty() && !m.search_editing, "cancel did not restore query");
        m.key(shortcut_key("/"));
        m.key(shortcut_key("数据库"));
        m.key(Key::new(KeyCode::Backspace));
        assert_eq!(m.search_query, "数据");
        m.key(Key::new(KeyCode::Enter));
        assert!(!m.search_editing && m.search_query == "数据", "enter did not apply");
    }

    #[test]
    fn filter_selection_and_refresh() {
        let mut m = search_model();
        m.container_index = m.find_container_item("b").unwrap();
        m.key(shortcut_key("R"));
        assert_eq!(m.selected_id(), "a");
        m.search_query = "redis".into();
        m.filter_selection("a");
        assert_eq!(m.selected_id(), "c", "wrong filtered identity");
        let containers = m.containers.clone();
        m.update(crate::testutil::refresh(vec![], containers));
        assert!(
            m.selected_id() == "c" && m.running_only && m.search_query == "redis",
            "refresh lost filters/selection"
        );
        m.search_query = "absent".into();
        m.filter_selection("c");
        assert!(m.selected_container().is_none() && m.list_items().is_empty(), "empty results still selectable");
        assert!(m.render_containers(20, 38).contains("no matches"));
        assert!(m.key(Key::new(KeyCode::Enter)).is_none(), "empty list dispatched action");
        m.key(Key::new(KeyCode::Esc));
        assert!(m.matching_count() == 3 && !m.selected_id().is_empty(), "clear did not restore results");
    }

    #[test]
    fn search_reveals_collapsed_groups() {
        let mut m = search_model();
        m.expanded.insert("Shop".into(), false);
        m.search_query = "nginx".into();
        assert!(m.find_container_item("a").is_some(), "matching service hidden");
        assert!(!m.expanded["Shop"], "search changed saved expansion");
    }
}
