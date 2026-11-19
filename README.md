# Type Safety History

An evidence-backed history of published type-safety proofs. The Rust service is
the system of record; a Python sidecar performs local-only document processing
and ML. PostgreSQL stores canonical metadata and provenance, while a local
content-addressed volume stores permitted source documents and derived data.

The project intentionally treats completeness as a measurable coverage goal,
not as a claim. Machine-generated results are candidates until reviewed.

## Local development

Requirements: Docker with Compose.

```bash
cp .env.example .env
# Set a real research contact email and a long random curator token in .env.
docker compose up --build
curl http://localhost:8080/health/ready
```

The API is exposed on port `8080`; PostgreSQL is exposed on `5451`. Persistent
application data lives in the `app-data` volume mounted at `/data`.

`CURATOR_TOKEN` is the bootstrap administrator secret. Use it to create named
curator accounts and revocable API tokens, then reserve it for recovery:

```bash
curl -X POST http://localhost:8080/api/v1/admin/users \
  -H "Authorization: Bearer $CURATOR_TOKEN" \
  -H 'Content-Type: application/json' \
  --data '{"username":"alice","display_name":"Alice Curator","role":"curator"}'

curl -X POST http://localhost:8080/api/v1/admin/users/<user-id>/tokens \
  -H "Authorization: Bearer $CURATOR_TOKEN" \
  -H 'Content-Type: application/json' \
  --data '{"label":"workstation","expires_at":null}'
```

The generated token is shown once and stored only as a SHA-256 digest. Accounts
have `viewer`, `curator`, or `admin` roles and can be deactivated; individual
tokens can expire or be revoked immediately. `GET /api/v1/auth/me` identifies
the current principal. Audit identity is always taken from that authenticated
principal. Legacy `actor`, `labelled_by`, and `requested_by` request fields are
accepted for compatibility but ignored.

Viewer accounts can inspect protected research and review queues but cannot
change them. Curators can review and annotate scholarship; administrators can
also manage accounts and token metadata. The searchable audit trail is exposed
at `GET /api/v1/audit`.

For host-side checks:

```bash
cargo test --workspace
uv sync --project ml --all-groups
uv run --project ml pytest
```

## Deployment

Kubernetes uses separate API and worker `Deployment` resources. They share one
local-path PVC and PostgreSQL, while retaining independent pod lifecycles. CNPG
is reconciled independently:

```bash
kubectl apply -k db
kubectl apply -k deploy
```

Production releases use Git tags. A successful `vMAJOR.MINOR.PATCH` build
publishes the API and ML image variants to the single
`sthomasch/type-safety-history` Docker Hub repository, then deploys that exact
version pair to the protected `production` GitHub environment through
Headscale. See [`docs/operations.md`](docs/operations.md) for the required
repository/environment settings and the manual recovery path.

## Crossref DOI import

Imports require curator authentication and a configured Crossref contact email.
The adapter uses Crossref's versioned REST endpoint and polite-pool etiquette,
caches selected bibliographic metadata, and deliberately excludes abstracts and
full text.

```bash
curl -X POST http://localhost:8080/api/v1/imports/crossref \
  -H "Authorization: Bearer $CURATOR_TOKEN" \
  -H 'Content-Type: application/json' \
  --data '{"doi":"10.1006/inco.1994.1093"}'
```

Database migrations install the two curated anchors and five enabled Crossref
discovery schedules. On first startup the schedules request up to 1,000 metadata
records each; subsequent runs are weekly and remain deduplicated in PostgreSQL.
The equivalent DOI fixture is retained in
[`fixtures/seed-works.json`](fixtures/seed-works.json) and can be refreshed
without network access:

```bash
docker compose run --rm api seed /app/fixtures/seed-works.json
```

The loader is idempotent and creates discovery candidates rather than silently
promoting seed records into canonical scholarship.

After a backup restore, verify database-to-volume consistency with:

```bash
docker compose run --rm api verify-storage
```

This checks every active source object and successful Parquet export against its
SHA-256 digest, confirms Zarr directories exist, and verifies that taken-down
objects have not reappeared.

## Research and historical curation

The research model distinguishes bibliographic works from versions, relations,
formal artifacts, citations and individual proof results. Proof results can be
classified by language feature and type-system family. Corrections,
retractions, disputes, counterexamples and priority claims are first-class,
evidence-linked annotations; scholarly status is therefore not inferred from a
single machine score.

`GET /api/v1/works/{id}/research` returns the aggregate research record.
Curator endpoints create versions, work relations, citations, formal artifacts,
feature/type-system assignments and research annotations. Coverage runs retain
the exact provider, query, year interval, requested/result counts and adapter
configuration, so corpus completeness can be measured and reproduced.

## Corpus discovery

Curators can search Crossref bibliographic metadata without importing results:

```bash
curl -X POST http://localhost:8080/api/v1/discovery/crossref \
  -H "Authorization: Bearer $CURATOR_TOKEN" \
  -H 'Content-Type: application/json' \
  --data '{"query":"type soundness progress preservation","from_year":1990,"until_year":2026,"rows":20}'
```

The adapter asks Crossref for selected fields only, stores pending candidates,
and records every query hit. The network-independent Python worker assigns each
pending record a reproducible `likely_relevant`, `possible`, or `unlikely`
ranking from its title and discovery-query labels. High-confidence records are
automatically promoted to unreviewed candidate works, obvious noise is moved to
the reversible `screened_out` state, and only ambiguous records remain pending
in `/curate`. This automation never approves a proof or historical claim.
`GET /api/v1/discovery/candidates` exposes every disposition and its score;
curators can still import or reject an ambiguous record explicitly.

