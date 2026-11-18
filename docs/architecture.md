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

## Authentication and audit identity

The deployment bootstrap secret authenticates an emergency administrator that
can create named `viewer`, `curator`, and `admin` accounts. Named API tokens are
random, stored only as SHA-256 digests, and can expire or be revoked without a
restart. Each protected request resolves the current account from PostgreSQL;
curation requires the `curator` or `admin` role and account administration
requires `admin`. Audit attribution, review ownership, export requesters and ML
label ownership are derived from that authenticated principal. Client-supplied
identity fields are never trusted.

## Data ownership

PostgreSQL is canonical for metadata and curation. `/data/objects` is intended
for content-addressed acquired objects. `/data/derived` contains reproducible
extraction and ML outputs. Parquet exports live under `/data/parquet`; local
page and document embeddings use Zarr under `/data/zarr`. Neither is canonical.

The worker uses deterministic scikit-learn hashing vectors for document
similarity and character n-gram TF-IDF for work/discovery entity resolution.
PostgreSQL keeps the compact document vector and review queues; Zarr keeps the
reproducible page-level arrays. No vector database or remote model service is
part of the deployment.

Approved-proof Parquet exports are reproducible projections of PostgreSQL. Each
export is tracked with its object key, SHA-256 digest, row count, requester and
completion time. Restricted source text is never included in this projection.

The blob boundary is represented by relative object keys. Absolute paths and
parent traversal are rejected in both services.

## Queue

Workers claim jobs with `FOR UPDATE SKIP LOCKED`. Leases allow work to be
recovered following a container crash. Idempotency keys prevent duplicate work.
Failures use bounded retries and eventually enter `dead_letter`.

## Metrics history

PostgreSQL statement triggers capture corpus and queue counts into one
upserted snapshot per minute whenever relevant records change. These snapshots
preserve backlog history without storing source text and let the dashboard show
both cumulative research output and the changing amount of work awaiting human
review. The mechanism is database-local and does not require a telemetry SaaS.
The same current counts are exported in Prometheus text format at `/metrics`;
no source text, labels or high-cardinality work identifiers are emitted.

## Scheduled acquisition

Saved Crossref searches live in PostgreSQL with bounded result counts and a
minimum hourly cadence. The Rust process atomically claims due schedules with
row locking, advances their next-run timestamp before the network request and
records both successful coverage and failures in the audit trail. Only selected
bibliographic fields are retained. The Python worker remains network-agnostic
and performs only local processing.

## Scaling boundary

The local-path volume and single pod deliberately trade horizontal scaling for
simple homelab operation. Moving beyond one pod requires an object-store backend
and must not be attempted by mounting the node-local volume from another node.

