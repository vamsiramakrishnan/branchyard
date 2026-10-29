//! Durable allocation intent for a task-owned repository.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::{folder, NewTask, Task};
use crate::{Error, Yard};

/// Serializable intent for initializing exactly one task-owned repository.
///
/// Persist this value with queue admission before calling
/// `initialize_reserved`. Replaying initialization never starts an attempt.
/// The paths are operator configuration, not browser-supplied source grants.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskReservation {
    pub(super) id: String,
    pub(super) created_ms: u64,
    pub(super) new: NewTask,
}

impl TaskReservation {
    /// The task identity reserved before any filesystem writes.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The deterministic yard root that attempts will use.
    pub fn work_root(&self) -> PathBuf {
        self.new
            .home
            .clone()
            .unwrap_or_else(super::home)
            .join("tasks")
            .join(&self.id)
            .join("work")
    }

    fn validate(&self) -> Result<(), Error> {
        if !valid_id(&self.id)
            || !self
                .id
                .bytes()
                .all(|byte| b"0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(&byte))
            || !self
                .new
                .home
                .as_ref()
                .is_some_and(|path| path.is_absolute())
        {
            return Err(Error::State(
                "invalid task initialization reservation".into(),
            ));
        }
        Ok(())
    }

    /// Inspect a completed initialization without creating directories or
    /// changing branches. `None` means initialization can be resumed.
    pub fn inspect(&self) -> Result<Option<Task>, Error> {
        self.validate()?;
        let work = self.work_root();
        let intent = work
            .parent()
            .ok_or_else(|| Error::State("task yard has no parent".into()))?
            .join("initialize/intent.json");
        if let Some(existing) = super::read_json::<TaskReservation>(&intent)? {
            if existing != *self {
                return Err(Error::State(format!(
                    "task {} has a different initialization reservation",
                    self.id
                )));
            }
        }
        super::load(&work, &self.id)
    }
}

/// Reserve a task identity and deterministic location without filesystem writes.
///
/// Folder grants are validated and paths are made absolute. Persist the entire
/// returned reservation before executing initialization; calling `reserve`
/// again creates a different identity and is not a retry.
pub fn reserve(new: &NewTask) -> Result<TaskReservation, Error> {
    Ok(TaskReservation {
        id: super::new_id()?,
        created_ms: branchyard_support::time::now_ms(),
        new: folder::validated(new)?,
    })
}

/// Initialize or resume the task at its reserved location.
///
/// Initialization is serialized by an operating-system lock and compares the
/// immutable local intent with the durable reservation. Completed tasks return
/// unchanged, including their attempts and native conversations. Partial
/// initialization remains inspectable and is completed at the same location;
/// this function never executes a model turn.
pub fn initialize_reserved(reservation: &TaskReservation) -> Result<(Task, Yard), Error> {
    reservation.validate()?;
    folder::initialize(reservation)
}

/// Reserve a task record for attempts in an already registered repository.
///
/// This allocates an identity without touching the repository. Persist the
/// returned record with queue admission, then call `initialize_repository`.
pub fn reserve_repository(prompt: &str, by: &str, policy: &str) -> Result<Task, Error> {
    Ok(Task {
        id: super::new_id()?,
        title: super::title(prompt),
        asked: prompt.to_owned(),
        by: by.to_owned(),
        policy: policy.to_owned(),
        created_ms: branchyard_support::time::now_ms(),
        files: super::TaskFiles::Repository,
        origin: "task".into(),
    })
}

/// Publish a reserved repository task exactly once, without starting a turn.
///
/// Replaying the original reservation returns the current task record, whose
/// first attempt may already have supplied its policy. A conflicting intent
/// with the same identity is rejected before changing the existing record.
/// Attempts join it through `TaskOptions::join_task`.
pub fn initialize_repository(yard: &Yard, reservation: &Task) -> Result<Task, Error> {
    if reservation.files != super::TaskFiles::Repository
        || !valid_id(&reservation.id)
        || super::folder::owned(yard.root()).is_some()
    {
        return Err(Error::State("invalid repository task reservation".into()));
    }
    let intent_dir = crate::state::dir(yard.root())
        .join("task-reservations")
        .join(&reservation.id);
    let _initializing = crate::DirLock::acquire(&intent_dir, "initializing this task record")?;
    let intent = intent_dir.join("intent.json");
    match super::read_json::<Task>(&intent)? {
        Some(existing) if existing != *reservation => {
            return Err(Error::State(format!(
                "task {} has a different initialization reservation",
                reservation.id
            )));
        }
        Some(_) => {}
        None => {
            if let Some(existing) = super::load(yard.root(), &reservation.id)? {
                if existing != *reservation {
                    return Err(Error::State(format!(
                        "task {} is already recorded with different metadata",
                        reservation.id
                    )));
                }
            }
            super::write_atomic(
                &intent,
                &serde_json::to_vec(reservation)
                    .map_err(|error| Error::State(error.to_string()))?,
            )?;
        }
    }
    if let Some(existing) = super::load(yard.root(), &reservation.id)? {
        return Ok(existing);
    }
    super::save(yard.root(), reservation)?;
    Ok(reservation.clone())
}

fn valid_id(id: &str) -> bool {
    id.len() == 26
        && id
            .bytes()
            .all(|byte| b"0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(&byte))
}
