//! Per-conversation state: the pinned boundaries, when each model last ran, and
//! whether the client clears tool results itself. It is soft state: losing it costs
//! one cache rewrite, never correctness. Two proxies on one project merge on write.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::plan::Conversation;
use crate::workspace::{lock_state, write_json};

const MAX_CONVERSATIONS: usize = 512;
const MAX_BOUNDARIES: usize = 64;

#[derive(Default, Serialize, Deserialize)]
struct Saved {
    conversations: BTreeMap<String, Conversation>,
}

/// What this process remembers about a conversation to count calls that repeat a
/// cleared one.
#[derive(Default)]
pub struct Calls {
    pub seen: HashSet<String>,
    pub cleared: HashSet<u64>,
    pub hashed: usize,
    pub started: bool,
}

pub struct Store {
    path: PathBuf,
    conversations: HashMap<String, Conversation>,
    calls: HashMap<String, Calls>,
    dirty: bool,
}

impl Store {
    pub fn load(path: PathBuf) -> Self {
        let conversations = fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Saved>(&bytes).ok())
            .map(|saved| saved.conversations.into_iter().collect())
            .unwrap_or_default();
        Self {
            path,
            conversations,
            calls: HashMap::new(),
            dirty: false,
        }
    }

    pub fn get(&self, key: &str) -> Option<&Conversation> {
        self.conversations.get(key)
    }

    pub fn update(&mut self, key: &str, now_ms: u64, change: impl FnOnce(&mut Conversation)) {
        let conversation = self.conversations.entry(key.to_owned()).or_default();
        change(conversation);
        conversation.touched = now_ms;
        let excess = conversation.boundaries.len().saturating_sub(MAX_BOUNDARIES);
        conversation.boundaries.drain(..excess);
        self.dirty = true;
    }

    pub fn calls(&mut self, key: &str) -> &mut Calls {
        self.calls.entry(key.to_owned()).or_default()
    }

    pub fn dirty(&self) -> bool {
        self.dirty
    }

    /// Merges with the file under a lock and writes the result: boundaries are
    /// unioned in order, the later start time wins, and the 512 most recently used
    /// conversations are kept.
    pub fn save(&mut self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        let _guard = lock_state(&self.path.with_extension("lock"))?;
        let mut saved: Saved = match fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
            Err(error) if error.kind() == ErrorKind::NotFound => Saved::default(),
            Err(error) => return Err(error.into()),
        };
        for (key, ours) in &self.conversations {
            let merged = saved.conversations.entry(key.clone()).or_default();
            for id in &ours.boundaries {
                if !merged.boundaries.contains(id) {
                    merged.boundaries.push(id.clone());
                }
            }
            let excess = merged.boundaries.len().saturating_sub(MAX_BOUNDARIES);
            merged.boundaries.drain(..excess);
            for (model, &start) in &ours.last_start {
                let entry = merged.last_start.entry(model.clone()).or_default();
                *entry = (*entry).max(start);
            }
            merged.client_managed |= ours.client_managed;
            merged.touched = merged.touched.max(ours.touched);
        }
        if saved.conversations.len() > MAX_CONVERSATIONS {
            let mut by_use: Vec<(u64, String)> = saved
                .conversations
                .iter()
                .map(|(key, c)| (c.touched, key.clone()))
                .collect();
            by_use.sort_unstable_by(|a, b| b.cmp(a));
            for (_, key) in by_use.into_iter().skip(MAX_CONVERSATIONS) {
                saved.conversations.remove(&key);
            }
        }
        write_json(&self.path, &saved)?;
        self.conversations = saved.conversations.into_iter().collect();
        self.dirty = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_stores_merge_on_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("conversations.json");
        let mut first = Store::load(path.clone());
        let mut second = Store::load(path.clone());
        first.update("toolu_a", 10, |c| {
            c.boundaries.push("toolu_b".into());
            c.last_start.insert("claude-opus-5-5".into(), 100);
        });
        second.update("toolu_a", 20, |c| {
            c.boundaries.push("toolu_c".into());
            c.last_start.insert("claude-opus-5-5".into(), 50);
            c.client_managed = true;
        });
        first.save().unwrap();
        second.save().unwrap();
        let merged = Store::load(path);
        let c = merged.get("toolu_a").unwrap();
        assert_eq!(c.boundaries, ["toolu_b", "toolu_c"]);
        assert_eq!(c.last_start["claude-opus-5-5"], 100);
        assert!(c.client_managed);
        assert_eq!(c.touched, 20);
    }

    #[test]
    fn merge_order_never_moves_a_boundary_back() {
        use crate::proxy::plan::Use;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("conversations.json");
        let mut first = Store::load(path.clone());
        let mut second = Store::load(path.clone());
        first.update("toolu_00", 10, |c| c.boundaries.push("toolu_04".into()));
        second.update("toolu_00", 20, |c| c.boundaries.push("toolu_08".into()));
        second.save().unwrap();
        first.save().unwrap();
        second.update("toolu_00", 30, |_| {});
        second.save().unwrap();
        let uses: Vec<Use> = (0..12)
            .map(|i| Use {
                id: format!("toolu_{i:02}"),
                name: "Read".into(),
                call: (i, 0),
                result: None,
                result_bytes: 0,
                result_tokens: 0,
            })
            .collect();
        for store in [&first, &second, &Store::load(path)] {
            let c = store.get("toolu_00").unwrap();
            assert_eq!(c.boundary(&uses), 9, "{:?}", c.boundaries);
        }
    }

    #[test]
    fn keeps_the_most_recent_conversations_and_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("conversations.json");
        let mut store = Store::load(path.clone());
        for i in 0..(MAX_CONVERSATIONS as u64 + 3) {
            store.update(&format!("toolu_{i}"), i, |_| {});
        }
        store.update("toolu_0", 10_000, |c| {
            c.boundaries = (0..70).map(|i| format!("b{i}")).collect();
        });
        store.save().unwrap();
        let loaded = Store::load(path);
        assert_eq!(loaded.conversations.len(), MAX_CONVERSATIONS);
        assert!(loaded.get("toolu_0").is_some());
        assert!(loaded.get("toolu_1").is_none());
        let boundaries = &loaded.get("toolu_0").unwrap().boundaries;
        assert_eq!(boundaries.len(), MAX_BOUNDARIES);
        assert_eq!(boundaries.last().unwrap(), "b69");
    }
}
