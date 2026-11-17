from pathlib import Path
from uuid import UUID

import pytest

from type_safety_ml.worker import classify_pages, safe_object_path


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

