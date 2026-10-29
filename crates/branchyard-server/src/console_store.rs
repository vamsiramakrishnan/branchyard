//! Transactional console metadata beside the existing durable operation queue.
//!
//! These records contain presentation, ownership and allocation intent only.
//! Native task records remain authoritative for execution and conversations.

use std::io;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A console metadata collection with an explicit, stable storage name.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    strum::Display,
    strum::EnumString,
    strum::EnumIter,
    strum::IntoStaticStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum Collection {
    /// Project source and result policy.
    Projects,
    /// Workspace identity and task allocation intent.
    Workspaces,
    /// Named bindings to native attempts.
    Sessions,
    /// Subject membership and role.
    Members,
    /// Attributed review comments.
    Comments,
    /// Expiring advisory presence.
    Presence,
    /// Immutable uploaded source provenance.
    Sources,
    /// Immutable original bytes, separated so metadata lists do not load blobs.
    SourceBlobs,
    /// Resource ownership bindings, including legacy routes.
    Bindings,
    /// Accepted immutable result identities.
    Acceptance,
    /// Idempotency for synchronous metadata mutations.
    Idempotency,
}

impl Collection {
    /// The SQL collection discriminator.
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// A tenant-owned console record. Its revision protects concurrent edits.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Record {
    /// The kind of metadata.
    pub collection: Collection,
    /// Stable collection-local identity, never a client filesystem path.
    pub id: String,
    /// Owning tenant, immutable after creation.
    pub tenant: String,
    /// Monotonic compare-and-set revision, beginning at one.
    pub revision: u64,
    /// The collection's domain record, without transcript copies.
    pub body: Value,
}

/// A checked metadata change applied atomically with every other change.
#[derive(Clone, Debug, PartialEq)]
pub enum Mutation {
    /// Insert when `expected` is absent; otherwise replace exactly that revision.
    Put {
        /// New record, with revision one or expected plus one.
        record: Record,
        /// Required existing revision, or absence for creation.
        expected: Option<u64>,
    },
    /// Delete exactly the named tenant-owned revision.
    Delete {
        /// Collection to change.
        collection: Collection,
        /// Stable record identity.
        id: String,
        /// Required owner.
        tenant: String,
        /// Required existing revision.
        expected: u64,
    },
}

impl Mutation {
    pub(crate) fn identity(&self) -> (Collection, &str) {
        match self {
            Self::Put { record, .. } => (record.collection, &record.id),
            Self::Delete { collection, id, .. } => (*collection, id),
        }
    }

    pub(crate) fn check(&self, current: Option<&Record>) -> io::Result<()> {
        let (tenant, expected) = match self {
            Self::Put { record, expected } => {
                if record.id.is_empty()
                    || record.tenant.is_empty()
                    || record.revision
                        != expected
                            .unwrap_or(0)
                            .checked_add(1)
                            .ok_or_else(|| io::Error::other("console revision exhausted"))?
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "invalid console record identity or revision",
                    ));
                }
                (&record.tenant, *expected)
            }
            Self::Delete {
                tenant, expected, ..
            } => (tenant, Some(*expected)),
        };
        match (expected, current) {
            (None, None) => Ok(()),
            (Some(expected), Some(current))
                if current.revision == expected && &current.tenant == tenant =>
            {
                Ok(())
            }
            _ => Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "console metadata revision conflict",
            )),
        }
    }
}

pub(crate) fn sqlite_get(
    conn: &rusqlite::Connection,
    collection: Collection,
    id: &str,
) -> io::Result<Option<Record>> {
    use rusqlite::OptionalExtension;
    let body: Option<String> = conn
        .query_row(
            "SELECT body FROM console_records WHERE collection=?1 AND id=?2",
            [collection.as_str(), id],
            |r| r.get(0),
        )
        .optional()
        .map_err(io::Error::other)?;
    body.map(|body| serde_json::from_str(&body).map_err(io::Error::other))
        .transpose()
}

pub(crate) fn sqlite_list(
    conn: &rusqlite::Connection,
    collection: Collection,
    tenant: &str,
) -> io::Result<Vec<Record>> {
    let mut stmt = conn
        .prepare("SELECT body FROM console_records WHERE collection=?1 AND tenant=?2 ORDER BY id")
        .map_err(io::Error::other)?;
    let rows = stmt
        .query_map([collection.as_str(), tenant], |r| r.get::<_, String>(0))
        .map_err(io::Error::other)?;
    rows.map(|row| serde_json::from_str(&row.map_err(io::Error::other)?).map_err(io::Error::other))
        .collect()
}

