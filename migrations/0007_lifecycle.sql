CREATE TYPE document_status AS ENUM ('active', 'taken_down');

ALTER TABLE documents
    ADD COLUMN status document_status NOT NULL DEFAULT 'active',
    ADD COLUMN taken_down_at timestamptz,
    ADD COLUMN taken_down_by text,
    ADD COLUMN takedown_reason text,
    ADD CONSTRAINT documents_takedown_state_check CHECK (
        (status = 'active' AND taken_down_at IS NULL AND taken_down_by IS NULL AND takedown_reason IS NULL)
        OR
        (status = 'taken_down' AND taken_down_at IS NOT NULL
         AND length(trim(taken_down_by)) > 0 AND length(trim(takedown_reason)) > 0)
    );

ALTER TABLE works
    ADD COLUMN merged_into_work_id uuid REFERENCES works(id) ON DELETE RESTRICT,
    ADD COLUMN merged_at timestamptz,
    ADD COLUMN merged_by text,
    ADD CONSTRAINT works_merge_state_check CHECK (
        (merged_into_work_id IS NULL AND merged_at IS NULL AND merged_by IS NULL)
        OR
        (merged_into_work_id IS NOT NULL AND merged_into_work_id <> id
         AND merged_at IS NOT NULL AND length(trim(merged_by)) > 0)
    );

CREATE INDEX documents_active_idx ON documents (work_id, acquired_at) WHERE status = 'active';
CREATE INDEX works_canonical_idx ON works (publication_year, title) WHERE merged_into_work_id IS NULL;
