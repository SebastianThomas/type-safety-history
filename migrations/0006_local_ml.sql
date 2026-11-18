ALTER TABLE proof_results
    ADD COLUMN confidence real CHECK (confidence IS NULL OR confidence BETWEEN 0 AND 1),
    ADD COLUMN classifier_version text;

ALTER TABLE documents
    ADD COLUMN extraction_method text,
    ADD COLUMN ocr_page_count integer NOT NULL DEFAULT 0 CHECK (ocr_page_count >= 0),
    ADD COLUMN processed_at timestamptz;

CREATE TABLE document_embeddings (
    document_id uuid PRIMARY KEY REFERENCES documents(id) ON DELETE CASCADE,
    algorithm text NOT NULL,
    dimensions integer NOT NULL CHECK (dimensions > 0),
    vector real[] NOT NULL,
    zarr_object_key text NOT NULL UNIQUE,
    zarr_manifest_sha256 text NOT NULL CHECK (zarr_manifest_sha256 ~ '^[0-9a-f]{64}$'),
    pipeline_run_id uuid NOT NULL REFERENCES pipeline_runs(id) ON DELETE RESTRICT,
    created_at timestamptz NOT NULL DEFAULT now(),
    CHECK (cardinality(vector) = dimensions)
);

CREATE TABLE document_similarities (
    left_document_id uuid NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
    right_document_id uuid NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
    score real NOT NULL CHECK (score BETWEEN 0 AND 1),
    algorithm text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    CHECK (left_document_id < right_document_id),
    PRIMARY KEY (left_document_id, right_document_id, algorithm)
);

CREATE TABLE citation_candidates (
    id uuid PRIMARY KEY DEFAULT uuidv7(),
    citing_work_id uuid NOT NULL REFERENCES works(id) ON DELETE CASCADE,
    source_document_id uuid NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
    page integer NOT NULL CHECK (page > 0),
    cited_doi text NOT NULL,
    context_sha256 text NOT NULL CHECK (context_sha256 ~ '^[0-9a-f]{64}$'),
    status assertion_status NOT NULL DEFAULT 'machine_candidate',
    created_by text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (source_document_id, page, cited_doi)
);

CREATE TABLE entity_resolution_suggestions (
    id uuid PRIMARY KEY DEFAULT uuidv7(),
    discovery_candidate_id uuid NOT NULL REFERENCES discovery_candidates(id) ON DELETE CASCADE,
    work_id uuid NOT NULL REFERENCES works(id) ON DELETE CASCADE,
    score real NOT NULL CHECK (score BETWEEN 0 AND 1),
    method text NOT NULL,
    rationale jsonb NOT NULL DEFAULT '{}'::jsonb,
    status resolution_status NOT NULL DEFAULT 'pending',
    resolved_by text,
    resolution_reason text,
    resolved_at timestamptz,
    UNIQUE (discovery_candidate_id, work_id, method)
);

CREATE TABLE ml_labels (
    id uuid PRIMARY KEY DEFAULT uuidv7(),
    proof_result_id uuid NOT NULL REFERENCES proof_results(id) ON DELETE CASCADE,
    expected_result_kind text NOT NULL,
    is_relevant boolean NOT NULL,
    labelled_by text NOT NULL,
    notes text,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (proof_result_id, labelled_by)
);

CREATE TABLE ml_evaluations (
    id uuid PRIMARY KEY DEFAULT uuidv7(),
    pipeline_version text NOT NULL,
    labelled_examples integer NOT NULL CHECK (labelled_examples >= 0),
    true_positives integer NOT NULL CHECK (true_positives >= 0),
    false_positives integer NOT NULL CHECK (false_positives >= 0),
    false_negatives integer NOT NULL CHECK (false_negatives >= 0),
    precision double precision,
    recall double precision,
    f1 double precision,
    details jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX document_similarities_left_idx ON document_similarities (left_document_id, score DESC);
CREATE INDEX document_similarities_right_idx ON document_similarities (right_document_id, score DESC);
CREATE INDEX citation_candidates_work_idx ON citation_candidates (citing_work_id, status);
CREATE INDEX entity_resolution_pending_idx
    ON entity_resolution_suggestions (score DESC) WHERE status = 'pending';