pub(crate) fn sqlite_list_all(
    conn: &rusqlite::Connection,
    collection: Collection,
) -> io::Result<Vec<Record>> {
    let mut stmt = conn
        .prepare("SELECT body FROM console_records WHERE collection=?1 ORDER BY id")
        .map_err(io::Error::other)?;
    let rows = stmt
        .query_map([collection.as_str()], |r| r.get::<_, String>(0))
        .map_err(io::Error::other)?;
    rows.map(|row| serde_json::from_str(&row.map_err(io::Error::other)?).map_err(io::Error::other))
        .collect()
}

pub(crate) fn sqlite_mutate(conn: &rusqlite::Connection, mutations: &[Mutation]) -> io::Result<()> {
    for mutation in mutations {
        let (collection, id) = mutation.identity();
        mutation.check(sqlite_get(conn, collection, id)?.as_ref())?;
        match mutation {
            Mutation::Put { record, .. } => {
                conn.execute("INSERT INTO console_records(collection,id,tenant,revision,body) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(collection,id) DO UPDATE SET revision=excluded.revision,body=excluded.body", rusqlite::params![collection.as_str(),id,record.tenant,branchyard::store_codec::to_db("console revision", record.revision)?,serde_json::to_string(record)?]).map_err(io::Error::other)?;
            }
            Mutation::Delete { .. } => {
                conn.execute(
                    "DELETE FROM console_records WHERE collection=?1 AND id=?2",
                    [collection.as_str(), id],
                )
                .map_err(io::Error::other)?;
            }
        }
    }
    Ok(())
}

#[cfg(feature = "postgres")]
pub(crate) fn pg_get(
    client: &mut impl postgres::GenericClient,
    collection: Collection,
    id: &str,
) -> Result<io::Result<Option<Record>>, postgres::Error> {
    let row = client.query_opt(
        "SELECT body FROM by_console_records WHERE collection=$1 AND id=$2",
        &[&collection.as_str(), &id],
    )?;
    Ok(row
        .map(|row| serde_json::from_str(&row.get::<_, String>(0)).map_err(io::Error::other))
        .transpose())
}

