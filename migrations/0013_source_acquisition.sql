CREATE TABLE source_resolution_queue (
    work_id uuid PRIMARY KEY REFERENCES works(id) ON DELETE CASCADE,
    status text NOT NULL DEFAULT 'queued'
        CHECK (status IN (
            'queued', 'running', 'resolved', 'no_doi', 'no_open_source',
            'rights_review', 'failed'
        )),
    priority smallint NOT NULL DEFAULT 0 CHECK (priority BETWEEN 0 AND 100),
    attempts integer NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    available_at timestamptz NOT NULL DEFAULT now(),
    lease_until timestamptz,
    provider text,
    result jsonb,
    last_error text,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX source_resolution_queue_claim_idx
    ON source_resolution_queue (status, available_at, priority DESC, created_at)
    WHERE status IN (
        'queued', 'failed', 'running', 'no_doi', 'no_open_source', 'rights_review'
    );

INSERT INTO source_resolution_queue (work_id, priority)
SELECT work.id,
       COALESCE(round(candidate.relevance_score * 100)::smallint, 50)
FROM works work
LEFT JOIN discovery_candidates candidate ON candidate.work_id = work.id
WHERE work.merged_into_work_id IS NULL
  AND NOT EXISTS (
      SELECT 1 FROM documents document
      WHERE document.work_id = work.id AND document.status = 'active'
  )
ON CONFLICT (work_id) DO NOTHING;

INSERT INTO jobs (job_type, idempotency_key, payload)
VALUES (
    'promote_possible_candidates',
    'promote-possible-candidates:local-tfidf-hybrid-v1',
    '{}'::jsonb
)
ON CONFLICT (idempotency_key) DO NOTHING;
