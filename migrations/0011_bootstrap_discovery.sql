ALTER TABLE discovery_schedules
    DROP CONSTRAINT discovery_schedules_requested_rows_check,
    ADD CONSTRAINT discovery_schedules_requested_rows_check
        CHECK (requested_rows BETWEEN 1 AND 1000);

ALTER TABLE discovery_schedules
    ADD COLUMN lease_until timestamptz,
    ADD COLUMN last_error text;

WITH seed(external_id, title, publication_year, work_type, role, curation_note) AS (
    VALUES
        (
            '10.1016/0022-0000(78)90014-4',
            'A Theory of Type Polymorphism in Programming',
            1978,
            'article',
            'historical_precursor',
            'Candidate anchor for semantic type soundness; theorem claims require page-level primary-source review.'
        ),
        (
            '10.1006/inco.1994.1093',
            'A Syntactic Approach to Type Soundness',
            1994,
            'article',
            'syntactic_approach_anchor',
            'Canonical modern anchor for the syntactic approach; do not convert this into an unqualified priority claim.'
        )
)
INSERT INTO discovery_candidates
    (provider, external_id, title, publication_year, work_type, raw_metadata)
SELECT
    'crossref', external_id, title, publication_year, work_type::work_type,
    jsonb_build_object(
        'seed_role', role,
        'curation_note', curation_note,
        'source', 'migration:0011_bootstrap_discovery'
    )
FROM seed
ON CONFLICT (provider, external_id) DO NOTHING;

WITH seed(external_id, query_text, publication_year) AS (
    VALUES
        ('10.1016/0022-0000(78)90014-4', 'curated-seed:historical_precursor', 1978),
        ('10.1006/inco.1994.1093', 'curated-seed:syntactic_approach_anchor', 1994)
)
INSERT INTO discovery_hits (candidate_id, query_text, from_year, until_year)
SELECT candidate.id, seed.query_text, seed.publication_year, seed.publication_year
FROM seed
JOIN discovery_candidates candidate
  ON candidate.provider = 'crossref' AND candidate.external_id = seed.external_id
ON CONFLICT DO NOTHING;

WITH seed(query_text) AS (
    VALUES
        ('type soundness programming language'),
        ('type safety programming language'),
        ('progress preservation type system'),
        ('subject reduction type system'),
        ('syntactic approach type soundness')
)
INSERT INTO discovery_schedules
    (query_text, from_year, until_year, requested_rows, cadence_minutes,
     enabled, next_run_at, created_by)
SELECT
    seed.query_text,
    1950,
    EXTRACT(YEAR FROM CURRENT_DATE)::integer,
    1000,
    10080,
    true,
    now(),
    'bootstrap'
FROM seed
WHERE NOT EXISTS (
    SELECT 1
    FROM discovery_schedules schedule
    WHERE schedule.provider = 'crossref'
      AND schedule.query_text = seed.query_text
      AND schedule.from_year = 1950
      AND schedule.until_year = EXTRACT(YEAR FROM CURRENT_DATE)::integer
);

INSERT INTO audit_log (actor, action, entity_type, after_value, reason)
VALUES (
    'migration:0011',
    'bootstrap',
    'discovery_schedule',
    jsonb_build_object('queries', 5, 'rows_per_query', 1000, 'cadence_minutes', 10080),
    'Install the default metadata-only Crossref discovery frontier'
);
