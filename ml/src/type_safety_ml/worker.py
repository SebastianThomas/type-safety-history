from __future__ import annotations

import hashlib
import json
import logging
import os
import socket
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any
from uuid import UUID

import psycopg
import pymupdf
from psycopg.rows import dict_row

LOG = logging.getLogger("type_safety_ml.worker")
LEASE_SECONDS = 300
PIPELINE_VERSION = "local-term-baseline-v1"
MAX_CANDIDATES_PER_DOCUMENT = 25


@dataclass(frozen=True)
class Candidate:
    page: int
    result_kind: str
    char_start: int
    char_end: int
    content_sha256: str
    matched_terms: tuple[str, ...]
    candidate_key: str


@dataclass(frozen=True)
class ProcessedDocument:
    result: dict[str, Any]
    candidates: tuple[Candidate, ...]


def safe_object_path(data_root: Path, object_key: str) -> Path:
    candidate = (data_root / object_key).resolve()
    root = data_root.resolve()
    if not candidate.is_relative_to(root):
        raise ValueError("object key escapes DATA_ROOT")
    if not candidate.is_file():
        raise ValueError("object key is not a local regular file")
    return candidate


def extract_pdf(path: Path, derived_root: Path, document_id: UUID) -> ProcessedDocument:
    pages: list[dict[str, Any]] = []
    with pymupdf.open(path) as document:
        if document.needs_pass:
            raise ValueError("encrypted PDFs are not processed")
        for number, page in enumerate(document, start=1):
            text = page.get_text("text")
            pages.append({"page": number, "text": text})

    encoded = json.dumps(
        {"document_id": str(document_id), "pages": pages},
        ensure_ascii=False,
        separators=(",", ":"),
    ).encode("utf-8")
    digest = hashlib.sha256(encoded).hexdigest()
    output_dir = derived_root / str(document_id)
    output_dir.mkdir(parents=True, exist_ok=True)
    temporary = output_dir / f"extracted-{digest}.json.tmp"
    destination = output_dir / f"extracted-{digest}.json"
    temporary.write_bytes(encoded)
    temporary.replace(destination)
    candidates = classify_pages(document_id, pages)
    return ProcessedDocument(
        result={
            "derived_object_key": str(destination.relative_to(derived_root.parent)),
            "sha256": digest,
            "page_count": len(pages),
            "candidate_count": len(candidates),
            "pipeline_version": PIPELINE_VERSION,
        },
        candidates=tuple(candidates),
    )


def classify_pages(document_id: UUID, pages: list[dict[str, Any]]) -> list[Candidate]:
    candidates: list[Candidate] = []
    patterns = (
        ("progress_and_preservation", ("progress", "preservation")),
        ("preservation", ("subject reduction",)),
        ("type_soundness", ("type soundness",)),
        ("type_soundness", ("well-typed", "go wrong")),
        ("preservation", ("preservation",)),
        ("progress", ("progress",)),
    )
    for page in pages:
        text = str(page["text"])
        lowered = text.casefold()
        emitted: set[str] = set()
        for result_kind, terms in patterns:
            if result_kind in emitted or not all(term in lowered for term in terms):
                continue
            offsets = [lowered.index(term) for term in terms]
            char_start = min(offsets)
            char_end = min(len(text), max(offsets[index] + len(term) for index, term in enumerate(terms)))
            content_sha256 = hashlib.sha256(text.encode("utf-8")).hexdigest()
            key_material = (
                f"{document_id}:{page['page']}:{result_kind}:{content_sha256}:{PIPELINE_VERSION}"
            )
            candidates.append(
                Candidate(
                    page=int(page["page"]),
                    result_kind=result_kind,
                    char_start=char_start,
                    char_end=char_end,
                    content_sha256=content_sha256,
                    matched_terms=terms,
                    candidate_key=hashlib.sha256(key_material.encode("utf-8")).hexdigest(),
                )
            )
            emitted.add(result_kind)
            if result_kind == "progress_and_preservation":
                emitted.update(("progress", "preservation"))
            if len(candidates) >= MAX_CANDIDATES_PER_DOCUMENT:
                return candidates
    return candidates


def claim_job(connection: psycopg.Connection[Any], worker_id: str) -> dict[str, Any] | None:
    with connection.transaction(), connection.cursor(row_factory=dict_row) as cursor:
        cursor.execute(
            """
            SELECT id, job_type, payload, attempts, max_attempts
            FROM jobs
            WHERE (
                status IN ('queued', 'failed') AND available_at <= now()
                OR status = 'running' AND leased_until < now()
            )
            AND attempts < max_attempts
            ORDER BY created_at
            FOR UPDATE SKIP LOCKED
            LIMIT 1
            """
        )
        job = cursor.fetchone()
        if job is None:
            return None
        cursor.execute(
            """
            UPDATE jobs SET status = 'running', worker_id = %s,
                leased_until = now() + make_interval(secs => %s),
                attempts = attempts + 1, updated_at = now()
            WHERE id = %s
            """,
            (worker_id, LEASE_SECONDS, job["id"]),
        )
        return job


