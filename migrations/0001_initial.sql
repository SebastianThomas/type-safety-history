CREATE EXTENSION IF NOT EXISTS pg_trgm;

CREATE TYPE work_type AS ENUM (
    'article', 'conference_paper', 'preprint', 'thesis', 'technical_report',
    'book_chapter', 'formal_artifact', 'other'
);
CREATE TYPE review_status AS ENUM ('candidate', 'reviewed', 'rejected', 'disputed');
CREATE TYPE source_kind AS ENUM (
    'publisher', 'proceedings', 'doi_registry', 'institutional_repository',
    'author_page', 'preprint_repository', 'bibliography', 'artifact_repository',
    'curator_upload', 'other'
);
CREATE TYPE source_reliability AS ENUM ('primary', 'authoritative_metadata', 'corroborating', 'discovery_only');
CREATE TYPE rights_status AS ENUM (
    'open_redistributable', 'open_metadata_only', 'internal_research_copy',
    'link_only', 'unknown_requires_review', 'prohibited'
);
CREATE TYPE assertion_status AS ENUM ('machine_candidate', 'curator_approved', 'rejected', 'disputed');
CREATE TYPE job_status AS ENUM ('queued', 'running', 'succeeded', 'failed', 'dead_letter');

CREATE TABLE works (
    id uuid PRIMARY KEY DEFAULT uuidv7(),
    title text NOT NULL CHECK (length(trim(title)) > 0),
    work_type work_type NOT NULL DEFAULT 'article',
    publication_year integer CHECK (publication_year BETWEEN 1800 AND 2200),
    doi text UNIQUE,
    abstract text,
    review_status review_status NOT NULL DEFAULT 'candidate',
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX works_title_trgm_idx ON works USING gin (title gin_trgm_ops);
CREATE INDEX works_publication_year_idx ON works (publication_year);

CREATE TABLE work_versions (
    id uuid PRIMARY KEY DEFAULT uuidv7(),
    work_id uuid NOT NULL REFERENCES works(id) ON DELETE CASCADE,
    version_label text NOT NULL,
    published_at date,
    is_canonical boolean NOT NULL DEFAULT false,
    UNIQUE (work_id, version_label)
);

CREATE TABLE people (
    id uuid PRIMARY KEY DEFAULT uuidv7(),
    display_name text NOT NULL,
    orcid text UNIQUE
);
CREATE TABLE work_authors (
    work_id uuid NOT NULL REFERENCES works(id) ON DELETE CASCADE,
    person_id uuid NOT NULL REFERENCES people(id),
    position integer NOT NULL CHECK (position >= 0),
    PRIMARY KEY (work_id, person_id),
    UNIQUE (work_id, position)
);

CREATE TABLE sources (
    id uuid PRIMARY KEY DEFAULT uuidv7(),
    work_id uuid REFERENCES works(id) ON DELETE CASCADE,
    url text NOT NULL,
    source_kind source_kind NOT NULL,
    reliability source_reliability NOT NULL,
    rights_status rights_status NOT NULL DEFAULT 'unknown_requires_review',
    license_name text,
    license_url text,
    terms_url text,
    robots_checked_at timestamptz,
    acquired_at timestamptz NOT NULL DEFAULT now(),
    raw_metadata jsonb,
    UNIQUE (url, work_id)
);

CREATE TABLE documents (
    id uuid PRIMARY KEY DEFAULT uuidv7(),
    work_id uuid NOT NULL REFERENCES works(id) ON DELETE CASCADE,
    source_id uuid REFERENCES sources(id),
    object_key text NOT NULL UNIQUE CHECK (object_key !~ '(^/|(^|/)\.\.(/|$))'),
    sha256 text NOT NULL UNIQUE CHECK (sha256 ~ '^[0-9a-f]{64}$'),
    media_type text NOT NULL,
    byte_size bigint NOT NULL CHECK (byte_size >= 0),
    rights_status rights_status NOT NULL,
    ml_use_permitted boolean NOT NULL DEFAULT false,
    redistribution_permitted boolean NOT NULL DEFAULT false,
    acquired_at timestamptz NOT NULL DEFAULT now(),
    CHECK (NOT redistribution_permitted OR rights_status = 'open_redistributable'),
    CHECK (rights_status IN ('open_redistributable', 'internal_research_copy')),
    CHECK (NOT ml_use_permitted OR rights_status IN ('open_redistributable', 'internal_research_copy'))
);

CREATE TABLE document_spans (
    id uuid PRIMARY KEY DEFAULT uuidv7(),
    document_id uuid NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
    page_start integer,
    page_end integer,
    section_path text,
    char_start integer,
    char_end integer,
    content_sha256 text CHECK (content_sha256 IS NULL OR content_sha256 ~ '^[0-9a-f]{64}$'),
    CHECK (page_end IS NULL OR page_start IS NULL OR page_end >= page_start),
    CHECK (char_end IS NULL OR char_start IS NULL OR char_end >= char_start)
);

CREATE TABLE languages (
    id uuid PRIMARY KEY DEFAULT uuidv7(),
    name text NOT NULL UNIQUE,
    kind text NOT NULL DEFAULT 'calculus',
    description text
);

CREATE TABLE proof_results (
    id uuid PRIMARY KEY DEFAULT uuidv7(),
    work_id uuid NOT NULL REFERENCES works(id) ON DELETE CASCADE,
    language_id uuid REFERENCES languages(id),
    theorem_name text,
    result_kind text NOT NULL,
    proof_method text,
    statement_summary text NOT NULL,
    status assertion_status NOT NULL DEFAULT 'machine_candidate',
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE proof_obligations (
    id uuid PRIMARY KEY DEFAULT uuidv7(),
    proof_result_id uuid NOT NULL REFERENCES proof_results(id) ON DELETE CASCADE,
    obligation_kind text NOT NULL,
    theorem_name text,
    UNIQUE (proof_result_id, obligation_kind, theorem_name)
);

CREATE TABLE evidence (
    id uuid PRIMARY KEY DEFAULT uuidv7(),
    proof_result_id uuid NOT NULL REFERENCES proof_results(id) ON DELETE CASCADE,
    document_span_id uuid NOT NULL REFERENCES document_spans(id),
    support_kind text NOT NULL DEFAULT 'supports',
    paraphrase text NOT NULL,
    created_by text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE citations (
    citing_work_id uuid NOT NULL REFERENCES works(id) ON DELETE CASCADE,
    cited_work_id uuid NOT NULL REFERENCES works(id) ON DELETE CASCADE,
    source_document_id uuid REFERENCES documents(id),
    status assertion_status NOT NULL DEFAULT 'machine_candidate',
    PRIMARY KEY (citing_work_id, cited_work_id),
    CHECK (citing_work_id <> cited_work_id)
);

CREATE TABLE pipeline_runs (
    id uuid PRIMARY KEY DEFAULT uuidv7(),
    pipeline_name text NOT NULL,
    pipeline_version text NOT NULL,
    model_name text,
    model_version text,
    model_sha256 text,
    model_license text,
    started_at timestamptz NOT NULL DEFAULT now(),
    finished_at timestamptz,
    configuration jsonb NOT NULL DEFAULT '{}'::jsonb
);

CREATE TABLE jobs (
    id uuid PRIMARY KEY DEFAULT uuidv7(),
    job_type text NOT NULL,
    status job_status NOT NULL DEFAULT 'queued',
    idempotency_key text NOT NULL UNIQUE,
    payload jsonb NOT NULL,
    result jsonb,
    attempts integer NOT NULL DEFAULT 0,
    max_attempts integer NOT NULL DEFAULT 5,
    available_at timestamptz NOT NULL DEFAULT now(),
    leased_until timestamptz,
    worker_id text,
    last_error text,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    CHECK (attempts >= 0 AND max_attempts > 0)
);
CREATE INDEX jobs_claim_idx ON jobs (status, available_at, created_at)
    WHERE status IN ('queued', 'failed');

CREATE TABLE audit_log (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    actor text NOT NULL,
    action text NOT NULL,
    entity_type text NOT NULL,
    entity_id uuid,
    before_value jsonb,
    after_value jsonb,
    reason text,
    occurred_at timestamptz NOT NULL DEFAULT now()
);

