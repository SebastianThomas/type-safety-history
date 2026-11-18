CREATE TYPE scholarly_status AS ENUM ('active', 'corrected', 'retracted', 'disputed');
CREATE TYPE work_relation_kind AS ENUM (
    'preprint_of', 'conference_version_of', 'journal_version_of',
    'corrects', 'retracts', 'extends', 'mechanizes', 'other'
);
CREATE TYPE artifact_kind AS ENUM (
    'source_repository', 'proof_source', 'archived_snapshot',
    'build_manifest', 'build_log', 'supplement', 'other'
);
CREATE TYPE artifact_status AS ENUM ('candidate', 'verified', 'unavailable', 'invalid');
CREATE TYPE annotation_kind AS ENUM (
    'curator_note', 'correction', 'retraction', 'counterexample',
    'dispute', 'priority_claim', 'other'
);
CREATE TYPE resolution_status AS ENUM ('pending', 'accepted', 'rejected');

ALTER TABLE works
    ADD COLUMN scholarly_status scholarly_status NOT NULL DEFAULT 'active';

CREATE TABLE work_relations (
    id uuid PRIMARY KEY DEFAULT uuidv7(),
    from_work_id uuid NOT NULL REFERENCES works(id) ON DELETE CASCADE,
    to_work_id uuid NOT NULL REFERENCES works(id) ON DELETE CASCADE,
    relation_kind work_relation_kind NOT NULL,
    evidence_source_id uuid REFERENCES sources(id) ON DELETE SET NULL,
    notes text,
    status assertion_status NOT NULL DEFAULT 'curator_approved',
    created_by text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    CHECK (from_work_id <> to_work_id),
    UNIQUE (from_work_id, to_work_id, relation_kind)
);

CREATE TABLE formal_artifacts (
    id uuid PRIMARY KEY DEFAULT uuidv7(),
    work_id uuid NOT NULL REFERENCES works(id) ON DELETE CASCADE,
    artifact_kind artifact_kind NOT NULL,
    url text NOT NULL,
    repository_commit text,
    proof_assistant text,
    proof_assistant_version text,
    license_name text,
    license_url text,
    archived_url text,
    sha256 text CHECK (sha256 IS NULL OR sha256 ~ '^[0-9a-f]{64}$'),
    status artifact_status NOT NULL DEFAULT 'candidate',
    verification_notes text,
    verified_by text,
    verified_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (work_id, url),
    CHECK ((status = 'verified') = (verified_by IS NOT NULL AND verified_at IS NOT NULL))
);

CREATE TABLE language_features (
    id uuid PRIMARY KEY DEFAULT uuidv7(),
    slug text NOT NULL UNIQUE CHECK (slug ~ '^[a-z0-9]+(?:_[a-z0-9]+)*$'),
    display_name text NOT NULL UNIQUE,
    description text NOT NULL
);

CREATE TABLE proof_result_features (
    proof_result_id uuid NOT NULL REFERENCES proof_results(id) ON DELETE CASCADE,
    feature_id uuid NOT NULL REFERENCES language_features(id),
    created_by text NOT NULL,
    PRIMARY KEY (proof_result_id, feature_id)
);

CREATE TABLE type_systems (
    id uuid PRIMARY KEY DEFAULT uuidv7(),
    name text NOT NULL UNIQUE,
    description text,
    parent_id uuid REFERENCES type_systems(id) ON DELETE SET NULL
);

CREATE TABLE proof_result_type_systems (
    proof_result_id uuid NOT NULL REFERENCES proof_results(id) ON DELETE CASCADE,
    type_system_id uuid NOT NULL REFERENCES type_systems(id),
    created_by text NOT NULL,
    PRIMARY KEY (proof_result_id, type_system_id)
);

CREATE TABLE research_annotations (
    id uuid PRIMARY KEY DEFAULT uuidv7(),
    work_id uuid REFERENCES works(id) ON DELETE CASCADE,
    proof_result_id uuid REFERENCES proof_results(id) ON DELETE CASCADE,
    annotation_kind annotation_kind NOT NULL,
    summary text NOT NULL,
    evidence_source_id uuid REFERENCES sources(id) ON DELETE SET NULL,
    evidence_document_span_id uuid REFERENCES document_spans(id) ON DELETE SET NULL,
    status assertion_status NOT NULL DEFAULT 'curator_approved',
    created_by text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    CHECK ((work_id IS NOT NULL)::integer + (proof_result_id IS NOT NULL)::integer = 1)
);