def persist_success(
    connection: psycopg.Connection[Any],
    job_id: UUID,
    document_id: UUID,
    processed: ProcessedDocument,
) -> None:
    with connection.transaction(), connection.cursor(row_factory=dict_row) as cursor:
        cursor.execute(
            "SELECT work_id FROM documents WHERE id = %s AND ml_use_permitted = true",
            (document_id,),
        )
        document = cursor.fetchone()
        if document is None:
            raise ValueError("document is missing or no longer permits ML processing")
        cursor.execute(
            """
            INSERT INTO pipeline_runs
                (pipeline_name, pipeline_version, configuration)
            VALUES ('local-proof-candidate-extraction', %s, %s)
            RETURNING id
            """,
            (PIPELINE_VERSION, json.dumps({"kind": "deterministic_term_baseline"})),
        )
        pipeline_run_id = cursor.fetchone()["id"]
        for candidate in processed.candidates:
            summary = (
                f"Page contains terminology associated with {candidate.result_kind.replace('_', ' ')}; "
                "curator verification is required."
            )
            cursor.execute(
                """
                INSERT INTO proof_results
                    (work_id, result_kind, statement_summary, source_document_id,
                     pipeline_run_id, candidate_key)
                VALUES (%s, %s, %s, %s, %s, %s)
                ON CONFLICT (candidate_key) DO NOTHING
                RETURNING id
                """,
                (
                    document["work_id"],
                    candidate.result_kind,
                    summary,
                    document_id,
                    pipeline_run_id,
                    candidate.candidate_key,
                ),
            )
            inserted = cursor.fetchone()
            if inserted is None:
                continue
            proof_result_id = inserted["id"]
            cursor.execute(
                """
                INSERT INTO document_spans
                    (document_id, page_start, page_end, section_path, char_start,
                     char_end, content_sha256)
                VALUES (%s, %s, %s, %s, %s, %s, %s)
                RETURNING id
                """,
                (
                    document_id,
                    candidate.page,
                    candidate.page,
                    f"page:{candidate.page}",
                    candidate.char_start,
                    candidate.char_end,
                    candidate.content_sha256,
                ),
            )
            span_id = cursor.fetchone()["id"]
            cursor.execute(
                """
                INSERT INTO evidence
                    (proof_result_id, document_span_id, paraphrase, created_by)
                VALUES (%s, %s, %s, %s)
                """,
                (
                    proof_result_id,
                    span_id,
                    "Local baseline matched terminology: " + ", ".join(candidate.matched_terms),
                    PIPELINE_VERSION,
                ),
            )
            obligations = {
                "progress_and_preservation": ("progress", "preservation"),
                "progress": ("progress",),
                "preservation": ("preservation",),
                "type_soundness": ("type_soundness",),
            }[candidate.result_kind]
            for obligation in obligations:
                cursor.execute(
                    """
                    INSERT INTO proof_obligations (proof_result_id, obligation_kind)
                    VALUES (%s, %s)
                    """,
                    (proof_result_id, obligation),
                )
        cursor.execute(
            "UPDATE pipeline_runs SET finished_at = now() WHERE id = %s",
            (pipeline_run_id,),
        )
        cursor.execute(
            """
            UPDATE jobs SET status = 'succeeded', result = %s,
                leased_until = NULL, updated_at = now()
            WHERE id = %s
            """,
            (json.dumps(processed.result), job_id),
        )


def fail_job(connection: psycopg.Connection[Any], job: dict[str, Any], error: Exception) -> None:
    terminal = job["attempts"] + 1 >= job["max_attempts"]
    with connection.transaction():
        connection.execute(
            """
            UPDATE jobs SET status = %s::job_status, last_error = %s,
                available_at = now() + make_interval(secs => LEAST(300, attempts * attempts * 5)),
                leased_until = NULL, updated_at = now()
            WHERE id = %s
            """,
            ("dead_letter" if terminal else "failed", str(error)[:2000], job["id"]),
        )


def process_job(job: dict[str, Any], data_root: Path) -> ProcessedDocument:
    if job["job_type"] != "extract_document":
        raise ValueError(f"unsupported job type: {job['job_type']}")
    payload = job["payload"]
    path = safe_object_path(data_root, payload["object_key"])
    return extract_pdf(path, data_root / "derived", UUID(str(payload["document_id"])))


def run() -> None:
    database_url = os.environ["DATABASE_URL"]
    data_root = Path(os.getenv("DATA_ROOT", "/data"))
    worker_id = os.getenv("WORKER_ID", f"{socket.gethostname()}-{os.getpid()}")
    poll_seconds = float(os.getenv("WORKER_POLL_SECONDS", "2"))
    LOG.info("worker started", extra={"worker_id": worker_id})

    while True:
        try:
            with psycopg.connect(database_url, autocommit=False) as connection:
                while True:
                    job = claim_job(connection, worker_id)
                    if job is None:
                        time.sleep(poll_seconds)
                        continue
                    try:
                        processed = process_job(job, data_root)
                        document_id = UUID(str(job["payload"]["document_id"]))
                        persist_success(connection, job["id"], document_id, processed)
                    except Exception as error:  # job boundary: failure is persisted
                        LOG.exception("job failed", extra={"job_id": str(job["id"])})
                        fail_job(connection, job, error)
        except psycopg.Error:
            LOG.exception("database connection failed; retrying")
            time.sleep(5)


def main() -> None:
    logging.basicConfig(
        level=os.getenv("LOG_LEVEL", "INFO"),
        format="%(asctime)s %(levelname)s %(name)s %(message)s",
    )
    try:
        run()
    except KeyboardInterrupt:
        LOG.info("worker stopped")


if __name__ == "__main__":
    main()

