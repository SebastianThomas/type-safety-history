from __future__ import annotations

import hashlib
import io
import json
import logging
import os
import re
import shutil
import socket
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any
from uuid import UUID

import numpy as np
import psycopg
import pyarrow as pa
import pyarrow.parquet as pq
import pypdfium2 as pdfium
import pytesseract
import sklearn
import zarr
from PIL import Image
from psycopg.rows import dict_row
from pypdf import PdfReader
from sklearn.feature_extraction.text import HashingVectorizer, TfidfVectorizer
from sklearn.metrics.pairwise import cosine_similarity
from sklearn.preprocessing import normalize

LOG = logging.getLogger("type_safety_ml.worker")
LEASE_SECONDS = 300
PIPELINE_VERSION = "local-term-baseline-v1"
EMBEDDING_ALGORITHM = "sklearn-hashing-word-1-2-v1"
EMBEDDING_DIMENSIONS = 512
MAX_CANDIDATES_PER_DOCUMENT = 25
DOI_PATTERN = re.compile(r"\b10\.\d{4,9}/[-._;()/:A-Z0-9]+", re.IGNORECASE)
DISCOVERY_TRIAGE_VERSION = "local-tfidf-hybrid-v1"

DISCOVERY_STRONG_PHRASES = (
    "type soundness",
    "type safety",
    "subject reduction",
    "progress and preservation",
    "progress & preservation",
    "syntactic approach to type soundness",
    "well typed programs cannot go wrong",
    "well-typed programs cannot go wrong",
)
DISCOVERY_CONTEXT_TERMS = (
    "programming language",
    "type system",
    "typed calculus",
    "lambda calculus",
    "operational semantics",
    "static semantics",
    "preservation theorem",
    "progress theorem",
    "type theorem",
)
DISCOVERY_NEGATIVE_TERMS = (
    "patient safety",
    "food safety",
    "road safety",
    "traffic safety",
    "drug safety",
    "construction safety",
    "aviation safety",
    "nuclear safety",
    "occupational safety",
    "workplace safety",
    "public safety",
    "fire safety",
    "water safety",
    "safety management",
)
DISCOVERY_POSITIVE_PROTOTYPES = (
    "A syntactic approach to type soundness",
    "Type soundness by progress and preservation",
    "Subject reduction for a typed lambda calculus",
    "Safety of a programming language type system",
    "Operational semantics and type preservation",
    "Well-typed programs cannot go wrong",
)
DISCOVERY_NEGATIVE_PROTOTYPES = (
    "Patient safety management in hospitals",
    "Food safety and public health",
    "Road traffic safety assessment",
    "Drug safety in clinical practice",
    "Occupational and workplace safety",
    "Aviation safety management systems",
    "Soundness examination in animal breeding",
    "Syntactic processing in human linguistics",
)


@dataclass(frozen=True)
class Candidate:
    page: int
    result_kind: str
    char_start: int
    char_end: int
    content_sha256: str
    matched_terms: tuple[str, ...]
    candidate_key: str
    confidence: float


@dataclass(frozen=True)
class CitationCandidate:
    page: int
    doi: str
    context_sha256: str


@dataclass(frozen=True)
class ProcessedDocument:
    result: dict[str, Any]
    candidates: tuple[Candidate, ...]
    citations: tuple[CitationCandidate, ...]
    embedding: tuple[float, ...]
    zarr_object_key: str
    zarr_manifest_sha256: str
    ocr_page_count: int


def score_discovery_candidate(title: str, query_texts: list[str]) -> tuple[float, str]:
    """Rank metadata locally without changing a candidate's review status."""
    text = " ".join([title, *query_texts]).lower().replace("–", "-").replace("—", "-")
    title_text = title.lower().replace("–", "-").replace("—", "-")
    score = 0.0
    score += min(0.7, 0.55 * sum(phrase in title_text for phrase in DISCOVERY_STRONG_PHRASES))
    score += min(0.3, 0.15 * sum(term in title_text for term in DISCOVERY_CONTEXT_TERMS))
    if "progress" in title_text and "preservation" in title_text:
        score += 0.45
    if "soundness" in title_text and ("type" in title_text or "typed" in title_text):
        score += 0.4
    if "safety" in title_text and ("type" in title_text or "typed" in title_text):
        score += 0.3
    if "type polymorphism" in title_text or "typed programming" in title_text:
        score += 0.25
    if any(term in title_text for term in DISCOVERY_NEGATIVE_TERMS):
        score -= 0.75
    # A matching discovery query is weak evidence; it must never make a generic title likely.
    if any(phrase in text for phrase in DISCOVERY_STRONG_PHRASES):
        score += 0.08
    score = round(min(1.0, max(0.0, score)), 4)
    if score >= 0.7:
        category = "likely_relevant"
    elif score >= 0.25:
        category = "possible"
    else:
        category = "unlikely"
    return score, category


