use super::util::coordination_scope_for_dir;
use crate::todo::TodoItem;
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// A newly observed overlap between two independent session TODOs.
///
/// `attempt_id` is an observation/coordination episode id. It is deliberately
/// not an ownership or assignment id: this registry only warns about possible
/// duplicate work and never merges TODOs, swarms, or plans.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct TodoOverlapNotice {
    pub left_session_id: String,
    pub right_session_id: String,
    pub left_todo_id: String,
    pub right_todo_id: String,
    pub match_kind: &'static str,
    pub attempt_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct OverlapKey {
    left_session_id: String,
    right_session_id: String,
    work_key: String,
}

#[derive(Clone, Debug)]
struct TrackedTodo {
    id: String,
    keys: Vec<(String, &'static str)>,
}

#[derive(Clone, Debug)]
struct SessionTodoState {
    scope: Option<String>,
    todos: Vec<TrackedTodo>,
}

#[derive(Clone, Debug)]
struct ActiveOverlap {
    key: OverlapKey,
    left_todo_id: String,
    right_todo_id: String,
    match_kind: &'static str,
}

/// Server-local coordination ledger for independent root sessions.
///
/// This is intentionally in-memory and advisory. It covers the common case
/// where several clients share one Jcode server and independently create TODO
/// plans for the same repository. The TODO files remain session-private, and
/// no registry record is persisted until a later design proves that a durable
/// cross-session record is needed without leaking TODO content.
#[derive(Default)]
pub(super) struct WorkItemCoordinationRegistry {
    sessions: HashMap<String, SessionTodoState>,
    active_episodes: HashMap<OverlapKey, String>,
}

impl WorkItemCoordinationRegistry {
    /// Replace one session's snapshot and return only newly opened overlap
    /// episodes. Replaying the same TODO update is therefore idempotent.
    pub(super) fn update_session(
        &mut self,
        session_id: &str,
        working_dir: Option<&Path>,
        todos: &[TodoItem],
    ) -> Vec<TodoOverlapNotice> {
        self.sessions.insert(
            session_id.to_string(),
            SessionTodoState {
                scope: coordination_scope_for_dir(working_dir),
                todos: tracked_todos(todos),
            },
        );

        let overlaps = self.active_overlaps();
        let mut next_episodes = HashMap::with_capacity(overlaps.len());
        let mut notices = Vec::new();

        for overlap in overlaps {
            let attempt_id = self
                .active_episodes
                .get(&overlap.key)
                .cloned()
                .unwrap_or_else(|| crate::id::new_id("coord-attempt"));
            if !self.active_episodes.contains_key(&overlap.key) {
                notices.push(TodoOverlapNotice {
                    left_session_id: overlap.key.left_session_id.clone(),
                    right_session_id: overlap.key.right_session_id.clone(),
                    left_todo_id: overlap.left_todo_id.clone(),
                    right_todo_id: overlap.right_todo_id.clone(),
                    match_kind: overlap.match_kind,
                    attempt_id: attempt_id.clone(),
                });
            }
            next_episodes.insert(overlap.key, attempt_id);
        }

        self.active_episodes = next_episodes;
        notices.sort_by(|left, right| left.attempt_id.cmp(&right.attempt_id));
        notices
    }

    /// Remove snapshots for sessions that are no longer live on this server.
    /// Existing episodes are retained only while both source sessions and the
    /// matching active work remain present.
    pub(super) fn prune_missing_sessions(&mut self, live_session_ids: &HashSet<String>) {
        self.sessions
            .retain(|session_id, _| live_session_ids.contains(session_id));
        let active_keys: HashSet<OverlapKey> = self
            .active_overlaps()
            .into_iter()
            .map(|overlap| overlap.key)
            .collect();
        self.active_episodes
            .retain(|key, _| active_keys.contains(key));
    }