CREATE TABLE coverage_runs (
    id uuid PRIMARY KEY DEFAULT uuidv7(),
    provider text NOT NULL,
    query_text text NOT NULL,
    from_year integer NOT NULL CHECK (from_year BETWEEN 1800 AND 2200),
    until_year integer NOT NULL CHECK (until_year BETWEEN from_year AND 2200),
    requested_rows integer NOT NULL CHECK (requested_rows BETWEEN 1 AND 1000),
    result_count integer NOT NULL CHECK (result_count >= 0),
    started_at timestamptz NOT NULL,
    finished_at timestamptz NOT NULL,
    configuration jsonb NOT NULL DEFAULT '{}'::jsonb,
    CHECK (finished_at >= started_at)
);

CREATE TABLE duplicate_suggestions (
    id uuid PRIMARY KEY DEFAULT uuidv7(),
    left_work_id uuid NOT NULL REFERENCES works(id) ON DELETE CASCADE,
    right_work_id uuid NOT NULL REFERENCES works(id) ON DELETE CASCADE,
    score double precision NOT NULL CHECK (score BETWEEN 0 AND 1),
    method text NOT NULL,
    rationale jsonb NOT NULL DEFAULT '{}'::jsonb,
    status resolution_status NOT NULL DEFAULT 'pending',
    canonical_work_id uuid REFERENCES works(id) ON DELETE SET NULL,
    resolved_by text,
    resolution_reason text,
    created_at timestamptz NOT NULL DEFAULT now(),
    resolved_at timestamptz,
    CHECK (left_work_id <> right_work_id),
    CHECK (left_work_id < right_work_id),
    CHECK ((status = 'accepted') = (canonical_work_id IS NOT NULL)),
    UNIQUE (left_work_id, right_work_id, method)
);

ALTER TABLE citations
    ADD COLUMN evidence_span_id uuid REFERENCES document_spans(id) ON DELETE SET NULL,
    ADD COLUMN created_by text,
    ADD COLUMN review_reason text;

CREATE INDEX work_relations_from_idx ON work_relations (from_work_id);
CREATE INDEX work_relations_to_idx ON work_relations (to_work_id);
CREATE INDEX formal_artifacts_work_idx ON formal_artifacts (work_id);
CREATE INDEX research_annotations_work_idx ON research_annotations (work_id);
CREATE INDEX research_annotations_proof_idx ON research_annotations (proof_result_id);
CREATE INDEX coverage_runs_provider_years_idx ON coverage_runs (provider, from_year, until_year);
CREATE INDEX duplicate_suggestions_pending_idx
    ON duplicate_suggestions (score DESC) WHERE status = 'pending';

INSERT INTO language_features (slug, display_name, description) VALUES
    ('references', 'References', 'Mutable or immutable reference cells and store typing.'),
    ('exceptions', 'Exceptions', 'Exception values, handlers, or exceptional control flow.'),
    ('effects', 'Effects', 'Effect systems, handlers, rows, or algebraic effects.'),
    ('subtyping', 'Subtyping', 'A non-trivial subtype relation participates in typing.'),
    ('polymorphism', 'Polymorphism', 'Parametric, ad-hoc, row, or higher-rank polymorphism.'),
    ('dependent_types', 'Dependent types', 'Types may depend on terms.'),
    ('gradual_typing', 'Gradual typing', 'Static and dynamic typing interact through gradual types.'),
    ('concurrency', 'Concurrency', 'Concurrent processes, threads, actors, or shared-memory semantics.'),
    ('session_types', 'Session types', 'Communication protocols are represented by session types.'),
    ('objects', 'Objects', 'Object, class, or member-based calculi.'),
    ('modules', 'Modules', 'Module, signature, or package typing.'),
    ('linear_types', 'Linear types', 'Linear, affine, or resource-sensitive typing.'),
    ('recursive_types', 'Recursive types', 'Iso-recursive or equi-recursive types.'),
    ('refinement_types', 'Refinement types', 'Types contain logical predicates or refinements.');
