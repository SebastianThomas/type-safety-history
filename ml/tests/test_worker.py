from pathlib import Path
from uuid import UUID

import pytest
import zarr

from type_safety_ml.worker import (
    EMBEDDING_DIMENSIONS,
    classify_discovery_candidates,
    classify_pages,
    extract_citation_candidates,
    safe_object_path,
    write_embeddings,
)


def test_safe_object_path_accepts_local_file(tmp_path: Path) -> None:
    document = tmp_path / "objects" / "paper.pdf"
    document.parent.mkdir()
    document.write_bytes(b"pdf")
    assert safe_object_path(tmp_path, "objects/paper.pdf") == document


def test_safe_object_path_rejects_escape(tmp_path: Path) -> None:
    outside = tmp_path.parent / "secret.pdf"
    outside.write_bytes(b"secret")
    try:
        with pytest.raises(ValueError, match="escapes"):
            safe_object_path(tmp_path, "../secret.pdf")
    finally:
        outside.unlink()


def test_classification_is_conservative_and_idempotent() -> None:
    document_id = UUID("00000000-0000-0000-0000-000000000001")
    pages = [
        {"page": 1, "text": "Background discussion about programming languages."},
        {"page": 2, "text": "The progress and preservation theorems establish type soundness."},
    ]
    first = classify_pages(document_id, pages)
    second = classify_pages(document_id, pages)
    assert {candidate.result_kind for candidate in first} == {
        "progress_and_preservation",
        "type_soundness",
    }
    assert [candidate.candidate_key for candidate in first] == [
        candidate.candidate_key for candidate in second
    ]


def test_classification_rejects_ecological_progress_and_preservation() -> None:
    candidates = classify_pages(
        UUID("00000000-0000-0000-0000-000000000003"),
        [
            {
                "page": 1,
                "text": (
                    "Coastal wetlands of the north of Peru: preservation progress "
                    "and biodiversity conservation in a changing ecosystem."
                ),
            }
        ],
    )
    assert candidates == []


def test_embeddings_are_local_deterministic_zarr(tmp_path: Path) -> None:
    document_id = UUID("00000000-0000-0000-0000-000000000002")
    pages = [
        {"page": 1, "text": "Progress holds for every well-typed closed term."},
        {"page": 2, "text": "Preservation holds after each reduction step."},
    ]
    key, first_digest, vector = write_embeddings(tmp_path, document_id, pages)
    _, second_digest, repeated = write_embeddings(tmp_path, document_id, pages)
    group = zarr.open_group(tmp_path / key, mode="r")
    assert vector.shape == (EMBEDDING_DIMENSIONS,)
    assert group["page_embeddings"].shape == (2, EMBEDDING_DIMENSIONS)
    assert group.attrs["license"] == "BSD-3-Clause"
    assert first_digest == second_digest
    assert vector.tolist() == repeated.tolist()


def test_citation_candidates_store_hash_not_context() -> None:
    candidates = extract_citation_candidates(
        [{"page": 3, "text": "We build on the proof at doi:10.1145/1234.5678."}]
    )
    assert len(candidates) == 1
    assert candidates[0].doi == "10.1145/1234.5678"
    assert len(candidates[0].context_sha256) == 64


def test_discovery_triage_prioritizes_type_soundness() -> None:
    score, category = classify_discovery_candidates(
        [
            {
                "title": "A Syntactic Approach to Type Soundness",
                "query_texts": ["type soundness programming language"],
            }
        ]
    )[0]
    assert score >= 0.7
    assert category == "likely_relevant"


def test_discovery_triage_demotes_unrelated_safety_results() -> None:
    score, category = classify_discovery_candidates(
        [
            {
                "title": "Food safety management in regional restaurants",
                "query_texts": ["type safety programming language"],
            }
        ]
    )[0]
    assert score < 0.25
    assert category == "unlikely"


def test_discovery_triage_rejects_coastal_wetland_preservation_progress() -> None:
    score, category = classify_discovery_candidates(
        [
            {
                "title": "Coastal wetlands of the north of Peru: preservation progress",
                "query_texts": ["progress preservation type system"],
            }
        ]
    )[0]
    assert score < 0.25
    assert category == "unlikely"


def test_discovery_triage_keeps_historical_precursors_for_review() -> None:
    score, category = classify_discovery_candidates(
        [
            {
                "title": "A Theory of Type Polymorphism in Programming",
                "query_texts": ["curated-seed:historical_precursor"],
            }
        ]
    )[0]
    assert score >= 0.25
    assert category == "possible"