#[cfg(feature = "postgres")]
pub(crate) fn pg_mutate(
    tx: &mut postgres::Transaction<'_>,
    mutations: &[Mutation],
) -> Result<io::Result<()>, postgres::Error> {
    if mutations.is_empty() {
        return Ok(Ok(()));
    }
    // One lock serializes metadata CAS across rows and prevents cross-row deadlocks.
    tx.execute("SELECT pg_advisory_xact_lock(7390184327)", &[])?;
    for mutation in mutations {
        let (collection, id) = mutation.identity();
        let current = match pg_get(tx, collection, id)? {
            Ok(current) => current,
            Err(e) => return Ok(Err(e)),
        };
        if let Err(e) = mutation.check(current.as_ref()) {
            return Ok(Err(e));
        }
        match mutation {
            Mutation::Put { record, .. } => {
                let revision =
                    match branchyard::store_codec::to_db("console revision", record.revision) {
                        Ok(revision) => revision,
                        Err(e) => return Ok(Err(e.into())),
                    };
                let body = match serde_json::to_string(record) {
                    Ok(body) => body,
                    Err(e) => return Ok(Err(e.into())),
                };
                tx.execute("INSERT INTO by_console_records(collection,id,tenant,revision,body) VALUES($1,$2,$3,$4,$5) ON CONFLICT(collection,id) DO UPDATE SET revision=excluded.revision,body=excluded.body", &[&collection.as_str(),&id,&record.tenant,&revision,&body])?;
            }
            Mutation::Delete { .. } => {
                tx.execute(
                    "DELETE FROM by_console_records WHERE collection=$1 AND id=$2",
                    &[&collection.as_str(), &id],
                )?;
            }
        }
    }
    Ok(Ok(()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{
        Admission, AdmissionQuota, Idempotency, MemoryStore, OperationStore, SqliteStore,
        StoredOperation,
    };
    use branchyard_client::api::{Operation, OperationKind, OperationState};
    use serde_json::json;

    fn record(id: &str) -> Record {
        Record {
            collection: Collection::Workspaces,
            id: id.into(),
            tenant: "tenant-a".into(),
            revision: 1,
            body: json!({"title":"Research"}),
        }
    }

    fn operation(id: &str, key: &str) -> StoredOperation {
        StoredOperation {
            operation: Operation {
                id: id.into(),
                repo: "managed".into(),
                kind: OperationKind::Task,
                state: OperationState::Queued,
                branches: vec![],
                cursor: 0,
                end_cursor: None,
                created_at_ms: 1,
                finished_at_ms: None,
                result: None,
                error: None,
                requires: vec![],
                waiting: None,
                priority: 0,
            },
            idempotency: Some(Idempotency {
                caller: "tenant-a:human".into(),
                key: key.into(),
                fingerprint: "same-request".into(),
            }),
            locks: vec![],
            tenant: "tenant-a".into(),
            principal: None,
            creates: vec![],
            trace: None,
        }
    }

    fn conformance(store: &dyn OperationStore) {
        let put = Mutation::Put {
            record: record("w"),
            expected: None,
        };
        assert_eq!(
            store
                .admit_console(
                    &operation("op1", "create"),
                    &json!({"allocation":"w"}),
                    &AdmissionQuota::default(),
                    std::slice::from_ref(&put)
                )
                .unwrap(),
            Admission::Admitted
        );
        assert_eq!(store.queue().unwrap().len(), 1);
        assert_eq!(
            store.console_get(Collection::Workspaces, "w").unwrap(),
            Some(record("w"))
        );
        assert!(store
            .console_list(Collection::Workspaces, "tenant-b")
            .unwrap()
            .is_empty());
        // Replayed admission never applies the new caller's metadata batch.
        assert!(matches!(
            store
                .admit_console(
                    &operation("op2", "create"),
                    &json!({}),
                    &AdmissionQuota::default(),
                    &[Mutation::Put {
                        record: record("orphan"),
                        expected: None
                    }]
                )
                .unwrap(),
            Admission::Replayed(_)
        ));
        assert!(store
            .console_get(Collection::Workspaces, "orphan")
            .unwrap()
            .is_none());
        // A failed CAS rolls back queue/operation/locks and every earlier row.
        let failure = store.admit_console(
            &operation("op3", "conflict"),
            &json!({}),
            &AdmissionQuota::default(),
            &[
                Mutation::Put {
                    record: record("rollback"),
                    expected: None,
                },
                put,
            ],
        );
        assert_eq!(failure.unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        assert!(store.get("op3").unwrap().is_none());
        assert!(store
            .console_get(Collection::Workspaces, "rollback")
            .unwrap()
            .is_none());
        let mut updated = record("w");
        updated.revision = 2;
        updated.body = json!({"title":"Edited"});
        store
            .console_mutate(&[Mutation::Put {
                record: updated.clone(),
                expected: Some(1),
            }])
            .unwrap();
        let mut stolen = updated.clone();
        stolen.revision = 3;
        stolen.tenant = "tenant-b".into();
        assert!(store
            .console_mutate(&[Mutation::Put {
                record: stolen,
                expected: Some(2)
            }])
            .is_err());
        assert_eq!(
            store.console_get(Collection::Workspaces, "w").unwrap(),
            Some(updated)
        );
        assert!(store
            .console_mutate(&[Mutation::Delete {
                collection: Collection::Workspaces,
                id: "w".into(),
                tenant: "tenant-a".into(),
                expected: 1
            }])
            .is_err());
        store
            .console_mutate(&[Mutation::Delete {
                collection: Collection::Workspaces,
                id: "w".into(),
                tenant: "tenant-a".into(),
                expected: 2,
            }])
            .unwrap();
        assert!(store
            .console_get(Collection::Workspaces, "w")
            .unwrap()
            .is_none());
    }

    #[test]
    fn sqlite_and_memory_console_transactions() {
        conformance(&SqliteStore::memory());
        conformance(&MemoryStore::default());
    }

    #[test]
    fn console_metadata_and_queue_survive_reopen() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.db");
        {
            let store = SqliteStore::open(&path, None).unwrap();
            store
                .admit_console(
                    &operation("op", "create"),
                    &json!({"allocation":"reserved"}),
                    &AdmissionQuota::default(),
                    &[Mutation::Put {
                        record: record("reserved"),
                        expected: None,
                    }],
                )
                .unwrap();
        }
        let reopened = SqliteStore::open(path, None).unwrap();
        assert_eq!(
            reopened
                .console_get(Collection::Workspaces, "reserved")
                .unwrap(),
            Some(record("reserved"))
        );
        assert_eq!(reopened.queue().unwrap().len(), 1);
        assert!(matches!(
            reopened
                .admit_console(
                    &operation("retry", "create"),
                    &json!({}),
                    &AdmissionQuota::default(),
                    &[]
                )
                .unwrap(),
            Admission::Replayed(_)
        ));
    }

    #[test]
    fn independent_sqlite_connections_replay_one_allocation() {
        use std::sync::{Arc, Barrier};
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.db");
        let left = SqliteStore::open(&path, None).unwrap();
        let right = SqliteStore::open(&path, None).unwrap();
        let barrier = Arc::new(Barrier::new(2));
        std::thread::scope(|scope| {
            let other = Arc::clone(&barrier);
            let first = scope.spawn(move || {
                other.wait();
                left.admit_console(
                    &operation("first", "same-key"),
                    &json!({"allocation":"first"}),
                    &AdmissionQuota::default(),
                    &[Mutation::Put {
                        record: record("first"),
                        expected: None,
                    }],
                )
                .unwrap()
            });
            barrier.wait();
            let second = right
                .admit_console(
                    &operation("second", "same-key"),
                    &json!({"allocation":"second"}),
                    &AdmissionQuota::default(),
                    &[Mutation::Put {
                        record: record("second"),
                        expected: None,
                    }],
                )
                .unwrap();
            let first = first.join().unwrap();
            assert!(matches!(
                (&first, &second),
                (Admission::Admitted, Admission::Replayed(_))
                    | (Admission::Replayed(_), Admission::Admitted)
            ));
        });
        let reopened = SqliteStore::open(&path, None).unwrap();
        assert_eq!(
            reopened
                .console_list(Collection::Workspaces, "tenant-a")
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            reopened
                .console_list_all(Collection::Workspaces)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(reopened.queue().unwrap().len(), 1);
    }

    #[test]
    fn control_claims_share_queue_fencing_and_do_not_take_normal_slots() {
        use crate::store::{Scheduling, Worker};
        use std::time::Duration;
        let store = SqliteStore::memory();
        let control = json!({"kind":"console","command":{"action":"control","control":"cancel","workspace_id":"w"}});
        store
            .admit(
                &operation("control", "control"),
                &control,
                &AdmissionQuota::default(),
            )
            .unwrap();
        store
            .admit(
                &operation("normal", "normal"),
                &json!({"kind":"task"}),
                &AdmissionQuota::default(),
            )
            .unwrap();
        let worker = Worker::current();
        let lease = Duration::from_secs(30);
        let scheduling = Scheduling::default();
        assert!(store
            .claim_control(&worker, &["other-repo".into()], &[], lease, &scheduling)
            .unwrap()
            .is_none());
        let normal = store
            .claim_next(&worker, &["managed".into()], &[], lease, &scheduling)
            .unwrap()
            .unwrap();
        assert_eq!(normal.operation.operation.id, "normal");
        let claimed = store
            .claim_control(&worker, &["managed".into()], &[], lease, &scheduling)
            .unwrap()
            .unwrap();
        assert_eq!(claimed.operation.operation.id, "control");
        assert!(crate::store::control_work(&claimed.work));
        assert!(!crate::store::control_work(&normal.work));
        assert!(store
            .claim_control(&worker, &["managed".into()], &[], lease, &scheduling)
            .unwrap()
            .is_none());
        store.release(&worker, "control", claimed.fence).unwrap();
        let renewed = store
            .claim_control(&worker, &["managed".into()], &[], lease, &scheduling)
            .unwrap()
            .unwrap();
        assert!(renewed.fence > claimed.fence);
        assert!(!store
            .renew(&worker, "control", claimed.fence, lease)
            .unwrap());
    }

    #[test]
    fn collection_storage_names_and_invalid_revisions() {
        use strum::IntoEnumIterator as _;
        for collection in Collection::iter() {
            assert_eq!(
                serde_json::to_value(collection).unwrap(),
                json!(collection.as_str())
            );
            assert_eq!(
                collection.as_str().parse::<Collection>().unwrap(),
                collection
            );
        }
        let mut invalid = record("w");
        invalid.revision = 0;
        assert!(Mutation::Put {
            record: invalid,
            expected: None
        }
        .check(None)
        .is_err());
        let mut exhausted = record("w");
        exhausted.revision = u64::MAX;
        assert!(Mutation::Put {
            record: exhausted,
            expected: Some(u64::MAX)
        }
        .check(None)
        .is_err());
    }
}
