//! The state a fake shares with its handlers and its tests, and snapshots of it.
//!
//! A fake keeps everything it knows in one `State` value of its own type behind a
//! [`StateHandle`]. Handlers and tests read and change it through the same handle, so a
//! test builds the world it needs directly, without going through the fake's API. A
//! [`Snapshot`] is the whole state serialized, with the version of its shape, so a rich
//! world can be built once, saved, and restored into each test's fake in one deserialize.

use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

/// A fake's state, shared between its handlers and the test driving it.
pub struct StateHandle<S>(Arc<Mutex<S>>);

impl<S> Clone for StateHandle<S> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<S: fmt::Debug> fmt::Debug for StateHandle<S> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("StateHandle")
            .field(&*self.lock())
            .finish()
    }
}

impl<S> StateHandle<S> {
    pub fn new(state: S) -> Self {
        Self(Arc::new(Mutex::new(state)))
    }

    /// Runs `read` on the state and returns what it returns.
    pub fn read<T>(&self, read: impl FnOnce(&S) -> T) -> T {
        read(&self.lock())
    }

    /// Runs `update` on the state and returns what it returns.
    pub fn update<T>(&self, update: impl FnOnce(&mut S) -> T) -> T {
        update(&mut self.lock())
    }

    /// The state, locked, for code that holds it across several steps. A handler that
    /// panicked while holding it leaves it as it was, which a fake treats as usable.
    pub fn lock(&self) -> MutexGuard<'_, S> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl<S: Serialize> StateHandle<S> {
    /// The whole state, tagged with `version`, the version of its shape.
    pub fn snapshot(&self, version: u32) -> Snapshot {
        let state = serde_json::to_value(&*self.lock()).expect("a fake's state serializes");
        Snapshot { version, state }
    }
}

impl<S: DeserializeOwned> StateHandle<S> {
    /// A new handle holding the state `snapshot` describes, which must have been taken
    /// from a state of the shape `version`.
    pub fn restore(snapshot: &Snapshot, version: u32) -> Result<Self, SnapshotError> {
        if snapshot.version != version {
            return Err(SnapshotError::Version {
                found: snapshot.version,
                expected: version,
            });
        }
        serde_json::from_value(snapshot.state.clone())
            .map(Self::new)
            .map_err(|error| SnapshotError::Shape(error.to_string()))
    }
}

/// A fake's whole state, serialized, with the version of its shape.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Snapshot {
    pub version: u32,
    pub state: Value,
}

impl Snapshot {
    /// Reads a snapshot written by [`Snapshot::save`].
    pub fn load(path: &Path) -> Result<Self, SnapshotError> {
        let text =
            std::fs::read_to_string(path).map_err(|error| SnapshotError::Io(error.to_string()))?;
        Self::from_json(&text)
    }

    /// Parses a snapshot from its JSON text.
    pub fn from_json(text: &str) -> Result<Self, SnapshotError> {
        serde_json::from_str(text).map_err(|error| SnapshotError::Shape(error.to_string()))
    }

    /// Writes the snapshot as JSON.
    pub fn save(&self, path: &Path) -> Result<(), SnapshotError> {
        let text = serde_json::to_string(self).expect("a snapshot serializes");
        std::fs::write(path, text).map_err(|error| SnapshotError::Io(error.to_string()))
    }
}

/// Why a snapshot could not be loaded or restored.
#[derive(Debug, PartialEq, Eq)]
pub enum SnapshotError {
    /// It was taken from a state of another shape version.
    Version { found: u32, expected: u32 },
    /// It is not a snapshot, or its state does not fit the fake's state type.
    Shape(String),
    /// It could not be read or written.
    Io(String),
}

impl fmt::Display for SnapshotError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Version { found, expected } => {
                write!(
                    formatter,
                    "snapshot of state version {found}, expected {expected}"
                )
            }
            Self::Shape(message) => write!(formatter, "snapshot does not fit the state: {message}"),
            Self::Io(message) => write!(formatter, "snapshot file: {message}"),
        }
    }
}

impl std::error::Error for SnapshotError {}
