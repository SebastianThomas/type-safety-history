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

For host-side checks:

```bash
cargo test --workspace
uv sync --project ml --all-groups
uv run --project ml pytest
```

## Deployment

Kubernetes uses one application `Deployment` with two containers (Rust API and
Python worker) sharing one local-path PVC. CNPG is reconciled independently:

```bash
kubectl apply -k db
kubectl apply -k deploy
```

Set the real image names and public hostname before deployment. See
[`docs/operations.md`](docs/operations.md).

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

The initial DOI queue is in [`fixtures/seed-works.json`](fixtures/seed-works.json).

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
machine candidates for curator review.

## Timeline and exports

`GET /api/v1/timeline` returns only curator-approved proof records. Curators can
create an approved record from a primary-source document using
`POST /api/v1/proof-results/curated`; exact page evidence, a paraphrase, actor,
and review reason are mandatory.

`POST /api/v1/exports` queues a local Parquet export. The sidecar writes a
checksummed Zstandard-compressed file under `/data/parquet/approved-proofs/`.
Exports contain bibliographic metadata, proof classifications, paraphrases and
evidence coordinates, but no extracted PDF text.

## Policies

- [`docs/corpus-policy.md`](docs/corpus-policy.md)
- [`docs/source-and-rights-policy.md`](docs/source-and-rights-policy.md)
- [`docs/architecture.md`](docs/architecture.md)

