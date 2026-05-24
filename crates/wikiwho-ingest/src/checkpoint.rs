//! Per-language SSE resume checkpoint.
//!
//! One JSON file at `<storage_root>/ingest/checkpoint.json` holds the
//! last-seen SSE event id per language. EventStreams' `id:` field is
//! itself a JSON array of `{topic, partition, offset, ...}` entries,
//! so we store it as an opaque string and pass it back unchanged via
//! `Last-Event-ID` on reconnect.
//!
//! Why one file rather than one-per-language: the typical deployment
//! ingests a small number of wikis (1-10); avoiding directory traversal
//! at startup is worth the tiny serialization cost. We write via
//! tmp-file + rename so a crashed `flush` can't leave the file
//! half-written.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// On-disk shape. Kept simple — a map of `language -> last_event_id`.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct CheckpointFile {
    /// Format version. Bump if we add fields with different defaults.
    #[serde(default = "default_version")]
    version: u32,
    /// Map of language code → opaque SSE event id (a JSON array
    /// rendered as a string).
    #[serde(default)]
    last_event_id: HashMap<String, String>,
}

fn default_version() -> u32 {
    1
}

/// In-memory checkpoint state. Owns the merged on-disk data plus a
/// dirty counter so the main loop can flush every N events.
#[derive(Debug, Clone)]
pub struct Checkpoint {
    data: CheckpointFile,
    dirty: usize,
}

impl Checkpoint {
    /// Load `<storage_root>/ingest/checkpoint.json` if present, or
    /// return an empty checkpoint pre-populated with the configured
    /// languages so the on-disk file gets written cleanly on first
    /// flush.
    pub fn load_or_init(storage_root: &Path, languages: &[String]) -> io::Result<Self> {
        let path = path_for(storage_root);
        let data = if path.exists() {
            let bytes = std::fs::read(&path)?;
            match serde_json::from_slice::<CheckpointFile>(&bytes) {
                Ok(d) => d,
                Err(err) => {
                    // Corrupt checkpoint: log and start fresh. Not
                    // fatal — the worst case is we replay a few hours
                    // of events.
                    tracing::warn!(error = %err, path = %path.display(), "corrupt checkpoint, starting fresh");
                    CheckpointFile::default()
                }
            }
        } else {
            CheckpointFile::default()
        };
        let mut cp = Self { data, dirty: 0 };
        if cp.data.version == 0 {
            cp.data.version = default_version();
        }
        for lang in languages {
            cp.data.last_event_id.entry(lang.clone()).or_default();
        }
        Ok(cp)
    }

    /// Record a new last-seen event id for `language`. Bumps the
    /// dirty counter; flush when [`dirty_count`] crosses a threshold.
    pub fn advance(&mut self, language: &str, event_id: &str) {
        let entry = self.data.last_event_id.entry(language.to_string()).or_default();
        if entry != event_id {
            *entry = event_id.to_string();
            self.dirty += 1;
        }
    }

    pub fn dirty_count(&self) -> usize {
        self.dirty
    }

    /// Pick the "best" Last-Event-ID header value across configured
    /// languages. EventStreams uses a global stream, so we just pick
    /// any non-empty value — preferring the most-recently-updated one
    /// would require a per-entry timestamp we don't track. In practice
    /// all languages converge within seconds because the stream is
    /// global.
    pub fn last_event_id_header(&self) -> Option<String> {
        self.data
            .last_event_id
            .values()
            .find(|s| !s.is_empty())
            .cloned()
    }

    /// Write the checkpoint to disk atomically (tmp-file + rename).
    pub fn flush(&mut self, storage_root: &Path) -> io::Result<()> {
        let path = path_for(storage_root);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_vec_pretty(&self.data)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &json)?;
        std::fs::rename(&tmp, &path)?;
        self.dirty = 0;
        Ok(())
    }

    /// Read-only accessor for tests.
    #[cfg(test)]
    pub fn last_event_id(&self, language: &str) -> Option<&str> {
        self.data
            .last_event_id
            .get(language)
            .map(String::as_str)
            .filter(|s| !s.is_empty())
    }
}

fn path_for(storage_root: &Path) -> PathBuf {
    storage_root.join("ingest").join("checkpoint.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_load_pre_populates_languages() {
        let tmp = tempfile::tempdir().unwrap();
        let cp = Checkpoint::load_or_init(tmp.path(), &["en".into(), "simple".into()]).unwrap();
        assert!(cp.last_event_id("en").is_none());
        assert!(cp.last_event_id("simple").is_none());
        assert_eq!(cp.dirty_count(), 0);
    }

    #[test]
    fn advance_and_flush_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cp = Checkpoint::load_or_init(tmp.path(), &["en".into()]).unwrap();
        cp.advance("en", "[{\"topic\":\"eqiad.mediawiki.recentchange\",\"partition\":0,\"offset\":42}]");
        assert_eq!(cp.dirty_count(), 1);
        cp.flush(tmp.path()).unwrap();
        assert_eq!(cp.dirty_count(), 0);

        let reloaded = Checkpoint::load_or_init(tmp.path(), &["en".into()]).unwrap();
        assert_eq!(
            reloaded.last_event_id("en"),
            Some("[{\"topic\":\"eqiad.mediawiki.recentchange\",\"partition\":0,\"offset\":42}]")
        );
    }

    #[test]
    fn advance_to_same_value_is_no_op_for_dirty_counter() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cp = Checkpoint::load_or_init(tmp.path(), &["en".into()]).unwrap();
        cp.advance("en", "id-1");
        cp.advance("en", "id-1");
        cp.advance("en", "id-1");
        assert_eq!(cp.dirty_count(), 1);
        cp.advance("en", "id-2");
        assert_eq!(cp.dirty_count(), 2);
    }

    #[test]
    fn last_event_id_header_prefers_non_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cp = Checkpoint::load_or_init(tmp.path(), &["en".into(), "simple".into()]).unwrap();
        assert!(cp.last_event_id_header().is_none());
        cp.advance("simple", "simple-id-1");
        assert_eq!(cp.last_event_id_header().as_deref(), Some("simple-id-1"));
    }

    #[test]
    fn corrupt_file_resets_cleanly() {
        let tmp = tempfile::tempdir().unwrap();
        let path = path_for(tmp.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"this is not valid json").unwrap();
        let cp = Checkpoint::load_or_init(tmp.path(), &["en".into()]).unwrap();
        assert_eq!(cp.dirty_count(), 0);
        assert!(cp.last_event_id("en").is_none());
    }
}
