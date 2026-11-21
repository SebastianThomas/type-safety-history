WITH rejected AS (
    UPDATE proof_results
    SET status = 'rejected'
    WHERE status = 'machine_candidate'
      AND classifier_version = 'local-term-baseline-v1'
    RETURNING id
)
INSERT INTO audit_log (actor, action, entity_type, after_value, reason)
SELECT
    'migration:0014',
    'retire_classifier_output',
    'proof_result',
    jsonb_build_object(
        'classifier_version', 'local-term-baseline-v1',
        'rejected_candidates', count(*)
    ),
    'Retire context-free term matches before local contextual reclassification'
FROM rejected;
