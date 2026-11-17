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

## Policies

- [`docs/corpus-policy.md`](docs/corpus-policy.md)
- [`docs/source-and-rights-policy.md`](docs/source-and-rights-policy.md)
- [`docs/architecture.md`](docs/architecture.md)

