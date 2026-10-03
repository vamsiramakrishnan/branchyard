//! `mem://name`: objects in this process's memory, shared by every handle
//! of one name. For tests and examples; nothing survives the process.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};

use crate::error::{Error, Result};
use crate::store::{check_key, check_prefix, Entry, Generation, Object, ObjectStore};

#[derive(Default)]
struct State {
    objects: BTreeMap<String, (Vec<u8>, u64, u64)>,
    next: u64,
}

pub struct MemoryStore {
    name: String,
    state: Arc<Mutex<State>>,
    clock: branchyard::services::Clock,
}

impl MemoryStore {
    /// A store of its own.
    pub fn new() -> MemoryStore {
        MemoryStore {
            name: "anonymous".into(),
            state: Arc::default(),
            clock: branchyard::services::Clock::system(),
        }
    }

    /// The store every `mem://name` in this process shares.
    pub fn named(name: &str) -> Arc<MemoryStore> {
        static ALL: OnceLock<Mutex<BTreeMap<String, Arc<Mutex<State>>>>> = OnceLock::new();
        let state = ALL
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(name.to_owned())
            .or_default()
            .clone();
        Arc::new(MemoryStore {
            name: name.to_owned(),
            state,
            clock: branchyard::services::Clock::system(),
        })
    }

    /// Times from `clock` (what `modified_ms` reports).
    pub fn with_clock(mut self, clock: branchyard::services::Clock) -> MemoryStore {
        self.clock = clock;
        self
    }

    fn with<T>(&self, f: impl FnOnce(&mut State) -> T) -> T {
        f(&mut self.state.lock().unwrap_or_else(|e| e.into_inner()))
    }

    fn write(&self, key: &str, data: &[u8], expect: Option<&str>) -> Result<Generation> {
        check_key(key)?;
        let now = self.clock.now();
        self.with(|s| {
            let current = s.objects.get(key).map(|(_, g, _)| g.to_string());
            match (expect, current) {
                (None, None) => {}
                (None, Some(_)) => {
                    return Err(Error::precondition(format!("{key} already exists")))
                }
                (Some(want), Some(have)) if want == have => {}
                (Some(_), _) => {
                    return Err(Error::precondition(format!(
                        "{key} is not at the expected generation"
                    )))
                }
            }
            s.next += 1;
            let generation = s.next;
            s.objects
                .insert(key.to_owned(), (data.to_vec(), generation, now));
            Ok(generation.to_string())
        })
    }
}

impl Default for MemoryStore {
    fn default() -> Self {
        MemoryStore::new()
    }
}

impl ObjectStore for MemoryStore {
    fn url(&self) -> String {
        format!("mem://{}", self.name)
    }

    fn get(&self, key: &str) -> Result<Object> {
        check_key(key)?;
        self.with(|s| {
            s.objects
                .get(key)
                .map(|(data, g, _)| Object {
                    data: data.clone(),
                    generation: g.to_string(),
                })
                .ok_or_else(|| Error::not_found(format!("{key} is not in the remote")))
        })
    }

    fn get_range(&self, key: &str, start: u64, len: u64) -> Result<Vec<u8>> {
        let object = self.get(key)?;
        let start = (start as usize).min(object.data.len());
        let end = start.saturating_add(len as usize).min(object.data.len());
        Ok(object.data[start..end].to_vec())
    }

    fn stat(&self, key: &str) -> Result<Option<Entry>> {
        check_key(key)?;
        Ok(self.with(|s| {
            s.objects.get(key).map(|(data, g, at)| Entry {
                key: key.to_owned(),
                size: data.len() as u64,
                generation: g.to_string(),
                modified_ms: Some(*at),
            })
        }))
    }

    fn put_if_absent(&self, key: &str, data: &[u8]) -> Result<Generation> {
        self.write(key, data, None)
    }

    fn put_if_match(&self, key: &str, data: &[u8], generation: &str) -> Result<Generation> {
        self.write(key, data, Some(generation))
    }

    fn list(&self, prefix: &str) -> Result<Vec<Entry>> {
        check_prefix(prefix)?;
        Ok(self.with(|s| {
            s.objects
                .range(prefix.to_owned()..)
                .take_while(|(k, _)| k.starts_with(prefix))
                .map(|(k, (data, g, at))| Entry {
                    key: k.clone(),
                    size: data.len() as u64,
                    generation: g.to_string(),
                    modified_ms: Some(*at),
                })
                .collect()
        }))
    }

    fn delete_if_match(&self, key: &str, generation: &str) -> Result<()> {
        check_key(key)?;
        self.with(|s| match s.objects.get(key) {
            Some((_, g, _)) if g.to_string() == generation => {
                s.objects.remove(key);
                Ok(())
            }
            _ => Err(Error::precondition(format!(
                "{key} is not at the expected generation"
            ))),
        })
    }
}
