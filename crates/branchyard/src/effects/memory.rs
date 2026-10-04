//! The ledger and asks in memory: for tests, and the reference the SQLite
//! and PostgreSQL backends are checked against
//! (`crate::conformance::effects`).

use branchyard_support::LockExt as _;
use std::sync::Mutex;

use super::{
    project, ApprovalAsk, AskAnswer, EffectBackend, EffectChange, EffectEntry, EffectEvent,
    EffectMove, EffectState,
};
use crate::Error;

#[derive(Debug, Default)]
pub(crate) struct Memory {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    events: Vec<EffectEvent>,
    asks: Vec<ApprovalAsk>,
}

impl Inner {
    fn entry(&self, id: &str) -> Option<EffectEntry> {
        let events: Vec<EffectEvent> = self.events.iter().filter(|e| e.id == id).cloned().collect();
        project(&events)
    }
}

impl Memory {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock_recovering("inner")
    }
}

impl EffectBackend for Memory {
    fn open_effect(&self, entry: &EffectEntry) -> Result<(), Error> {
        let mut inner = self.lock();
        if inner.events.iter().any(|e| e.id == entry.id) {
            return Err(Error::State(format!("effect {} exists", entry.id)));
        }
        let seq = inner.events.len() as u64 + 1;
        inner.events.push(EffectEvent {
            seq,
            id: entry.id.clone(),
            at_ms: entry.created_ms,
            change: EffectChange::Opened {
                entry: Box::new(entry.clone()),
            },
        });
        Ok(())
    }

    fn move_effect(
        &self,
        id: &str,
        from: &[EffectState],
        change: &EffectMove,
        at_ms: u64,
    ) -> Result<Option<EffectEntry>, Error> {
        let mut inner = self.lock();
        let Some(mut entry) = inner.entry(id) else {
            return Ok(None);
        };
        if !from.is_empty() && !from.contains(&entry.state) {
            return Ok(None);
        }
        let seq = inner.events.len() as u64 + 1;
        inner.events.push(EffectEvent {
            seq,
            id: id.to_owned(),
            at_ms,
            change: EffectChange::Moved(Box::new(change.clone())),
        });
        change.apply(&mut entry, at_ms);
        Ok(Some(entry))
    }

    fn effect(&self, id: &str) -> Result<Option<EffectEntry>, Error> {
        Ok(self.lock().entry(id))
    }

    fn effects(&self, branch: Option<&str>) -> Result<Vec<EffectEntry>, Error> {
        let inner = self.lock();
        let mut ids: Vec<&str> = Vec::new();
        for event in &inner.events {
            if !ids.contains(&event.id.as_str()) {
                ids.push(&event.id);
            }
        }
        let mut entries: Vec<EffectEntry> = ids
            .into_iter()
            .filter_map(|id| inner.entry(id))
            .filter(|e| branch.is_none_or(|b| e.branch == b))
            .collect();
        entries.sort_by(|a, b| a.created_ms.cmp(&b.created_ms).then(a.id.cmp(&b.id)));
        Ok(entries)
    }

    fn effect_events(&self, id: &str) -> Result<Vec<EffectEvent>, Error> {
        Ok(self
            .lock()
            .events
            .iter()
            .filter(|e| e.id == id)
            .cloned()
            .collect())
    }

    fn put_ask(&self, ask: &ApprovalAsk) -> Result<(), Error> {
        let mut inner = self.lock();
        if inner.asks.iter().any(|a| a.id == ask.id) {
            return Err(Error::State(format!("approval {} exists", ask.id)));
        }
        inner.asks.push(ask.clone());
        Ok(())
    }

    fn answer_ask(
        &self,
        id: &str,
        answer: &AskAnswer,
    ) -> Result<Option<(ApprovalAsk, bool)>, Error> {
        let mut inner = self.lock();
        let Some(ask) = inner.asks.iter_mut().find(|a| a.id == id) else {
            return Ok(None);
        };
        if ask.answer.is_some() {
            return Ok(Some((ask.clone(), false)));
        }
        ask.answer = Some(answer.clone());
        Ok(Some((ask.clone(), true)))
    }

    fn ask(&self, id: &str) -> Result<Option<ApprovalAsk>, Error> {
        Ok(self.lock().asks.iter().find(|a| a.id == id).cloned())
    }

    fn asks(&self, pending_only: bool) -> Result<Vec<ApprovalAsk>, Error> {
        let mut asks: Vec<ApprovalAsk> = self
            .lock()
            .asks
            .iter()
            .filter(|a| !pending_only || a.pending())
            .cloned()
            .collect();
        asks.sort_by(|a, b| a.created_ms.cmp(&b.created_ms).then(a.id.cmp(&b.id)));
        Ok(asks)
    }
}
