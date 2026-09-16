-- Run with a migration role. Runtime role needs only data/queue access.
CREATE EXTENSION IF NOT EXISTS pgmq;
SELECT pgmq.create('branchyard_dispatch');

CREATE TABLE by_tenants (
    tenant text PRIMARY KEY,
    reserved_tasks bigint NOT NULL DEFAULT 0 CHECK (reserved_tasks >= 0),
    reserved_cpu bigint NOT NULL DEFAULT 0 CHECK (reserved_cpu >= 0),
    reserved_memory bigint NOT NULL DEFAULT 0 CHECK (reserved_memory >= 0)
);
CREATE TABLE by_operations (
    tenant text NOT NULL REFERENCES by_tenants(tenant),
    operation_id uuid NOT NULL,
    principal text NOT NULL,
    fingerprint text NOT NULL CHECK (length(fingerprint) = 64),
    operation jsonb NOT NULL,
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (tenant, operation_id)
);
CREATE TABLE by_roots (
    tenant text NOT NULL REFERENCES by_tenants(tenant),
    root_id uuid NOT NULL,
    graph jsonb NOT NULL,
    PRIMARY KEY (tenant, root_id)
);
CREATE TABLE by_tasks (
    tenant text NOT NULL,
    task_id uuid NOT NULL,
    root_id uuid NOT NULL,
    PRIMARY KEY (tenant, task_id),
    FOREIGN KEY (tenant, root_id) REFERENCES by_roots(tenant, root_id)
);
CREATE INDEX by_tasks_root ON by_tasks(tenant, root_id);
CREATE TABLE by_events (
    tenant text NOT NULL,
    task_id uuid NOT NULL,
    sequence bigint NOT NULL CHECK (sequence > 0),
    event jsonb NOT NULL,
    PRIMARY KEY (tenant, task_id, sequence),
    FOREIGN KEY (tenant, task_id) REFERENCES by_tasks(tenant, task_id)
);
-- Identities and operation records are retained indefinitely in this release.
-- Deleting an operation/task would allow a previously accepted identity to be reused.
