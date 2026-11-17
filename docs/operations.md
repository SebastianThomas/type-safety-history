# Operations

## Before first deployment

1. Replace both `example.invalid` image references in `deploy/deployment.yaml`.
2. Replace the example hostname in `deploy/httproute.yaml` or remove the route.
3. Confirm the CNPG image catalog name and PostgreSQL major version available
   in the target cluster.
4. Label/select a node suitable for local-path persistence if the cluster has
   multiple nodes.
5. Establish backups to storage physically separate from both local PVCs.
6. Create the application configuration Secret before starting the Deployment:

   ```bash
   kubectl -n type-safety-history create secret generic type-safety-history-config \
     --from-literal=curator-token='<long-random-token>' \
     --from-literal=crossref-mailto='<monitored-research-email>'
   ```

Apply `db/` first and wait for the CNPG cluster to become ready. Then apply
`deploy/`. Both Kustomizations include the same idempotent Namespace definition.
The API runs migrations before it becomes ready; the worker waits for
the schema and claims work afterward.

## Backups

Local volumes are not backups. Back up PostgreSQL and `/data/objects` separately,
retain checksums, and regularly restore into a disposable namespace. Derived
data can be regenerated, but restoring it may shorten recovery time.