Recurring, licence-compliant Crossref searches can be created through
`POST /api/v1/discovery/schedules`. The Rust service claims due schedules,
records every coverage run, and queues local relevance triage and entity
resolution. Schedules have a minimum hourly cadence and can be disabled or run
manually from `/curate`.

When `OPENALEX_API_KEY` is configured, curators can also search selected
OpenAlex bibliographic metadata through `POST /api/v1/discovery/openalex`.
OpenAlex metadata is retained under CC0; abstracts, full-text URLs and document
content are deliberately excluded by the adapter.

## Local PDF ingestion

PDF acquisition is curator-driven. The upload must include its source URL,
reliability, rights status, a concrete rights basis, and explicit ML and
redistribution permissions. Metadata must be the first multipart field so the
service can reject disallowed content before accepting file bytes.

```bash
curl -X POST http://localhost:8080/api/v1/documents/upload \
  -H "Authorization: Bearer $CURATOR_TOKEN" \
  -F 'metadata={"work_id":"<uuid>","source_url":"https://repository.example/paper","source_kind":"institutional_repository","reliability":"primary","rights_status":"open_redistributable","license_name":"CC BY 4.0","license_url":"https://creativecommons.org/licenses/by/4.0/","rights_basis":"Repository record explicitly identifies CC BY 4.0.","ml_use_permitted":true,"redistribution_permitted":true};type=application/json' \
  -F 'file=@paper.pdf;type=application/pdf'
```

The service streams the file, enforces a 50 MiB limit, validates PDF magic,
computes SHA-256, and atomically stores it under `/data/objects/sha256/`.
The local Python worker extracts permitted documents and creates page-level
machine candidates for curator review. It also writes deterministic page and
document embeddings to local Zarr storage, records document similarity, and
extracts DOI citation candidates. Scanned pages use local Tesseract OCR as a
fallback; no document content is sent to a hosted model or OCR service.

## Local ML review

All ML output remains a suggestion. Curator-protected endpoints expose the
queues and their audit-preserving resolution actions:

- `GET /api/v1/citation-candidates`
- `GET /api/v1/entity-resolution-suggestions`
- `GET /api/v1/duplicate-suggestions`
- `GET /api/v1/documents/{id}/similarities`
- `POST /api/v1/ml/labels` and `POST /api/v1/ml/evaluations`

Creating works automatically queues duplicate detection; Crossref discovery
automatically queues entity resolution. Manual reruns are available through
`POST /api/v1/ml/jobs/entity-resolution` and
`POST /api/v1/ml/jobs/duplicate-detection`.

## Corpus dashboard

Open `http://localhost:8080/dashboard` for a read-only view of current corpus
state. It shows the discovery funnel, canonical works, machine candidates,
approved proofs, review queues, queue health and cumulative growth during the
last year. The page refreshes every minute and does not expose source text.

The same data is available as JSON:

- `GET /api/v1/stats/overview`
- `GET /api/v1/stats/evolution?days=365`

Evolution includes cumulative discovery/research events and durable
minute-bucketed snapshots of pending discovery, proof-review and worker queues.
The overview remains authoritative for the current instant.

`/curate` is the authenticated browser workspace for proof, discovery,
duplicate, entity and citation review, work inspection, saved searches, audit
history and account administration. It stores the bearer token only in the
browser tab's session storage and loads no third-party JavaScript. API
documentation is available at `/docs` and `/api/openapi.json`; Prometheus text
metrics are available at `/metrics`.

## Lifecycle operations

`POST /api/v1/documents/{id}/takedown` performs an authenticated, audited and
idempotent takedown. It tombstones the document record, disables processing and
redistribution, rejects pending machine candidates, stops extraction jobs and
removes source/derived objects while retaining provenance.

Accepting a duplicate suggestion now performs a transactional consolidation:
dependent documents, proofs, versions, authors, sources, citations, relations,
annotations and artifacts move to the selected canonical work. The duplicate
work remains as an audited redirect tombstone.

## Timeline and exports

`GET /api/v1/timeline` returns only curator-approved proof records. Curators can
create an approved record from a primary-source document using
`POST /api/v1/proof-results/curated`; exact page evidence, a paraphrase, actor,
and review reason are mandatory.

`POST /api/v1/exports` queues a local Parquet export. The sidecar writes a
checksummed Zstandard-compressed file under `/data/parquet/approved-proofs/`.
Exports contain bibliographic metadata, proof classifications, paraphrases and
evidence coordinates, but no extracted PDF text.

`GET /api/v1/ml/pipeline-runs` exposes the exact local pipeline and library
versions used for reproducibility. Curators can queue all active, ML-permitted
documents for a fresh deterministic pass with
`POST /api/v1/ml/jobs/reprocess-documents`; existing reviewed assertions are
not silently replaced.

## Policies

- [`docs/corpus-policy.md`](docs/corpus-policy.md)
- [`docs/source-and-rights-policy.md`](docs/source-and-rights-policy.md)
- [`docs/architecture.md`](docs/architecture.md)
- [`docs/ml-dependencies.md`](docs/ml-dependencies.md)

