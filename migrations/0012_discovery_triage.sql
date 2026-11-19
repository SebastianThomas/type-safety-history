ALTER TYPE discovery_status ADD VALUE IF NOT EXISTS 'screened_out';

ALTER TABLE discovery_candidates
    ADD COLUMN relevance_score real
        CHECK (relevance_score IS NULL OR relevance_score BETWEEN 0 AND 1),
    ADD COLUMN relevance_category text
        CHECK (relevance_category IS NULL OR relevance_category IN
            ('likely_relevant', 'possible', 'unlikely')),
    ADD COLUMN triage_version text,
    ADD COLUMN triaged_at timestamptz;

CREATE INDEX discovery_candidates_pending_relevance_idx
    ON discovery_candidates (relevance_score DESC NULLS LAST, discovered_at DESC)
    WHERE status = 'pending';

INSERT INTO jobs (job_type, idempotency_key, payload)
VALUES (
    'triage_discovery_candidates',
    'triage-discovery:bootstrap:local-title-rules-v1',
    '{"only_untriaged": true}'::jsonb
)
ON CONFLICT (idempotency_key) DO NOTHING;
