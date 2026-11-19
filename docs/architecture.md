# Architecture

## Decision summary

The Rust API and Python worker use separate Kubernetes Deployments so a crash,
restart or rollout of one does not stop the other. They retain distinct health,
resources, users and dependency sets while sharing a local volume and
PostgreSQL. Each pod runs the idempotent SQLx migrations before its application
container starts, so schema availability does not depend on the API server
becoming healthy. CNPG is an independent data service, not another application
deployment.

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
Bibliographic candidates are additionally ranked by a versioned, deterministic
local metadata classifier. It promotes likely and possible records as
prioritized, unreviewed candidate works and moves obvious noise to a reversible
screened state. The Rust source resolver then looks up DOI-based open-access
locations and acquires only explicitly allowlisted, licensed PDFs. Proof results
and historical claims are published from the internal `machine_candidate`
state with an external `machine_classified` verification label. Optional
curation upgrades that label after a person verifies primary evidence.
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
row locking and a five-minute lease. It advances the regular cadence only after
the metadata transaction succeeds; a failed request retries after 15 minutes,
and a process killed mid-request is reclaimed when its lease expires. Coverage
and failures remain in the audit trail. Only selected bibliographic fields are
retained. The Python worker remains network-agnostic and performs only local
processing.

The bootstrap migration enables five complementary type-safety searches from
1950 through the current year. Each requests at most 1,000 records on its first
run and then refreshes weekly. Crossref requests are serialized, identified with
the configured research contact, cached through database upserts and exclude
abstracts and full text. This gives the initial deployment metadata throughput
without treating a discovery hit as a verified work or bypassing document
licensing review.

## Scaling boundary

The local-path volume deliberately trades horizontal scaling for simple homelab
operation. The API and worker pods must be scheduled on the volume's node;
moving either workload to another node or adding replicas requires an
object-store backend.