def classify_discovery_candidates(
    candidates: list[dict[str, Any]],
) -> list[tuple[float, str]]:
    """Apply a local TF-IDF prototype model with conservative lexical features."""
    if not candidates:
        return []
    prototypes = [*DISCOVERY_POSITIVE_PROTOTYPES, *DISCOVERY_NEGATIVE_PROTOTYPES]
    titles = [candidate["title"] for candidate in candidates]
    matrix = TfidfVectorizer(analyzer="char_wb", ngram_range=(3, 5)).fit_transform(
        [*prototypes, *titles]
    )
    candidate_vectors = matrix[len(prototypes) :]
    positive_vectors = matrix[: len(DISCOVERY_POSITIVE_PROTOTYPES)]
    negative_vectors = matrix[len(DISCOVERY_POSITIVE_PROTOTYPES) : len(prototypes)]
    positive_scores = cosine_similarity(candidate_vectors, positive_vectors).max(axis=1)
    negative_scores = cosine_similarity(candidate_vectors, negative_vectors).max(axis=1)
    classified = []
    for index, candidate in enumerate(candidates):
        lexical_score, _ = score_discovery_candidate(candidate["title"], candidate["query_texts"])
        score = round(
            min(
                1.0,
                max(
                    0.0,
                    lexical_score * 0.72
                    + float(positive_scores[index]) * 0.5
                    - float(negative_scores[index]) * 0.4,
                ),
            ),
            4,
        )
        category = (
            "likely_relevant" if score >= 0.7 else "possible" if score >= 0.25 else "unlikely"
        )
        classified.append((score, category))
    return classified


def normalize_orcid(value: str | None) -> str | None:
    if not value:
        return None
    normalized = value.removeprefix("https://orcid.org/").removeprefix("http://orcid.org/")
    return normalized or None


