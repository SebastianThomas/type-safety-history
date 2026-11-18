CREATE TABLE corpus_metric_snapshots (
    recorded_at timestamptz PRIMARY KEY,
    works bigint NOT NULL CHECK (works >= 0),
    discovered bigint NOT NULL CHECK (discovered >= 0),
    discovery_pending bigint NOT NULL CHECK (discovery_pending >= 0),
    discovery_imported bigint NOT NULL CHECK (discovery_imported >= 0),
    discovery_rejected bigint NOT NULL CHECK (discovery_rejected >= 0),
    proof_candidates bigint NOT NULL CHECK (proof_candidates >= 0),
    proofs_approved bigint NOT NULL CHECK (proofs_approved >= 0),
    proofs_rejected bigint NOT NULL CHECK (proofs_rejected >= 0),
    proofs_disputed bigint NOT NULL CHECK (proofs_disputed >= 0),
    active_documents bigint NOT NULL CHECK (active_documents >= 0),
    documents_taken_down bigint NOT NULL CHECK (documents_taken_down >= 0),
    duplicate_suggestions bigint NOT NULL CHECK (duplicate_suggestions >= 0),
    entity_suggestions bigint NOT NULL CHECK (entity_suggestions >= 0),
    queued_jobs bigint NOT NULL CHECK (queued_jobs >= 0),
    dead_letter_jobs bigint NOT NULL CHECK (dead_letter_jobs >= 0)
);

CREATE OR REPLACE FUNCTION capture_corpus_metric_snapshot()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    INSERT INTO corpus_metric_snapshots (
        recorded_at, works, discovered, discovery_pending, discovery_imported,
        discovery_rejected, proof_candidates, proofs_approved, proofs_rejected,
        proofs_disputed, active_documents, documents_taken_down,
        duplicate_suggestions, entity_suggestions, queued_jobs, dead_letter_jobs
    )
    SELECT
        date_trunc('minute', clock_timestamp()),
        (SELECT count(*) FROM works WHERE merged_into_work_id IS NULL),
        (SELECT count(*) FROM discovery_candidates),
        (SELECT count(*) FROM discovery_candidates WHERE status = 'pending'),
        (SELECT count(*) FROM discovery_candidates WHERE status = 'imported'),
        (SELECT count(*) FROM discovery_candidates WHERE status = 'rejected'),
        (SELECT count(*) FROM proof_results WHERE status = 'machine_candidate'),
        (SELECT count(*) FROM proof_results WHERE status = 'curator_approved'),
        (SELECT count(*) FROM proof_results WHERE status = 'rejected'),
        (SELECT count(*) FROM proof_results WHERE status = 'disputed'),
        (SELECT count(*) FROM documents WHERE status = 'active'),
        (SELECT count(*) FROM documents WHERE status = 'taken_down'),
        (SELECT count(*) FROM duplicate_suggestions WHERE status = 'pending'),
        (SELECT count(*) FROM entity_resolution_suggestions WHERE status = 'pending'),
        (SELECT count(*) FROM jobs WHERE status IN ('queued', 'running', 'failed')),
        (SELECT count(*) FROM jobs WHERE status = 'dead_letter')
    ON CONFLICT (recorded_at) DO UPDATE SET
        works = EXCLUDED.works,
        discovered = EXCLUDED.discovered,
        discovery_pending = EXCLUDED.discovery_pending,
        discovery_imported = EXCLUDED.discovery_imported,
        discovery_rejected = EXCLUDED.discovery_rejected,
        proof_candidates = EXCLUDED.proof_candidates,
        proofs_approved = EXCLUDED.proofs_approved,
        proofs_rejected = EXCLUDED.proofs_rejected,
        proofs_disputed = EXCLUDED.proofs_disputed,
        active_documents = EXCLUDED.active_documents,
        documents_taken_down = EXCLUDED.documents_taken_down,
        duplicate_suggestions = EXCLUDED.duplicate_suggestions,
        entity_suggestions = EXCLUDED.entity_suggestions,
        queued_jobs = EXCLUDED.queued_jobs,
        dead_letter_jobs = EXCLUDED.dead_letter_jobs;
    RETURN NULL;
END;
$$;

CREATE TRIGGER snapshot_works
AFTER INSERT OR UPDATE OR DELETE ON works
FOR EACH STATEMENT EXECUTE FUNCTION capture_corpus_metric_snapshot();
CREATE TRIGGER snapshot_discovery_candidates
AFTER INSERT OR UPDATE OR DELETE ON discovery_candidates
FOR EACH STATEMENT EXECUTE FUNCTION capture_corpus_metric_snapshot();
CREATE TRIGGER snapshot_proof_results
AFTER INSERT OR UPDATE OR DELETE ON proof_results
FOR EACH STATEMENT EXECUTE FUNCTION capture_corpus_metric_snapshot();
CREATE TRIGGER snapshot_documents
AFTER INSERT OR UPDATE OR DELETE ON documents
FOR EACH STATEMENT EXECUTE FUNCTION capture_corpus_metric_snapshot();
CREATE TRIGGER snapshot_jobs
AFTER INSERT OR UPDATE OR DELETE ON jobs
FOR EACH STATEMENT EXECUTE FUNCTION capture_corpus_metric_snapshot();
CREATE TRIGGER snapshot_duplicate_suggestions
AFTER INSERT OR UPDATE OR DELETE ON duplicate_suggestions
FOR EACH STATEMENT EXECUTE FUNCTION capture_corpus_metric_snapshot();
CREATE TRIGGER snapshot_entity_resolution_suggestions
AFTER INSERT OR UPDATE OR DELETE ON entity_resolution_suggestions
FOR EACH STATEMENT EXECUTE FUNCTION capture_corpus_metric_snapshot();

UPDATE works SET updated_at = updated_at WHERE false;