    fn active_overlaps(&self) -> Vec<ActiveOverlap> {
        let mut session_ids: Vec<&String> = self.sessions.keys().collect();
        session_ids.sort();

        let mut overlaps = HashMap::<OverlapKey, ActiveOverlap>::new();
        for (left_index, left_session_id) in session_ids.iter().enumerate() {
            let Some(left) = self.sessions.get(*left_session_id) else {
                continue;
            };
            let Some(left_scope) = left.scope.as_deref() else {
                continue;
            };

            for right_session_id in session_ids.iter().skip(left_index + 1) {
                let Some(right) = self.sessions.get(*right_session_id) else {
                    continue;
                };
                if right.scope.as_deref() != Some(left_scope) {
                    continue;
                }

                for left_todo in &left.todos {
                    for right_todo in &right.todos {
                        let Some((work_key, match_kind)) = shared_work_key(left_todo, right_todo)
                        else {
                            continue;
                        };
                        let key = OverlapKey {
                            left_session_id: (*left_session_id).clone(),
                            right_session_id: (*right_session_id).clone(),
                            work_key,
                        };
                        overlaps
                            .entry(key.clone())
                            .or_insert_with(|| ActiveOverlap {
                                key,
                                left_todo_id: left_todo.id.clone(),
                                right_todo_id: right_todo.id.clone(),
                                match_kind,
                            });
                    }
                }
            }
        }

        let mut overlaps: Vec<ActiveOverlap> = overlaps.into_values().collect();
        overlaps.sort_by(|left, right| {
            left.key
                .left_session_id
                .cmp(&right.key.left_session_id)
                .then_with(|| left.key.right_session_id.cmp(&right.key.right_session_id))
                .then_with(|| left.key.work_key.cmp(&right.key.work_key))
        });
        overlaps
    }
}

fn tracked_todos(todos: &[TodoItem]) -> Vec<TrackedTodo> {
    let mut tracked = todos
        .iter()
        .filter(|todo| !terminal_status(&todo.status))
        .filter_map(|todo| {
            let content_key = normalize_work_text(&todo.content);
            let id_key = normalize_work_text(&todo.id);
            let mut keys = Vec::new();
            if !id_key.is_empty() {
                keys.push((format!("id:{id_key}"), "shared TODO id"));
            }
            if !content_key.is_empty() {
                keys.push((format!("content:{content_key}"), "matching TODO text"));
            }
            (!keys.is_empty()).then(|| TrackedTodo {
                id: if todo.id.trim().is_empty() {
                    "(unnamed)".to_string()
                } else {
                    todo.id.trim().to_string()
                },
                keys,
            })
        })
        .collect::<Vec<_>>();
    tracked.sort_by(|left, right| left.id.cmp(&right.id));
    tracked
}

fn shared_work_key(left: &TrackedTodo, right: &TrackedTodo) -> Option<(String, &'static str)> {
    // An identical stable TODO id is stronger evidence than matching prose.
    let left_ids: HashSet<&str> = left
        .keys
        .iter()
        .filter(|(_, kind)| *kind == "shared TODO id")
        .map(|(key, _)| key.as_str())
        .collect();
    if let Some((key, _)) = right
        .keys
        .iter()
        .find(|(key, kind)| *kind == "shared TODO id" && left_ids.contains(key.as_str()))
    {
        return Some((key.clone(), "shared TODO id"));
    }

    left.keys.iter().find_map(|(left_key, left_kind)| {
        right
            .keys
            .iter()
            .find(|(right_key, _)| right_key == left_key)
            .map(|_| (left_key.clone(), *left_kind))
    })
}

fn terminal_status(status: &str) -> bool {
    matches!(
        status.trim().to_ascii_lowercase().as_str(),
        "completed" | "complete" | "done" | "finished" | "cancelled" | "canceled"
    )
}

fn normalize_work_text(text: &str) -> String {
    text.chars()
        .flat_map(char::to_lowercase)
        .map(|ch| if ch.is_alphanumeric() { ch } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn todo(id: &str, content: &str, status: &str) -> TodoItem {
        TodoItem {
            id: id.to_string(),
            content: content.to_string(),
            status: status.to_string(),
            priority: "high".to_string(),
            ..TodoItem::default()
        }
    }

    #[test]
    fn detects_same_active_work_once_and_keeps_attempt_id() {
        let mut registry = WorkItemCoordinationRegistry::default();
        let path = PathBuf::from(r"C:\coordination\project");
        let first = registry.update_session(
            "session-a",
            Some(&path),
            &[todo(
                "archive",
                "Extract unfinished archive tasks",
                "in_progress",
            )],
        );
        assert!(first.is_empty());

        let second = registry.update_session(
            "session-b",
            Some(&path),
            &[todo(
                "archive-copy",
                "extract unfinished archive tasks",
                "pending",
            )],
        );
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].match_kind, "matching TODO text");
        let attempt_id = second[0].attempt_id.clone();

        let replay = registry.update_session(
            "session-b",
            Some(&path),
            &[todo(
                "archive-copy",
                "extract unfinished archive tasks",
                "pending",
            )],
        );
        assert!(replay.is_empty());

        let first_update = registry.update_session(
            "session-a",
            Some(&path),
            &[todo(
                "archive",
                "Extract unfinished archive tasks",
                "running",
            )],
        );
        assert!(first_update.is_empty());
        assert_eq!(registry.active_episodes.values().next(), Some(&attempt_id));
    }

    #[test]
    fn shared_todo_id_wins_over_text_and_tracks_divergent_copies() {
        let mut registry = WorkItemCoordinationRegistry::default();
        let path = PathBuf::from(r"C:\coordination\project");
        registry.update_session(
            "session-a",
            Some(&path),
            &[todo("same-id", "Original wording", "pending")],
        );
        let notices = registry.update_session(
            "session-b",
            Some(&path),
            &[todo("same-id", "Different wording", "in_progress")],
        );
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].match_kind, "shared TODO id");
    }

    #[test]
    fn excludes_terminal_items_and_unrelated_or_missing_scopes() {
        let mut registry = WorkItemCoordinationRegistry::default();
        let first_path = PathBuf::from(r"C:\coordination\one");
        let second_path = PathBuf::from(r"C:\coordination\two");
        registry.update_session(
            "session-a",
            Some(&first_path),
            &[todo("done", "same work", "completed")],
        );
        assert!(
            registry
                .update_session(
                    "session-b",
                    Some(&first_path),
                    &[todo("other", "same work", "pending")],
                )
                .is_empty()
        );
        assert!(
            registry
                .update_session(
                    "session-c",
                    Some(&second_path),
                    &[todo("other-copy", "same work", "pending")],
                )
                .is_empty()
        );
        assert!(
            registry
                .update_session(
                    "session-d",
                    None,
                    &[todo("missing", "same work", "pending")],
                )
                .is_empty()
        );
    }

    #[test]
    fn pruning_live_sessions_closes_old_episode() {
        let mut registry = WorkItemCoordinationRegistry::default();
        let path = PathBuf::from(r"C:\coordination\project");
        registry.update_session(
            "session-a",
            Some(&path),
            &[todo("same", "same work", "pending")],
        );
        let notices = registry.update_session(
            "session-b",
            Some(&path),
            &[todo("copy", "same work", "pending")],
        );
        assert_eq!(notices.len(), 1);
        registry.prune_missing_sessions(&HashSet::from(["session-b".to_string()]));
        assert!(registry.active_episodes.is_empty());
    }
}