def promote_discovery_candidate(
    cursor: psycopg.Cursor[Any], candidate: dict[str, Any], pipeline_run_id: UUID
) -> UUID:
    """Promote high-confidence metadata to an explicitly unreviewed candidate work."""
    metadata = candidate["raw_metadata"]
    doi_value = (
        candidate["external_id"] if candidate["provider"] == "crossref" else metadata.get("doi")
    )
    doi = (
        str(doi_value).removeprefix("https://doi.org/").removeprefix("http://doi.org/").lower()
        if doi_value
        else None
    )
    existing_work_id: UUID | None = None
    if doi:
        cursor.execute("SELECT id FROM works WHERE doi = %s", (doi,))
        existing = cursor.fetchone()
        existing_work_id = existing["id"] if existing else None
    if existing_work_id is None:
        cursor.execute(
            """
            SELECT work_id AS id
            FROM entity_resolution_suggestions
            WHERE discovery_candidate_id = %s AND status = 'pending' AND score >= 0.9
            ORDER BY score DESC LIMIT 1
            """,
            (candidate["id"],),
        )
        existing = cursor.fetchone()
        existing_work_id = existing["id"] if existing else None
    if existing_work_id is None and not doi:
        cursor.execute(
            """
            SELECT id FROM works
            WHERE lower(title) = lower(%s)
              AND publication_year IS NOT DISTINCT FROM %s
              AND merged_into_work_id IS NULL
            ORDER BY created_at LIMIT 1
            """,
            (candidate["title"], candidate["publication_year"]),
        )
        existing = cursor.fetchone()
        existing_work_id = existing["id"] if existing else None
    is_new_work = existing_work_id is None
    if existing_work_id is not None:
        work_id = existing_work_id
    else:
        if doi:
            cursor.execute(
                """
                INSERT INTO works (title, work_type, publication_year, doi)
                VALUES (%s, %s::work_type, %s, %s) RETURNING id
                """,
                (
                    candidate["title"],
                    candidate["work_type"],
                    candidate["publication_year"],
                    doi,
                ),
            )
        else:
            cursor.execute(
                """
                INSERT INTO works (title, work_type, publication_year)
                VALUES (%s, %s::work_type, %s) RETURNING id
                """,
                (candidate["title"], candidate["work_type"], candidate["publication_year"]),
            )
        work_id = cursor.fetchone()["id"]

    if candidate["provider"] == "crossref":
        source_url = f"https://api.crossref.org/v1/works/{candidate['external_id']}"
        source_kind = "doi_registry"
        terms_url = "https://www.crossref.org/documentation/retrieve-metadata/rest-api/"
        rights_basis = "Crossref REST API metadata reuse; full text is not acquired."
    else:
        source_url = str(metadata.get("openalex_id") or candidate["external_id"])
        source_kind = "bibliography"
        terms_url = "https://docs.openalex.org/additional-help/faq#how-is-openalex-licensed"
        rights_basis = "OpenAlex bibliographic metadata is provided under CC0."
    cursor.execute(
        """
        INSERT INTO sources
            (work_id, url, source_kind, reliability, rights_status, terms_url,
             raw_metadata, rights_basis, ml_use_permitted, redistribution_permitted)
        VALUES (%s, %s, %s::source_kind, 'authoritative_metadata', 'open_metadata_only',
                %s, %s, %s, false, true)
        ON CONFLICT (url, work_id) DO UPDATE
        SET raw_metadata = EXCLUDED.raw_metadata, acquired_at = now()
        """,
        (work_id, source_url, source_kind, terms_url, json.dumps(metadata), rights_basis),
    )

    authors = metadata.get("authors") or [] if is_new_work else []
    for position, author in enumerate(authors):
        display_name = author.get("display_name") or " ".join(
            part for part in (author.get("given"), author.get("family")) if part
        )
        display_name = display_name.strip()
        if not display_name:
            continue
        orcid = normalize_orcid(author.get("orcid") or author.get("ORCID"))
        if orcid:
            cursor.execute(
                """
                INSERT INTO people (display_name, orcid) VALUES (%s, %s)
                ON CONFLICT (orcid) DO UPDATE SET display_name = EXCLUDED.display_name
                RETURNING id
                """,
                (display_name, orcid),
            )
        else:
            cursor.execute(
                "INSERT INTO people (display_name) VALUES (%s) RETURNING id",
                (display_name,),
            )
        person_id = cursor.fetchone()["id"]
        cursor.execute(
            """
            INSERT INTO work_authors (work_id, person_id, position)
            VALUES (%s, %s, %s) ON CONFLICT DO NOTHING
            """,
            (work_id, person_id, position),
        )

    cursor.execute(
        """
        UPDATE discovery_candidates
        SET status = 'imported', work_id = %s, reviewed_at = now(),
            review_reason = %s
        WHERE id = %s AND status = 'pending'
        """,
        (
            work_id,
            f"Automatically promoted by {DISCOVERY_TRIAGE_VERSION}; work remains a machine candidate",
            candidate["id"],
        ),
    )
    cursor.execute(
        """
        INSERT INTO audit_log
            (actor, action, entity_type, entity_id, after_value, reason)
        VALUES (%s, 'auto_promote_metadata', 'work', %s, %s, %s)
        """,
        (
            f"ml:{DISCOVERY_TRIAGE_VERSION}",
            work_id,
            json.dumps(
                {
                    "discovery_candidate_id": str(candidate["id"]),
                    "pipeline_run_id": str(pipeline_run_id),
                    "review_status": "candidate",
                }
            ),
            "High-confidence local relevance classification; proof claims remain unverified",
        ),
    )
    cursor.execute(
        """
        UPDATE entity_resolution_suggestions
        SET status = CASE WHEN work_id = %s THEN 'accepted'::resolution_status
                          ELSE 'rejected'::resolution_status END,
            resolved_by = %s, resolved_at = now(),
            resolution_reason = 'Resolved during automated bibliographic promotion'
        WHERE discovery_candidate_id = %s AND status = 'pending'
        """,
        (work_id, f"ml:{DISCOVERY_TRIAGE_VERSION}", candidate["id"]),
    )
    priority = round(float(candidate.get("relevance_score") or 0.5) * 100)
    cursor.execute(
        """
        INSERT INTO source_resolution_queue (work_id, priority)
        VALUES (%s, %s)
        ON CONFLICT (work_id) DO UPDATE
        SET priority = GREATEST(source_resolution_queue.priority, EXCLUDED.priority),
            status = CASE WHEN source_resolution_queue.status = 'resolved'
                          THEN source_resolution_queue.status ELSE 'queued' END,
            available_at = CASE WHEN source_resolution_queue.status = 'resolved'
                                THEN source_resolution_queue.available_at ELSE now() END,
            updated_at = now()
        """,
        (work_id, priority),
    )
    return work_id


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
    ocr_page_count = 0
    reader = PdfReader(path)
    if reader.is_encrypted:
        raise ValueError("encrypted PDFs are not processed")
    render_document: pdfium.PdfDocument | None = None
    try:
        for number, page in enumerate(reader.pages, start=1):
            text = page.extract_text() or ""
            extraction_method = "embedded_text"
            if len(text.strip()) < 40 and shutil.which("tesseract"):
                render_document = render_document or pdfium.PdfDocument(path)
                render_page = render_document[number - 1]
                bitmap = render_page.render(scale=200 / 72)
                rendered = bitmap.to_pil()
                buffer = io.BytesIO()
                rendered.save(buffer, format="PNG")
                image = Image.open(io.BytesIO(buffer.getvalue()))
                ocr_text = pytesseract.image_to_string(image, lang="eng")
                bitmap.close()
                render_page.close()
                if len(ocr_text.strip()) > len(text.strip()):
                    text = ocr_text
                    extraction_method = "tesseract_ocr"
                    ocr_page_count += 1
            pages.append({"page": number, "text": text, "method": extraction_method})
    finally:
        if render_document is not None:
            render_document.close()

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
    citations = extract_citation_candidates(pages)
    zarr_object_key, zarr_manifest_sha256, embedding = write_embeddings(
        derived_root.parent, document_id, pages
    )
    return ProcessedDocument(
        result={
            "derived_object_key": str(destination.relative_to(derived_root.parent)),
            "sha256": digest,
            "page_count": len(pages),
            "candidate_count": len(candidates),
            "pipeline_version": PIPELINE_VERSION,
            "embedding_algorithm": EMBEDDING_ALGORITHM,
            "ocr_page_count": ocr_page_count,
        },
        candidates=tuple(candidates),
        citations=tuple(citations),
        embedding=tuple(float(value) for value in embedding),
        zarr_object_key=zarr_object_key,
        zarr_manifest_sha256=zarr_manifest_sha256,
        ocr_page_count=ocr_page_count,
    )


