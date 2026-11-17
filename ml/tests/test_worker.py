from pathlib import Path

import pytest

from type_safety_ml.worker import safe_object_path


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

