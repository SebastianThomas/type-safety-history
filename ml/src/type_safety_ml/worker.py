from __future__ import annotations

import hashlib
import json
import logging
import os
import socket
import time
from pathlib import Path
from typing import Any
from uuid import UUID

import psycopg
import pymupdf
from psycopg.rows import dict_row

LOG = logging.getLogger("type_safety_ml.worker")
LEASE_SECONDS = 300


def safe_object_path(data_root: Path, object_key: str) -> Path:
    candidate = (data_root / object_key).resolve()
    root = data_root.resolve()
    if not candidate.is_relative_to(root):
        raise ValueError("object key escapes DATA_ROOT")
    if not candidate.is_file():
        raise ValueError("object key is not a local regular file")
    return candidate


def extract_pdf(path: Path, derived_root: Path, document_id: UUID) -> dict[str, Any]:
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
    return {
        "derived_object_key": str(destination.relative_to(derived_root.parent)),
        "sha256": digest,
        "page_count": len(pages),
    }


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


def complete_job(connection: psycopg.Connection[Any], job_id: UUID, result: dict[str, Any]) -> None:
    with connection.transaction():
        connection.execute(
            """
            UPDATE jobs SET status = 'succeeded', result = %s,
                leased_until = NULL, updated_at = now()
            WHERE id = %s
            """,
            (json.dumps(result), job_id),
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


def process_job(job: dict[str, Any], data_root: Path) -> dict[str, Any]:
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
                        result = process_job(job, data_root)
                        complete_job(connection, job["id"], result)
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