def write_embeddings(
    data_root: Path, document_id: UUID, pages: list[dict[str, Any]]
) -> tuple[str, str, np.ndarray[Any, np.dtype[np.float32]]]:
    texts = [str(page["text"]) for page in pages] or [""]
    vectorizer = HashingVectorizer(
        n_features=EMBEDDING_DIMENSIONS,
        alternate_sign=False,
        norm="l2",
        ngram_range=(1, 2),
        stop_words="english",
    )
    page_vectors = vectorizer.transform(texts).toarray().astype(np.float32)
    document_vector = page_vectors.mean(axis=0, dtype=np.float32).reshape(1, -1)
    document_vector = normalize(document_vector, norm="l2").astype(np.float32)[0]
    relative = Path("zarr") / "document-embeddings" / f"{document_id}.zarr"
    destination = data_root / relative
    temporary = destination.with_name(destination.name + ".tmp")
    if temporary.exists():
        shutil.rmtree(temporary)
    if destination.exists():
        shutil.rmtree(destination)
    temporary.parent.mkdir(parents=True, exist_ok=True)
    group = zarr.open_group(temporary, mode="w")
    group.create_array(
        "page_embeddings",
        data=page_vectors,
        chunks=(max(1, min(64, len(page_vectors))), EMBEDDING_DIMENSIONS),
    )
    group.create_array("document_embedding", data=document_vector)
    group.attrs.update(
        {
            "algorithm": EMBEDDING_ALGORITHM,
            "dimensions": EMBEDDING_DIMENSIONS,
            "document_id": str(document_id),
            "page_count": len(pages),
            "library": f"scikit-learn {sklearn.__version__}",
            "license": "BSD-3-Clause",
        }
    )
    temporary.replace(destination)
    manifest = hashlib.sha256()
    for file in sorted(path for path in destination.rglob("*") if path.is_file()):
        manifest.update(str(file.relative_to(destination)).encode("utf-8"))
        manifest.update(hashlib.sha256(file.read_bytes()).digest())
    return str(relative), manifest.hexdigest(), document_vector


def extract_citation_candidates(pages: list[dict[str, Any]]) -> list[CitationCandidate]:
    candidates: list[CitationCandidate] = []
    seen: set[tuple[int, str]] = set()
    for page in pages:
        text = str(page["text"])
        for match in DOI_PATTERN.finditer(text):
            doi = match.group(0).rstrip(".,;:)]}").lower()
            key = (int(page["page"]), doi)
            if key in seen:
                continue
            context = text[max(0, match.start() - 160) : min(len(text), match.end() + 160)]
            candidates.append(
                CitationCandidate(
                    page=key[0],
                    doi=doi,
                    context_sha256=hashlib.sha256(context.encode("utf-8")).hexdigest(),
                )
            )
            seen.add(key)
    return candidates


