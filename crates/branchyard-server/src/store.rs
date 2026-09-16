use crate::{
    config::{Config, Principal},
    graph::Graph,
    Error, Result,
};
use branchyard_protocol::*;
use sqlx::{postgres::PgPoolOptions, PgPool, Row};
use std::sync::Arc;

#[derive(Clone)]
pub struct Store {
    pub(crate) pool: PgPool,
    pub(crate) config: Arc<Config>,
}
impl Store {
    pub async fn connect(database_url: &str, config: Config) -> Result<Self> {
        config.validate()?;
        let pool = PgPoolOptions::new()
            .max_connections(20)
            .acquire_timeout(std::time::Duration::from_secs(5))
            .connect(database_url)
            .await?;
        Ok(Self {
            pool,
            config: Arc::new(config),
        })
    }
    pub fn config(&self) -> &Config {
        &self.config
    }
    pub async fn migrate(pool: &PgPool) -> Result<()> {
        sqlx::migrate!("./migrations")
            .run(pool)
            .await
            .map_err(|_| Error::Corrupt)
    }
    pub async fn initialize(&self) -> Result<()> {
        for tenant in self.config.tenants.keys() {
            sqlx::query("INSERT INTO by_tenants(tenant) VALUES ($1) ON CONFLICT DO NOTHING")
                .bind(tenant)
                .execute(&self.pool)
                .await?;
        }
        // Refuse a missing queue rather than allowing graph writes without dispatch.
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pgmq.meta WHERE queue_name='branchyard_dispatch')",
        )
        .fetch_one(&self.pool)
        .await?;
        if !exists {
            return Err(Error::Corrupt);
        }
        Ok(())
    }
    /// Embedding callers must supply a verified principal. The HTTP adapter obtains it from Config.
    pub async fn submit(&self, principal: &Principal, command: &Command) -> Result<Receipt> {
        principal.allows(command.action.name())?;
        let fingerprint = command.fingerprint().map_err(|_| Error::Invalid)?;
        let policy = self
            .config
            .tenants
            .get(&principal.tenant)
            .ok_or(Error::Forbidden)?;
        let mut tx = self.pool.begin().await?;
        sqlx::query("SET LOCAL statement_timeout = '10s'")
            .execute(&mut *tx)
            .await?;
        sqlx::query("SET LOCAL lock_timeout = '5s'")
            .execute(&mut *tx)
            .await?;
        // One admission lock per tenant serializes quota arithmetic and identity races.
        // No model/runtime IO happens in this transaction. Different tenants remain independent.
        let counters = sqlx::query("SELECT reserved_tasks, reserved_cpu, reserved_memory FROM by_tenants WHERE tenant=$1 FOR UPDATE")
            .bind(&principal.tenant).fetch_optional(&mut *tx).await?.ok_or(Error::Forbidden)?;
        if let Some(row) = sqlx::query(
            "SELECT principal, fingerprint FROM by_operations WHERE tenant=$1 AND operation_id=$2",
        )
        .bind(&principal.tenant)
        .bind(command.operation_id.0)
        .fetch_optional(&mut *tx)
        .await?
        {
            if row.get::<String, _>("principal") != principal.subject {
                return Err(Error::Forbidden);
            }
            if row.get::<String, _>("fingerprint") != fingerprint {
                return Err(Error::Conflict);
            }
            // Replay is scoped to the same authenticated principal and its original task envelope.
            let existing: serde_json::Value = sqlx::query_scalar(
                "SELECT operation FROM by_operations WHERE tenant=$1 AND operation_id=$2",
            )
            .bind(&principal.tenant)
            .bind(command.operation_id.0)
            .fetch_one(&mut *tx)
            .await?;
            let operation: Operation =
                serde_json::from_value(existing).map_err(|_| Error::Corrupt)?;
            for id in &operation.task_ids {
                self.authorize_tx(&mut tx, principal, *id).await?;
            }
            tx.commit().await?;
            return Ok(Receipt {
                schema: Version::V1Alpha1,
                operation_id: command.operation_id,
                request_sha256: fingerprint,
                disposition: Disposition::Replay,
            });
        }
        let (before, after) = match &command.action {
            Action::CreateTask { task_id, spec } => {
                if principal.subtree.is_some() {
                    return Err(Error::Forbidden);
                }
                let exists: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM by_tasks WHERE tenant=$1 AND task_id=$2)",
                )
                .bind(&principal.tenant)
                .bind(task_id.0)
                .fetch_one(&mut *tx)
                .await?;
                if exists {
                    return Err(Error::Conflict);
                }
                (None, Graph::create(*task_id, spec.clone(), policy)?)
            }
            Action::ApplyGraph {
                root_id,
                expected_revision,
                edits,
            } => {
                let graph = self.load_tx(&mut tx, principal, *root_id).await?;
                if graph.root != *root_id {
                    return Err(Error::Invalid);
                }
                // A delegated principal may propose changes only to its subtree, not the root task.
                if let Some(scope) = principal.subtree {
                    graph.authorize(principal, scope)?;
                }
                let next = graph.apply(principal, *expected_revision, edits, policy)?;
                (Some(graph), next)
            }
            Action::CancelTask {
                task_id,
                expected_revision,
                cascade,
            } => {
                let graph = self.load_tx(&mut tx, principal, *task_id).await?;
                let next = graph.cancel(principal, *task_id, *expected_revision, *cascade)?;
                (Some(graph), next)
            }
        };
        let old = before.as_ref().map(Graph::reservation).unwrap_or_default();
        let new = after.reservation();
        fn adjust(current: i64, old: u64, new: u64) -> Result<u64> {
            u64::try_from(current)
                .ok()
                .and_then(|n| n.checked_sub(old))
                .and_then(|n| n.checked_add(new))
                .ok_or(Error::Corrupt)
        }
        let tasks = adjust(counters.get("reserved_tasks"), old.0, new.0)?;
        let cpu = adjust(counters.get("reserved_cpu"), old.1, new.1)?;
        let memory = adjust(counters.get("reserved_memory"), old.2, new.2)?;
        if tasks > policy.max_reserved_tasks as u64
            || cpu > policy.max_reserved_cpu_millis
            || memory > policy.max_reserved_memory_mib
            || new.1 > policy.max_root_cpu_millis
            || new.2 > policy.max_root_memory_mib
        {
            return Err(Error::Capacity);
        }
        sqlx::query("INSERT INTO by_roots(tenant,root_id,graph) VALUES($1,$2,$3) ON CONFLICT (tenant,root_id) DO UPDATE SET graph=EXCLUDED.graph")
            .bind(&principal.tenant).bind(after.root.0).bind(serde_json::to_value(&after).map_err(|_| Error::Corrupt)?).execute(&mut *tx).await?;
        let mut affected = Vec::new();
        for (id, node) in &after.nodes {
            let prior = before.as_ref().and_then(|g| g.nodes.get(id));
            if prior.is_none() {
                let inserted = sqlx::query("INSERT INTO by_tasks(tenant,task_id,root_id) VALUES($1,$2,$3) ON CONFLICT DO NOTHING")
                    .bind(&principal.tenant).bind(id.0).bind(after.root.0).execute(&mut *tx).await?.rows_affected();
                if inserted != 1 {
                    return Err(Error::Conflict);
                }
            }
            if prior.is_none_or(|p| p.task != node.task) {
                affected.push(*id);
                let event = Event {
                    sequence: node.task.revision,
                    kind: command.action.name().into(),
                    summary: format!("Task state: {:?}", node.task.state),
                    artifacts: vec![],
                };
                sqlx::query(
                    "INSERT INTO by_events(tenant,task_id,sequence,event) VALUES($1,$2,$3,$4)",
                )
                .bind(&principal.tenant)
                .bind(id.0)
                .bind(i64::try_from(event.sequence).map_err(|_| Error::Capacity)?)
                .bind(serde_json::to_value(event).map_err(|_| Error::Corrupt)?)
                .execute(&mut *tx)
                .await?;
            }
        }
        sqlx::query("UPDATE by_tenants SET reserved_tasks=$2,reserved_cpu=$3,reserved_memory=$4 WHERE tenant=$1")
            .bind(&principal.tenant).bind(i64::try_from(tasks).map_err(|_| Error::Capacity)?).bind(i64::try_from(cpu).map_err(|_| Error::Capacity)?).bind(i64::try_from(memory).map_err(|_| Error::Capacity)?).execute(&mut *tx).await?;
        // Scope the operation receipt to explicitly addressed tasks, not unrelated sibling revision bumps.
        let task_ids = match &command.action {
            Action::CreateTask { task_id, .. } | Action::CancelTask { task_id, .. } => {
                vec![*task_id]
            }
            Action::ApplyGraph { edits, .. } => edits
                .iter()
                .map(|e| match e {
                    GraphEdit::Spawn { task_id, .. }
                    | GraphEdit::AddDependency { task_id, .. }
                    | GraphEdit::RemoveDependency { task_id, .. } => *task_id,
                })
                .collect(),
        };
        let operation = Operation {
            schema: Version::V1Alpha1,
            operation_id: command.operation_id,
            request_sha256: fingerprint.clone(),
            state: OperationState::Succeeded,
            task_ids,
        };
        sqlx::query("INSERT INTO by_operations(tenant,operation_id,principal,fingerprint,operation) VALUES($1,$2,$3,$4,$5)")
            .bind(&principal.tenant).bind(command.operation_id.0).bind(&principal.subject).bind(&fingerprint).bind(serde_json::to_value(operation).map_err(|_| Error::Corrupt)?).execute(&mut *tx).await?;
        let message = serde_json::json!({"schema":"branchyard/dispatch/v1alpha1", "tenant":principal.tenant, "root_id":after.root, "operation_id":command.operation_id, "action":command.action.name(), "task_ids":affected});
        sqlx::query("SELECT pgmq.send('branchyard_dispatch', $1::jsonb)")
            .bind(message)
            .execute(&mut *tx)
            .await?;
        // A commit error is always 5xx: it can mean the commit happened but its ACK was lost.
        tx.commit().await?;
        Ok(Receipt {
            schema: Version::V1Alpha1,
            operation_id: command.operation_id,
            request_sha256: fingerprint,
            disposition: Disposition::Accepted,
        })
    }
    async fn load_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        principal: &Principal,
        id: TaskId,
    ) -> Result<Graph> {
        let value: serde_json::Value = sqlx::query_scalar("SELECT r.graph FROM by_roots r JOIN by_tasks t ON r.tenant=t.tenant AND r.root_id=t.root_id WHERE t.tenant=$1 AND t.task_id=$2")
            .bind(&principal.tenant).bind(id.0).fetch_optional(&mut **tx).await?.ok_or(Error::NotFound)?;
        serde_json::from_value(value).map_err(|_| Error::Corrupt)
    }
    async fn authorize_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        principal: &Principal,
        id: TaskId,
    ) -> Result<Graph> {
        let graph = self.load_tx(tx, principal, id).await?;
        graph.authorize(principal, id)?;
        Ok(graph)
    }
    pub async fn operation(&self, principal: &Principal, id: OperationId) -> Result<Operation> {
        principal.allows("read")?;
        let mut tx = self.pool.begin().await?;
        let value: serde_json::Value = sqlx::query_scalar("SELECT operation FROM by_operations WHERE tenant=$1 AND principal=$2 AND operation_id=$3")
            .bind(&principal.tenant).bind(&principal.subject).bind(id.0).fetch_optional(&mut *tx).await?.ok_or(Error::NotFound)?;
        let operation: Operation = serde_json::from_value(value).map_err(|_| Error::Corrupt)?;
        for id in &operation.task_ids {
            self.authorize_tx(&mut tx, principal, *id).await?;
        }
        tx.commit().await?;
        Ok(operation)
    }
    pub async fn task(&self, principal: &Principal, id: TaskId) -> Result<Task> {
        principal.allows("read")?;
        let mut tx = self.pool.begin().await?;
        let graph = self.authorize_tx(&mut tx, principal, id).await?;
        tx.commit().await?;
        Ok(graph.nodes[&id].task.clone())
    }
    pub async fn events(
        &self,
        principal: &Principal,
        id: TaskId,
        after: u64,
        limit: u16,
    ) -> Result<EventPage> {
        principal.allows("read")?;
        if limit == 0 || limit > MAX_EVENT_PAGE {
            return Err(Error::Invalid);
        }
        let after_sql = i64::try_from(after).map_err(|_| Error::Invalid)?;
        let mut tx = self.pool.begin().await?;
        let graph = self.authorize_tx(&mut tx, principal, id).await?;
        if after > graph.nodes[&id].task.revision {
            return Err(Error::Conflict);
        }
        let rows: Vec<serde_json::Value> = sqlx::query_scalar("SELECT event FROM by_events WHERE tenant=$1 AND task_id=$2 AND sequence>$3 ORDER BY sequence LIMIT $4")
            .bind(&principal.tenant).bind(id.0).bind(after_sql).bind(i64::from(limit)).fetch_all(&mut *tx).await?;
        let events: Vec<Event> = rows
            .into_iter()
            .map(|v| serde_json::from_value(v).map_err(|_| Error::Corrupt))
            .collect::<Result<_>>()?;
        let next_after = events.last().map(|e| e.sequence).unwrap_or(after);
        tx.commit().await?;
        Ok(EventPage {
            schema: Version::V1Alpha1,
            task_id: id,
            events,
            next_after,
        })
    }
}
