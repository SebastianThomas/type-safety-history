CREATE TYPE export_status AS ENUM ('queued', 'running', 'succeeded', 'failed');

CREATE TABLE exports (
    id uuid PRIMARY KEY DEFAULT uuidv7(),
    export_kind text NOT NULL CHECK (export_kind IN ('approved_proofs_parquet')),
    status export_status NOT NULL DEFAULT 'queued',
    object_key text,
    sha256 text CHECK (sha256 IS NULL OR sha256 ~ '^[0-9a-f]{64}$'),
    row_count bigint CHECK (row_count IS NULL OR row_count >= 0),
    requested_by text NOT NULL,
    error text,
    created_at timestamptz NOT NULL DEFAULT now(),
    finished_at timestamptz,
    CHECK ((status = 'succeeded') = (object_key IS NOT NULL AND sha256 IS NOT NULL AND row_count IS NOT NULL))
);

CREATE INDEX exports_created_at_idx ON exports (created_at DESC);