def classify_pages(document_id: UUID, pages: list[dict[str, Any]]) -> list[Candidate]:
    candidates: list[Candidate] = []
    patterns = (
        ("progress_and_preservation", ("progress", "preservation"), 0.92),
        ("preservation", ("subject reduction",), 0.88),
        ("type_soundness", ("type soundness",), 0.86),
        ("type_soundness", ("well-typed", "go wrong"), 0.84),
        ("preservation", ("preservation",), 0.72),
        ("progress", ("progress",), 0.70),
    )
    for page in pages:
        text = str(page["text"])
        lowered = text.casefold()
        emitted: set[str] = set()
        for result_kind, terms, confidence in patterns:
            if result_kind in emitted or not all(term in lowered for term in terms):
                continue
            offsets = [lowered.index(term) for term in terms]
            char_start = min(offsets)
            char_end = min(
                len(text), max(offsets[index] + len(term) for index, term in enumerate(terms))
            )
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
                    confidence=confidence,
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
        if job["job_type"] == "export_approved_proofs_parquet":
            cursor.execute(
                "UPDATE exports SET status = 'running', error = NULL WHERE id = %s",
                (job["payload"]["export_id"],),
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
            "SELECT work_id FROM documents WHERE id = %s AND ml_use_permitted = true "
            "AND status = 'active'",
            (document_id,),
        )
        document = cursor.fetchone()
        if document is None:
            raise ValueError("document is missing or no longer permits ML processing")
        cursor.execute(
            """
            INSERT INTO pipeline_runs
                (pipeline_name, pipeline_version, model_name, model_version,
                 model_license, configuration)
            VALUES ('local-proof-candidate-extraction', %s, 'scikit-learn', %s,
                    'BSD-3-Clause', %s)
            RETURNING id
            """,
            (
                PIPELINE_VERSION,
                sklearn.__version__,
                json.dumps(
                    {
                        "kind": "deterministic_term_baseline",
                        "embedding_algorithm": EMBEDDING_ALGORITHM,
                        "embedding_dimensions": EMBEDDING_DIMENSIONS,
                        "hosted_model": False,
                    }
                ),
            ),
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
                     pipeline_run_id, candidate_key, confidence, classifier_version)
                VALUES (%s, %s, %s, %s, %s, %s, %s, %s)
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
                    candidate.confidence,
                    PIPELINE_VERSION,
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
            """
            INSERT INTO document_embeddings
                (document_id, algorithm, dimensions, vector, zarr_object_key,
                 zarr_manifest_sha256, pipeline_run_id)
            VALUES (%s, %s, %s, %s, %s, %s, %s)
            ON CONFLICT (document_id) DO UPDATE SET
                algorithm = EXCLUDED.algorithm, dimensions = EXCLUDED.dimensions,
                vector = EXCLUDED.vector, zarr_object_key = EXCLUDED.zarr_object_key,
                zarr_manifest_sha256 = EXCLUDED.zarr_manifest_sha256,
                pipeline_run_id = EXCLUDED.pipeline_run_id, created_at = now()
            """,
            (
                document_id,
                EMBEDDING_ALGORITHM,
                EMBEDDING_DIMENSIONS,
                list(processed.embedding),
                processed.zarr_object_key,
                processed.zarr_manifest_sha256,
                pipeline_run_id,
            ),
        )
        cursor.execute(
            """
            SELECT document_id, vector FROM document_embeddings
            WHERE document_id <> %s AND algorithm = %s AND dimensions = %s
            """,
            (document_id, EMBEDDING_ALGORITHM, EMBEDDING_DIMENSIONS),
        )
        current = np.asarray(processed.embedding, dtype=np.float32)
        for other in cursor.fetchall():
            other_id = other["document_id"]
            score = float(np.clip(np.dot(current, np.asarray(other["vector"])), 0.0, 1.0))
            left_id, right_id = sorted((document_id, other_id))
            cursor.execute(
                """
                INSERT INTO document_similarities
                    (left_document_id, right_document_id, score, algorithm)
                VALUES (%s, %s, %s, %s)
                ON CONFLICT (left_document_id, right_document_id, algorithm)
                DO UPDATE SET score = EXCLUDED.score, created_at = now()
                """,
                (left_id, right_id, score, EMBEDDING_ALGORITHM),
            )
        for citation in processed.citations:
            cursor.execute(
                """
                INSERT INTO citation_candidates
                    (citing_work_id, source_document_id, page, cited_doi,
                     context_sha256, created_by)
                VALUES (%s, %s, %s, %s, %s, %s)
                ON CONFLICT (source_document_id, page, cited_doi) DO NOTHING
                """,
                (
                    document["work_id"],
                    document_id,
                    citation.page,
                    citation.doi,
                    citation.context_sha256,
                    PIPELINE_VERSION,
                ),
            )
        cursor.execute(
            """
            UPDATE documents SET extraction_method = %s, ocr_page_count = %s,
                processed_at = now() WHERE id = %s
            """,
            (
                "embedded_text_with_ocr_fallback" if processed.ocr_page_count else "embedded_text",
                processed.ocr_page_count,
                document_id,
            ),
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
        if job["job_type"] == "export_approved_proofs_parquet":
            connection.execute(
                "UPDATE exports SET status = 'failed', error = %s WHERE id = %s",
                (str(error)[:2000], job["payload"]["export_id"]),
            )


def process_job(job: dict[str, Any], data_root: Path) -> ProcessedDocument:
    if job["job_type"] != "extract_document":
        raise ValueError(f"unsupported job type: {job['job_type']}")
    payload = job["payload"]
    path = safe_object_path(data_root, payload["object_key"])
    return extract_pdf(path, data_root / "derived", UUID(str(payload["document_id"])))


def export_approved_proofs(
    connection: psycopg.Connection[Any], data_root: Path, export_id: UUID
) -> tuple[str, str, int]:
    with connection.transaction(), connection.cursor(row_factory=dict_row) as cursor:
        cursor.execute(
            """
            SELECT
                w.id::text AS work_id,
                w.title,
                w.publication_year,
                w.doi,
                pr.id::text AS proof_result_id,
                pr.theorem_name,
                pr.result_kind,
                pr.proof_method,
                pr.statement_summary,
                l.name AS language,
                ds.page_start,
                ds.page_end,
                ds.char_start,
                ds.char_end,
                e.paraphrase AS evidence_paraphrase,
                s.url AS source_url,
                s.reliability::text AS source_reliability,
                s.rights_status::text AS source_rights_status
            FROM proof_results pr
            JOIN works w ON w.id = pr.work_id
            LEFT JOIN languages l ON l.id = pr.language_id
            LEFT JOIN evidence e ON e.proof_result_id = pr.id
            LEFT JOIN document_spans ds ON ds.id = e.document_span_id
            LEFT JOIN documents d ON d.id = ds.document_id
            LEFT JOIN sources s ON s.id = d.source_id
            WHERE pr.status = 'curator_approved'
            ORDER BY w.publication_year NULLS LAST, w.title, pr.id, ds.page_start
            """
        )
        rows = [dict(row) for row in cursor.fetchall()]

    output_dir = data_root / "parquet" / "approved-proofs"
    output_dir.mkdir(parents=True, exist_ok=True)
    destination = output_dir / f"{export_id}.parquet"
    temporary = output_dir / f"{export_id}.parquet.tmp"
    schema = pa.schema(
        [
            ("work_id", pa.string()),
            ("title", pa.string()),
            ("publication_year", pa.int32()),
            ("doi", pa.string()),
            ("proof_result_id", pa.string()),
            ("theorem_name", pa.string()),
            ("result_kind", pa.string()),
            ("proof_method", pa.string()),
            ("statement_summary", pa.string()),
            ("language", pa.string()),
            ("page_start", pa.int32()),
            ("page_end", pa.int32()),
            ("char_start", pa.int32()),
            ("char_end", pa.int32()),
            ("evidence_paraphrase", pa.string()),
            ("source_url", pa.string()),
            ("source_reliability", pa.string()),
            ("source_rights_status", pa.string()),
        ]
    )
    table = pa.Table.from_pylist(rows, schema=schema)
    pq.write_table(table, temporary, compression="zstd")
    temporary.replace(destination)
    digest = hashlib.sha256()
    with destination.open("rb") as exported:
        for chunk in iter(lambda: exported.read(1024 * 1024), b""):
            digest.update(chunk)
    return (
        str(destination.relative_to(data_root)),
        digest.hexdigest(),
        table.num_rows,
    )


def persist_export_success(
    connection: psycopg.Connection[Any],
    job_id: UUID,
    export_id: UUID,
    object_key: str,
    sha256: str,
    row_count: int,
) -> None:
    with connection.transaction():
        connection.execute(
            """
            UPDATE exports SET status = 'succeeded', object_key = %s, sha256 = %s,
                row_count = %s, finished_at = now(), error = NULL
            WHERE id = %s
            """,
            (object_key, sha256, row_count, export_id),
        )
        connection.execute(
            """
            UPDATE jobs SET status = 'succeeded', result = %s,
                leased_until = NULL, updated_at = now()
            WHERE id = %s
            """,
            (
                json.dumps({"object_key": object_key, "sha256": sha256, "row_count": row_count}),
                job_id,
            ),
        )


def resolve_discovery_entities(connection: psycopg.Connection[Any]) -> dict[str, int]:
    with connection.transaction(), connection.cursor(row_factory=dict_row) as cursor:
        cursor.execute(
            "SELECT id, title, publication_year FROM discovery_candidates WHERE status = 'pending'"
        )
        candidates = cursor.fetchall()
        cursor.execute(
            "SELECT id, title, publication_year FROM works WHERE merged_into_work_id IS NULL"
        )
        works = cursor.fetchall()
        if not candidates or not works:
            return {"candidates": len(candidates), "suggestions": 0}
        texts = [row["title"] for row in candidates] + [row["title"] for row in works]
        matrix = TfidfVectorizer(analyzer="char_wb", ngram_range=(3, 5)).fit_transform(texts)
        scores = cosine_similarity(matrix[: len(candidates)], matrix[len(candidates) :])
        stored = 0
        for candidate_index, candidate in enumerate(candidates):
            for work_index in np.argsort(scores[candidate_index])[-3:][::-1]:
                score = float(scores[candidate_index, work_index])
                work = works[int(work_index)]
                year_match = (
                    candidate["publication_year"] is not None
                    and candidate["publication_year"] == work["publication_year"]
                )
                adjusted = min(1.0, score + (0.05 if year_match else 0.0))
                if adjusted < 0.55:
                    continue
                cursor.execute(
                    """
                    INSERT INTO entity_resolution_suggestions
                        (discovery_candidate_id, work_id, score, method, rationale)
                    VALUES (%s, %s, %s, %s, %s)
                    ON CONFLICT (discovery_candidate_id, work_id, method)
                    DO UPDATE SET score = EXCLUDED.score, rationale = EXCLUDED.rationale
                    """,
                    (
                        candidate["id"],
                        work["id"],
                        adjusted,
                        "sklearn-char-tfidf-3-5-v1",
                        json.dumps({"title_score": score, "publication_year_match": year_match}),
                    ),
                )
                stored += 1
        return {"candidates": len(candidates), "suggestions": stored}


def triage_discovery_candidates(connection: psycopg.Connection[Any]) -> dict[str, int]:
    """Promote strong metadata, defer ambiguity, and reversibly screen noise."""
    with connection.transaction(), connection.cursor(row_factory=dict_row) as cursor:
        cursor.execute(
            """
            INSERT INTO pipeline_runs
                (pipeline_name, pipeline_version, model_name, model_version, configuration)
            VALUES ('discovery_relevance_triage', %s, 'scikit-learn-char-tfidf', %s, %s)
            RETURNING id
            """,
            (
                DISCOVERY_TRIAGE_VERSION,
                DISCOVERY_TRIAGE_VERSION,
                json.dumps(
                    {
                        "input": "bibliographic titles and discovery query labels",
                        "network_access": False,
                        "automatic_actions": {
                            "likely_relevant": "promote_candidate_work",
                            "possible": "promote_low_priority_candidate_work",
                            "unlikely": "screen_out",
                        },
                    }
                ),
            ),
        )
        pipeline_run_id = cursor.fetchone()["id"]
        cursor.execute(
            """
            SELECT dc.id, dc.provider, dc.external_id, dc.title, dc.publication_year,
                   dc.work_type::text AS work_type, dc.raw_metadata,
                   COALESCE(array_agg(dh.query_text ORDER BY dh.query_text)
                       FILTER (WHERE dh.query_text IS NOT NULL), ARRAY[]::text[]) AS query_texts
            FROM discovery_candidates dc
            LEFT JOIN discovery_hits dh ON dh.candidate_id = dc.id
            WHERE dc.status = 'pending'
              AND (dc.triaged_at IS NULL OR dc.triage_version <> %s)
            GROUP BY dc.id
            ORDER BY dc.id
            """,
            (DISCOVERY_TRIAGE_VERSION,),
        )
        candidates = cursor.fetchall()
        counts = {"likely_relevant": 0, "possible": 0, "unlikely": 0}
        updates: list[tuple[float, str, str, UUID]] = []
        classifications = classify_discovery_candidates(candidates)
        for candidate, (score, category) in zip(candidates, classifications, strict=True):
            candidate["relevance_score"] = score
            candidate["relevance_category"] = category
            updates.append((score, category, DISCOVERY_TRIAGE_VERSION, candidate["id"]))
            counts[category] += 1
        cursor.executemany(
            """
            UPDATE discovery_candidates
            SET relevance_score = %s, relevance_category = %s,
                triage_version = %s, triaged_at = now()
            WHERE id = %s
            """,
            updates,
        )
        promoted_work_ids = [
            promote_discovery_candidate(cursor, candidate, pipeline_run_id)
            for candidate in candidates
            if candidate["relevance_category"] in {"likely_relevant", "possible"}
        ]
        screened_ids = [
            (candidate["id"],)
            for candidate in candidates
            if candidate["relevance_category"] == "unlikely"
        ]
        cursor.executemany(
            """
            UPDATE discovery_candidates
            SET status = 'screened_out', reviewed_at = now(),
                review_reason = %s
            WHERE id = %s AND status = 'pending'
            """,
            [
                (
                    f"Automatically screened by {DISCOVERY_TRIAGE_VERSION}; reversible on rediscovery or classifier upgrade",
                    candidate_id,
                )
                for (candidate_id,) in screened_ids
            ],
        )
        if promoted_work_ids:
            cursor.execute(
                """
                INSERT INTO jobs (job_type, idempotency_key, payload)
                VALUES ('resolve_work_duplicates', %s, %s)
                ON CONFLICT (idempotency_key) DO NOTHING
                """,
                (
                    f"resolve-work-duplicates:triage:{pipeline_run_id}",
                    json.dumps({"pipeline_run_id": str(pipeline_run_id)}),
                ),
            )
        cursor.execute(
            """
            INSERT INTO audit_log (actor, action, entity_type, after_value, reason)
            VALUES (%s, 'automated_triage', 'discovery_candidate', %s, %s)
            """,
            (
                f"ml:{DISCOVERY_TRIAGE_VERSION}",
                json.dumps(
                    {
                        "pipeline_run_id": str(pipeline_run_id),
                        "promoted": len(promoted_work_ids),
                        "human_review": 0,
                        "screened_out": len(screened_ids),
                    }
                ),
                "Local metadata classifier applied reversible automated dispositions",
            ),
        )
        cursor.execute(
            "UPDATE pipeline_runs SET finished_at = now() WHERE id = %s",
            (pipeline_run_id,),
        )
        return {
            "candidates": len(candidates),
            "promoted": len(promoted_work_ids),
            "human_review": 0,
            "screened_out": len(screened_ids),
            **counts,
        }


def promote_possible_candidates(connection: psycopg.Connection[Any]) -> dict[str, int]:
    """Migrate already-triaged ambiguous metadata into low-priority candidate works."""
    with connection.transaction(), connection.cursor(row_factory=dict_row) as cursor:
        cursor.execute(
            """
            INSERT INTO pipeline_runs
                (pipeline_name, pipeline_version, model_name, model_version, configuration)
            VALUES ('discovery_candidate_promotion', %s, 'policy', 'promote-possible-v1', %s)
            RETURNING id
            """,
            (
                DISCOVERY_TRIAGE_VERSION,
                json.dumps(
                    {
                        "category": "possible",
                        "review_status": "candidate",
                        "proof_claims_approved": False,
                    }
                ),
            ),
        )
        pipeline_run_id = cursor.fetchone()["id"]
        cursor.execute(
            """
            SELECT id, provider, external_id, title, publication_year,
                   work_type::text AS work_type, raw_metadata,
                   relevance_score, relevance_category
            FROM discovery_candidates
            WHERE status = 'pending' AND relevance_category = 'possible'
            ORDER BY relevance_score DESC, id
            """
        )
        candidates = cursor.fetchall()
        work_ids = [
            promote_discovery_candidate(cursor, candidate, pipeline_run_id)
            for candidate in candidates
        ]
        if work_ids:
            cursor.execute(
                """
                INSERT INTO jobs (job_type, idempotency_key, payload)
                VALUES ('resolve_work_duplicates', %s, %s)
                ON CONFLICT (idempotency_key) DO NOTHING
                """,
                (
                    f"resolve-work-duplicates:promotion:{pipeline_run_id}",
                    json.dumps({"pipeline_run_id": str(pipeline_run_id)}),
                ),
            )
        cursor.execute(
            "UPDATE pipeline_runs SET finished_at = now() WHERE id = %s",
            (pipeline_run_id,),
        )
        return {"candidates": len(candidates), "promoted": len(work_ids)}


def resolve_work_duplicates(connection: psycopg.Connection[Any]) -> dict[str, int]:
    with connection.transaction(), connection.cursor(row_factory=dict_row) as cursor:
        cursor.execute(
            "SELECT id, title, publication_year FROM works "
            "WHERE merged_into_work_id IS NULL ORDER BY id"
        )
        works = cursor.fetchall()
        if len(works) < 2:
            return {"works": len(works), "suggestions": 0}
        matrix = TfidfVectorizer(analyzer="char_wb", ngram_range=(3, 5)).fit_transform(
            [row["title"] for row in works]
        )
        scores = cosine_similarity(matrix)
        stored = 0
        for left_index, left in enumerate(works):
            for right_index in range(left_index + 1, len(works)):
                right = works[right_index]
                score = float(scores[left_index, right_index])
                year_match = (
                    left["publication_year"] is not None
                    and left["publication_year"] == right["publication_year"]
                )
                adjusted = min(1.0, score + (0.05 if year_match else 0.0))
                if adjusted < 0.75:
                    continue
                left_id, right_id = sorted((left["id"], right["id"]))
                cursor.execute(
                    """
                    INSERT INTO duplicate_suggestions
                        (left_work_id, right_work_id, score, method, rationale)
                    VALUES (%s, %s, %s, %s, %s)
                    ON CONFLICT (left_work_id, right_work_id, method)
                    DO UPDATE SET score = EXCLUDED.score, rationale = EXCLUDED.rationale
                    """,
                    (
                        left_id,
                        right_id,
                        adjusted,
                        "sklearn-char-tfidf-3-5-v1",
                        json.dumps({"title_score": score, "publication_year_match": year_match}),
                    ),
                )
                stored += 1
        return {"works": len(works), "suggestions": stored}


def evaluate_classifier(connection: psycopg.Connection[Any]) -> dict[str, Any]:
    with connection.transaction(), connection.cursor(row_factory=dict_row) as cursor:
        cursor.execute(
            """
            SELECT ml.is_relevant, ml.expected_result_kind, pr.result_kind
            FROM ml_labels ml JOIN proof_results pr ON pr.id = ml.proof_result_id
            WHERE pr.classifier_version = %s
            """,
            (PIPELINE_VERSION,),
        )
        labels = cursor.fetchall()
        true_positives = sum(
            row["is_relevant"] and row["expected_result_kind"] == row["result_kind"]
            for row in labels
        )
        false_positives = sum(
            not row["is_relevant"] or row["expected_result_kind"] != row["result_kind"]
            for row in labels
        )
        false_negatives = sum(
            row["is_relevant"] and row["expected_result_kind"] != row["result_kind"]
            for row in labels
        )
        precision = (
            true_positives / (true_positives + false_positives)
            if true_positives + false_positives
            else None
        )
        recall = (
            true_positives / (true_positives + false_negatives)
            if true_positives + false_negatives
            else None
        )
        f1 = (
            2 * precision * recall / (precision + recall)
            if precision is not None and recall is not None and precision + recall
            else None
        )
        cursor.execute(
            """
            INSERT INTO ml_evaluations
                (pipeline_version, labelled_examples, true_positives, false_positives,
                 false_negatives, precision, recall, f1, details)
            VALUES (%s, %s, %s, %s, %s, %s, %s, %s, %s) RETURNING id
            """,
            (
                PIPELINE_VERSION,
                len(labels),
                true_positives,
                false_positives,
                false_negatives,
                precision,
                recall,
                f1,
                json.dumps({"metric_scope": "machine candidates with curator labels"}),
            ),
        )
        return {
            "evaluation_id": str(cursor.fetchone()["id"]),
            "labelled_examples": len(labels),
            "precision": precision,
            "recall": recall,
            "f1": f1,
        }


def persist_generic_job_success(
    connection: psycopg.Connection[Any], job_id: UUID, result: dict[str, Any]
) -> None:
    with connection.transaction():
        connection.execute(
            """
            UPDATE jobs SET status = 'succeeded', result = %s,
                leased_until = NULL, updated_at = now() WHERE id = %s
            """,
            (json.dumps(result), job_id),
        )


def run() -> None:
    database_url = os.environ["DATABASE_URL"]
    data_root = Path(os.getenv("DATA_ROOT", "/data"))
    worker_id = os.getenv("WORKER_ID", f"{socket.gethostname()}-{os.getpid()}")
    poll_seconds = float(os.getenv("WORKER_POLL_SECONDS", "2"))
    LOG.info("worker started: worker_id=%s poll_seconds=%s", worker_id, poll_seconds)
    last_idle_log = 0.0

    while True:
        try:
            with psycopg.connect(database_url, autocommit=False) as connection:
                while True:
                    job = claim_job(connection, worker_id)
                    if job is None:
                        now = time.monotonic()
                        if now - last_idle_log >= 60:
                            LOG.info("worker idle: no queued jobs")
                            last_idle_log = now
                        time.sleep(poll_seconds)
                        continue
                    try:
                        LOG.info(
                            "job started: id=%s type=%s attempt=%s",
                            job["id"],
                            job["job_type"],
                            job["attempts"],
                        )
                        if job["job_type"] == "extract_document":
                            processed = process_job(job, data_root)
                            document_id = UUID(str(job["payload"]["document_id"]))
                            persist_success(connection, job["id"], document_id, processed)
                        elif job["job_type"] == "export_approved_proofs_parquet":
                            export_id = UUID(str(job["payload"]["export_id"]))
                            object_key, sha256, row_count = export_approved_proofs(
                                connection, data_root, export_id
                            )
                            persist_export_success(
                                connection,
                                job["id"],
                                export_id,
                                object_key,
                                sha256,
                                row_count,
                            )
                        elif job["job_type"] == "resolve_discovery_entities":
                            persist_generic_job_success(
                                connection, job["id"], resolve_discovery_entities(connection)
                            )
                        elif job["job_type"] == "triage_discovery_candidates":
                            persist_generic_job_success(
                                connection, job["id"], triage_discovery_candidates(connection)
                            )
                        elif job["job_type"] == "promote_possible_candidates":
                            persist_generic_job_success(
                                connection, job["id"], promote_possible_candidates(connection)
                            )
                        elif job["job_type"] == "resolve_work_duplicates":
                            persist_generic_job_success(
                                connection, job["id"], resolve_work_duplicates(connection)
                            )
                        elif job["job_type"] == "evaluate_local_classifier":
                            persist_generic_job_success(
                                connection, job["id"], evaluate_classifier(connection)
                            )
                        else:
                            raise ValueError(f"unsupported job type: {job['job_type']}")
                        LOG.info("job succeeded: id=%s type=%s", job["id"], job["job_type"])
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
