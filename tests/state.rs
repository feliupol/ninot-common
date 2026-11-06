//! The state handle a fake shares with its handlers and its tests, and its snapshots.
#![cfg(feature = "state")]

use ninot_common::state::{Snapshot, SnapshotError, StateHandle};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct World {
    jobs: Vec<String>,
    next_id: u64,
}

const VERSION: u32 = 3;

#[test]
fn reads_and_changes_go_through_the_same_state() {
    let handle = StateHandle::new(World::default());
    handle.update(|world| {
        world.jobs.push("build".into());
        world.next_id = 7;
    });
    let shared = handle.clone();
    assert_eq!(shared.read(|world| world.jobs.clone()), ["build"]);
    assert_eq!(handle.read(|world| world.next_id), 7);
}

#[test]
fn a_snapshot_round_trips_the_whole_state() {
    let handle = StateHandle::new(World {
        jobs: (0..1000).map(|n| format!("job-{n}")).collect(),
        next_id: 1000,
    });
    let snapshot = handle.snapshot(VERSION);
    let restored: StateHandle<World> = StateHandle::restore(&snapshot, VERSION).unwrap();
    assert_eq!(restored.read(Clone::clone), handle.read(Clone::clone));
    // A restored state is independent of the one it was taken from.
    restored.update(|world| world.jobs.clear());
    assert_eq!(handle.read(|world| world.jobs.len()), 1000);
}

#[test]
fn snapshots_save_to_and_load_from_files() {
    let dir = std::env::temp_dir().join(format!("ninot-state-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("world.json");
    let handle = StateHandle::new(World {
        jobs: vec!["a".into()],
        next_id: 2,
    });
    handle.snapshot(VERSION).save(&path).unwrap();
    let loaded = Snapshot::load(&path).unwrap();
    let restored: StateHandle<World> = StateHandle::restore(&loaded, VERSION).unwrap();
    assert_eq!(restored.read(|world| world.next_id), 2);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_snapshot_of_another_version_is_refused() {
    let snapshot = StateHandle::new(World::default()).snapshot(VERSION);
    match StateHandle::<World>::restore(&snapshot, VERSION + 1) {
        Err(SnapshotError::Version { found, expected }) => {
            assert_eq!((found, expected), (VERSION, VERSION + 1));
        }
        other => panic!("expected a version error, got {other:?}"),
    }
}

#[test]
fn a_snapshot_that_does_not_fit_the_state_is_refused() {
    let snapshot = Snapshot::from_json(r#"{"version":3,"state":{"jobs":42}}"#).unwrap();
    assert!(matches!(
        StateHandle::<World>::restore(&snapshot, VERSION),
        Err(SnapshotError::Shape(_))
    ));
    assert!(Snapshot::from_json("not json").is_err());
}
