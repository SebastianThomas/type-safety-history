ALTER TABLE sources
    ADD COLUMN rights_basis text,
    ADD COLUMN ml_use_permitted boolean NOT NULL DEFAULT false,
    ADD COLUMN redistribution_permitted boolean NOT NULL DEFAULT false;

UPDATE sources
SET rights_basis = 'Crossref REST API metadata reuse; full text is not acquired.',
    ml_use_permitted = false,
    redistribution_permitted = true
WHERE source_kind = 'doi_registry' AND rights_status = 'open_metadata_only';

ALTER TABLE sources
    ADD CONSTRAINT sources_redistribution_rights_check
        CHECK (NOT redistribution_permitted OR rights_status IN ('open_redistributable', 'open_metadata_only')),
    ADD CONSTRAINT sources_ml_rights_check
        CHECK (NOT ml_use_permitted OR rights_status IN ('open_redistributable', 'internal_research_copy'));

ALTER TABLE proof_results
    ADD COLUMN source_document_id uuid REFERENCES documents(id) ON DELETE SET NULL,
    ADD COLUMN pipeline_run_id uuid REFERENCES pipeline_runs(id) ON DELETE SET NULL,
    ADD COLUMN candidate_key text UNIQUE;

CREATE INDEX proof_results_source_document_idx ON proof_results (source_document_id);
CREATE INDEX evidence_proof_result_idx ON evidence (proof_result_id);
