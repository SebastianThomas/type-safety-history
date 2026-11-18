CREATE TYPE discovery_status AS ENUM ('pending', 'imported', 'rejected');

CREATE TABLE discovery_candidates (
    id uuid PRIMARY KEY DEFAULT uuidv7(),
    provider text NOT NULL CHECK (provider IN ('crossref')),
    external_id text NOT NULL,
    title text NOT NULL,
    publication_year integer CHECK (publication_year BETWEEN 1800 AND 2200),
    work_type work_type NOT NULL,
    raw_metadata jsonb NOT NULL,
    status discovery_status NOT NULL DEFAULT 'pending',
    work_id uuid REFERENCES works(id) ON DELETE SET NULL,
    review_reason text,
    discovered_at timestamptz NOT NULL DEFAULT now(),
    reviewed_at timestamptz,
    UNIQUE (provider, external_id),
    CHECK ((status = 'imported') = (work_id IS NOT NULL))
);

CREATE TABLE discovery_hits (
    id uuid PRIMARY KEY DEFAULT uuidv7(),
    candidate_id uuid NOT NULL REFERENCES discovery_candidates(id) ON DELETE CASCADE,
    query_text text NOT NULL,
    from_year integer NOT NULL CHECK (from_year BETWEEN 1800 AND 2200),
    until_year integer NOT NULL CHECK (until_year BETWEEN 1800 AND 2200 AND until_year >= from_year),
    discovered_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (candidate_id, query_text, from_year, until_year)
);

CREATE INDEX discovery_candidates_pending_idx
    ON discovery_candidates (discovered_at DESC) WHERE status = 'pending';
CREATE INDEX discovery_hits_candidate_idx ON discovery_hits (candidate_id);
