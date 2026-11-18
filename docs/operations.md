# Operations

## Before first deployment

1. Replace both `example.invalid` image references in `deploy/deployment.yaml`.
2. Replace the example hostname in `deploy/httproute.yaml` or remove the route.
3. Confirm the CNPG image catalog name and PostgreSQL major version available
   in the target cluster.
4. Label/select a node suitable for local-path persistence if the cluster has
   multiple nodes.
5. Establish backups to storage physically separate from both local PVCs.
6. Create the application bootstrap configuration Secret before starting the Deployment:

   ```bash
   kubectl -n type-safety-history create secret generic type-safety-history-config \
     --from-literal=curator-token='<long-random-token>' \
     --from-literal=crossref-mailto='<monitored-research-email>' \
     --from-literal=openalex-api-key='<optional-openalex-key>'
   ```

Apply `db/` first and wait for the CNPG cluster to become ready. Then apply
`deploy/`. Both Kustomizations include the same idempotent Namespace definition.
The API runs migrations before it becomes ready; the worker waits for
the schema and claims work afterward.

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

Load the curated starting queue after migrations:

```bash
kubectl -n type-safety-history exec deploy/type-safety-history -c api -- \
  api seed /app/fixtures/seed-works.json
```

The command is idempotent and does not require outbound network access.

## Backups

Local volumes are not backups. Back up PostgreSQL and `/data/objects` separately,
retain checksums, and regularly restore into a disposable namespace. Derived
data can be regenerated, but restoring it may shorten recovery time.

Exercise restoration and the document-takedown path in a disposable namespace.
CI runs a black-box workflow covering seed loading, PDF processing, duplicate
consolidation, takedown and the statistics interface.

After restoring PostgreSQL and the application volume, run:

```bash
kubectl -n type-safety-history exec deploy/type-safety-history -c api -- \
  api verify-storage
```

The command exits non-zero for missing/mismatched source or Parquet objects,
missing Zarr directories, unsafe stored paths, or reappearing taken-down files.

