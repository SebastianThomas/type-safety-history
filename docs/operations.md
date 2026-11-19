# Operations

## Before first deployment

1. Confirm `type-safety-history.sthomas.ch` resolves to the homelab ingress.
2. Confirm the homelab `postgresql-minimal-trixie` CNPG image catalog exposes
   PostgreSQL major version 18.
3. Label/select a node suitable for local-path persistence if the cluster has
   multiple nodes.
4. Establish backups to storage physically separate from both local PVCs.
5. Configure the GitHub Actions settings listed below.

Tagged releases are the production path. A tag matching `vMAJOR.MINOR.PATCH`
(with an optional prerelease suffix) runs the complete Rust, Python, E2E,
manifest, container and supply-chain suite before publishing these tags to the
single Docker Hub repository:

- `vMAJOR.MINOR.PATCH-api` and `latest-api`
- `vMAJOR.MINOR.PATCH-ml` and `latest-ml`

After a successful tagged build, `.github/workflows/deploy.yml` joins the
homelab through Headscale, reconciles CNPG and the application Secret, deploys
the immutable versioned image pair, waits for rollout, seeds the research queue
and verifies database-to-volume integrity. The workflow can also redeploy a
previously published tag manually. Configure the `production` GitHub
environment with deployment protection/required reviewers as appropriate.
The repository's `production` environment currently has a five-minute wait
timer so a tagged release cannot deploy instantaneously.

Repository Actions configuration:

- Secret `DOCKERHUB_TOKEN`: Docker Hub personal access token with write access
  to `sthomasch/type-safety-history`.
- Variable `DOCKERHUB_USERNAME`: `sthomasch`.
- Variable `DOCKERHUB_REPOSITORY`: `sthomasch/type-safety-history`.

`production` environment secrets:

- `TS_AUTHKEY`: reusable or ephemeral Headscale pre-authentication key for the
  Actions runner.
- `HEADSCALE_URL`: Headscale control-server URL.
- `KUBE_API`: homelab Kubernetes API URL reachable after joining Headscale.
- `KUBE_CA`: base64-encoded cluster CA expected by `homelab-actions`.
- `KUBE_TOKEN`: least-privilege service-account bearer token for the
  `type-safety-history` namespace plus the CNPG resources it reconciles.
- `CURATOR_TOKEN`: long random bootstrap administrator token.
- `OPENALEX_API_KEY`: optional; omit or leave empty to disable OpenAlex.

Generate `CURATOR_TOKEN`, store it in the protected `production` GitHub
environment, and print it once for transfer to a password manager:

```bash
./scripts/set-curator-token.sh
```

The script uses 256 bits from OpenSSL and sends the value to `gh secret set`
over standard input, so it is not exposed as a command-line argument. Override
the defaults with `GH_REPO=owner/repository` or `GH_ENVIRONMENT=name`. GitHub
cannot reveal an Actions secret after it is stored; retain the printed value.
The Kubernetes Secret changes on the next deployment.

`production` environment variable:

- `CROSSREF_MAILTO`: monitored research contact email used for Crossref's
  polite pool.

For an emergency manual deployment, create the application configuration
Secret before starting the Deployment:

   ```bash
   kubectl -n type-safety-history create secret generic type-safety-history-config \
     --from-literal=curator-token='<long-random-token>' \
     --from-literal=crossref-mailto='<monitored-research-email>' \
     --from-literal=openalex-api-key='<optional-openalex-key>'
   ```

Apply `db/` first and wait for the CNPG cluster to become ready. Then apply
`deploy/` after replacing `${API_IMAGE}` and `${ML_IMAGE}` with published image
references. Both Kustomizations include the same idempotent Namespace definition.
Both Deployments have two init containers: one runs idempotent migrations and
one prepares the shared local volume permissions. A failed migration is retried
by Kubernetes, so a separate database-wait container is unnecessary. The
migrations install the curated anchors and default discovery schedules. Storage
verification remains an explicit restoration/integrity operation instead of
blocking every application restart.

Migration `0012_discovery_triage.sql` queues one idempotent local triage job for
all pre-existing pending candidates. Each later discovery batch queues the same
worker operation for new or refreshed metadata. Observe progress through
`discovery_untriaged`, `discovery_likely_relevant`, and the active-job counters
on `/dashboard`, `/api/v1/stats/overview`, or `/metrics`. A completed triage run
promotes likely records as candidate works, screens unlikely records without
deleting them, and leaves only the uncertain band pending in `/curate`.

Application Deployments use `Recreate` and retain all durable state in CNPG and
the local PVC. Discovery schedules and worker jobs use expiring database leases;
an API or worker process killed after claiming work can therefore be retried by
its replacement. File-producing worker operations write temporary paths and
rename them into place before committing their database result.

The Kubernetes pod and every container use the runtime-default seccomp profile,
drop Linux capabilities and use immutable root filesystems. A bounded
`emptyDir` provides `/tmp`; canonical and derived application data remain on
the local PVC.

After first start, use the bootstrap token to create named administrators and
curators through `/api/v1/admin/users` and issue separate tokens through
`/api/v1/admin/users/{id}/tokens`. Store issued tokens in the operator's secret
manager. The plaintext is returned only once. Deactivate departed users or
revoke individual tokens through the admin API; reserve the bootstrap secret
for recovery and rotate it through the Kubernetes Secret. Do not use shared
named tokens: the authenticated username is the authoritative audit identity.
Use viewer tokens for read-only research access, curator tokens for review, and
admin tokens only for account operations. Review account and token activity via
`/api/v1/audit` and `/api/v1/admin/users/{id}/tokens`.

To refresh the curated anchors manually:

```bash
kubectl -n type-safety-history exec deploy/type-safety-history-api -c api -- \
  api seed /app/fixtures/seed-works.json
```

The command is idempotent and does not require outbound network access. It is
not required during normal deployment.

## Backups

Local volumes are not backups. Back up PostgreSQL and `/data/objects` separately,
retain checksums, and regularly restore into a disposable namespace. Derived
data can be regenerated, but restoring it may shorten recovery time.

Exercise restoration and the document-takedown path in a disposable namespace.
CI runs a black-box workflow covering seed loading, PDF processing, duplicate
consolidation, takedown and the statistics interface.

After restoring PostgreSQL and the application volume, run:

```bash
kubectl -n type-safety-history exec deploy/type-safety-history-api -c api -- \
  api verify-storage
```

The command exits non-zero for missing/mismatched source or Parquet objects,
missing Zarr directories, unsafe stored paths, or reappearing taken-down files.

