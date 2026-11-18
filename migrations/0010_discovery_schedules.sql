ALTER TABLE discovery_candidates
    DROP CONSTRAINT discovery_candidates_provider_check,
    ADD CONSTRAINT discovery_candidates_provider_check
        CHECK (provider IN ('crossref', 'openalex'));

CREATE TABLE discovery_schedules (
    id uuid PRIMARY KEY DEFAULT uuidv7(),
    provider text NOT NULL DEFAULT 'crossref' CHECK (provider = 'crossref'),
    query_text text NOT NULL CHECK (length(trim(query_text)) BETWEEN 3 AND 200),
    from_year integer NOT NULL CHECK (from_year BETWEEN 1800 AND 2200),
    until_year integer NOT NULL CHECK (until_year BETWEEN 1800 AND 2200),
    requested_rows integer NOT NULL CHECK (requested_rows BETWEEN 1 AND 100),
    cadence_minutes integer NOT NULL CHECK (cadence_minutes BETWEEN 60 AND 525600),
    enabled boolean NOT NULL DEFAULT true,
    next_run_at timestamptz NOT NULL DEFAULT now(),
    last_run_at timestamptz,
    created_by text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    CHECK (until_year >= from_year AND until_year - from_year <= 100)
);

CREATE INDEX discovery_schedules_due_idx
    ON discovery_schedules (next_run_at)
    WHERE enabled;
