# Architecture

## Decision summary

The application is one Kubernetes Deployment and one pod. Its Rust API and
Python worker are separate containers so they retain distinct health,
resources, users and dependency sets. They share a local volume and PostgreSQL.
CNPG is an independent data service, not another application deployment.

Rust owns canonical writes, validation, provenance and rights enforcement.
Python claims durable jobs from PostgreSQL and emits derived candidates. It
does not access hosted inference services and must never promote a candidate to
a canonical scholarly assertion.

## Data ownership

PostgreSQL is canonical for metadata and curation. `/data/objects` is intended
for content-addressed acquired objects. `/data/derived` contains reproducible
extraction and ML outputs. Parquet exports live under `/data/parquet`; future
embedding arrays belong under `/data/zarr`. Neither is canonical.

Approved-proof Parquet exports are reproducible projections of PostgreSQL. Each
export is tracked with its object key, SHA-256 digest, row count, requester and
completion time. Restricted source text is never included in this projection.

The blob boundary is represented by relative object keys. Absolute paths and
parent traversal are rejected in both services.

## Queue

Workers claim jobs with `FOR UPDATE SKIP LOCKED`. Leases allow work to be
recovered following a container crash. Idempotency keys prevent duplicate work.
Failures use bounded retries and eventually enter `dead_letter`.

## Scaling boundary

The local-path volume and single pod deliberately trade horizontal scaling for
simple homelab operation. Moving beyond one pod requires an object-store backend
and must not be attempted by mounting the node-local volume from another node.

