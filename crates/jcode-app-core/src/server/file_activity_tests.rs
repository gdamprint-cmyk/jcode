use super::file_activity::coordination_session_ids;
use super::util::{coordination_scope_for_dir, same_coordination_scope};
use super::{FileAccess, latest_peer_touches};
use crate::bus::FileOp;
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

fn access(session_id: &str, op: FileOp, age_ms: u64) -> FileAccess {
    let now = Instant::now();
    FileAccess {
        session_id: session_id.to_string(),
        op,
        timestamp: now
            .checked_sub(Duration::from_millis(age_ms))
            .unwrap_or(now),
        absolute_time: SystemTime::now(),
        intent: None,
        summary: None,
        detail: None,
    }
}

#[test]
fn latest_peer_touches_excludes_previous_readers_from_modification_alerts() {
    let swarm_session_ids = HashSet::from([
        "current".to_string(),
        "reader".to_string(),
        "writer".to_string(),
    ]);
    let accesses = vec![
        access("reader", FileOp::Read, 20),
        access("current", FileOp::Edit, 10),
        access("writer", FileOp::Write, 5),
    ];

    let latest = latest_peer_touches(&accesses, "current", &swarm_session_ids);

    assert_eq!(latest.len(), 1);
    assert!(!latest.iter().any(|entry| entry.session_id == "reader"));
    assert!(
        latest
            .iter()
            .any(|entry| entry.session_id == "writer" && entry.op == FileOp::Write)
    );
}

#[test]
fn latest_peer_touches_deduplicates_to_most_recent_touch_per_peer() {
    let swarm_session_ids = HashSet::from(["current".to_string(), "peer".to_string()]);
    let accesses = vec![
        access("peer", FileOp::Read, 30),
        access("peer", FileOp::Edit, 5),
        access("current", FileOp::Write, 1),
    ];

    let latest = latest_peer_touches(&accesses, "current", &swarm_session_ids);

    assert_eq!(latest.len(), 1);
    assert_eq!(latest[0].session_id, "peer");
    assert_eq!(latest[0].op, FileOp::Edit);
}

#[test]
fn coordination_scope_groups_independent_sessions_in_one_repository() {
    let root = tempfile::tempdir().expect("create temporary repository");
    let git_dir = root.path().join("repo/.git");
    let first = root.path().join("repo/first");
    let second = root.path().join("repo/second");
    fs::create_dir_all(&git_dir).expect("create fake git directory");
    fs::create_dir_all(&first).expect("create first working directory");
    fs::create_dir_all(&second).expect("create second working directory");

    let first_scope = coordination_scope_for_dir(Some(&first));
    let second_scope = coordination_scope_for_dir(Some(&second));

    assert_eq!(first_scope, second_scope);
    assert!(
        first_scope
            .as_deref()
            .is_some_and(|scope| scope.starts_with("git:"))
    );
    assert!(same_coordination_scope(Some(&first), Some(&second)));
}

#[test]
fn coordination_scope_groups_git_worktrees_without_merging_swarm_plans() {
    let root = tempfile::tempdir().expect("create temporary worktree layout");
    let common_git = root.path().join("repo/.git");
    let worktree_one = root.path().join("worktree-one");
    let worktree_two = root.path().join("worktree-two");
    fs::create_dir_all(common_git.join("worktrees/one")).expect("create first gitdir");
    fs::create_dir_all(common_git.join("worktrees/two")).expect("create second gitdir");
    fs::create_dir_all(&worktree_one).expect("create first worktree");
    fs::create_dir_all(&worktree_two).expect("create second worktree");
    fs::write(
        worktree_one.join(".git"),
        "gitdir: ../repo/.git/worktrees/one\n",
    )
    .expect("write first worktree git file");
    fs::write(
        worktree_two.join(".git"),
        "gitdir: ../repo/.git/worktrees/two\n",
    )
    .expect("write second worktree git file");

    assert_eq!(
        coordination_scope_for_dir(Some(&worktree_one)),
        coordination_scope_for_dir(Some(&worktree_two))
    );
    assert!(same_coordination_scope(
        Some(Path::new(&worktree_one)),
        Some(Path::new(&worktree_two))
    ));
}

#[test]
fn coordination_scope_does_not_group_unrelated_directories_or_missing_workdirs() {
    let root = tempfile::tempdir().expect("create temporary directories");
    let first = root.path().join("first");
    let second = root.path().join("second");
    fs::create_dir_all(&first).expect("create first directory");
    fs::create_dir_all(&second).expect("create second directory");

    assert_ne!(
        coordination_scope_for_dir(Some(&first)),
        coordination_scope_for_dir(Some(&second))
    );
    assert!(!same_coordination_scope(Some(&first), Some(&second)));
    assert_eq!(coordination_scope_for_dir(None), None);
    assert!(!same_coordination_scope(None, Some(&first)));
}

#[test]
fn coordination_session_ids_cover_three_roots_without_auto_joining_a_swarm() {
    let root = tempfile::tempdir().expect("create three-root layout");
    let repo = root.path().join("repo");
    let git_dir = repo.join(".git");
    let current = repo.join("current");
    let same_repo_peer = repo.join("peer");
    let unrelated = root.path().join("unrelated");
    fs::create_dir_all(&git_dir).expect("create fake git directory");
    fs::create_dir_all(&current).expect("create current root");
    fs::create_dir_all(&same_repo_peer).expect("create repository peer root");
    fs::create_dir_all(&unrelated).expect("create unrelated root");

    let explicit_swarm = HashSet::from(["legacy-worker".to_string()]);
    let peers = coordination_session_ids(
        "current",
        Some(&explicit_swarm),
        Some(&current),
        [
            ("current", Some(current.as_path())),
            ("repo-peer", Some(same_repo_peer.as_path())),
            ("unrelated", Some(unrelated.as_path())),
            ("no-workdir", None),
        ],
    );

    assert!(peers.contains("current"));
    assert!(peers.contains("repo-peer"));
    assert!(peers.contains("legacy-worker"));
    assert!(!peers.contains("unrelated"));
    assert!(!peers.contains("no-workdir"));
}
