use std::collections::HashMap;
use std::sync::Mutex;

use loro::VersionVector;

/// What a store holds for one project: the latest snapshot and the log entries
/// written after it, in order.
#[derive(Debug, Clone, Default)]
pub struct Stored {
    pub snapshot: Option<Vec<u8>>,
    pub log: Vec<Vec<u8>>,
}

/// Durable storage for the rooms. The room calls these methods on a blocking
/// thread, so they may block on disk.
pub trait RoomStore: Send + Sync + 'static {
    fn load(&self, project: &str) -> Result<Stored, String>;

    /// Appends one update if `admit` allows it at the moment of writing.
    /// Returns `false`, and stores nothing, when it does not; otherwise
    /// returns `true` only after the update is durable. A store that ends
    /// access under the same lock orders that end before or after the write.
    fn append(
        &self,
        project: &str,
        update: &[u8],
        admit: &dyn Fn() -> bool,
    ) -> Result<bool, String>;

    /// Replaces the snapshot and removes every log entry that `covers`
    /// includes, in one atomic step.
    fn compact(&self, project: &str, covers: &VersionVector, snapshot: &[u8])
    -> Result<(), String>;
}

/// A store in memory, for tests.
#[derive(Default)]
pub struct MemoryStore(Mutex<HashMap<String, Stored>>);

impl RoomStore for MemoryStore {
    fn load(&self, project: &str) -> Result<Stored, String> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .get(project)
            .cloned()
            .unwrap_or_default())
    }

    fn append(
        &self,
        project: &str,
        update: &[u8],
        admit: &dyn Fn() -> bool,
    ) -> Result<bool, String> {
        let mut map = self.0.lock().unwrap();
        if !admit() {
            return Ok(false);
        }
        map.entry(project.to_owned())
            .or_default()
            .log
            .push(update.to_vec());
        Ok(true)
    }

    fn compact(
        &self,
        project: &str,
        covers: &VersionVector,
        snapshot: &[u8],
    ) -> Result<(), String> {
        let mut map = self.0.lock().unwrap();
        let stored = map.entry(project.to_owned()).or_default();
        stored.snapshot = Some(snapshot.to_vec());
        stored.log.retain(|u| {
            loro::LoroDoc::decode_import_blob_meta(u, false)
                .map_or(true, |m| !covers.includes_vv(&m.partial_end_vv))
        });
        Ok(())
    }
}
