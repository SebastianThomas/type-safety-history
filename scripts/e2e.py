"""Black-box API/worker lifecycle test. Requires a running API and worker."""

from __future__ import annotations

import json
import os
import time
import urllib.error
import urllib.request
from uuid import uuid4

BASE_URL = os.getenv("E2E_BASE_URL", "http://127.0.0.1:8081")
TOKEN = os.getenv("E2E_CURATOR_TOKEN", "ci-only-token")


def request(method: str, path: str, payload: object | None = None) -> object | None:
    body = None if payload is None else json.dumps(payload).encode()
    headers = {"Authorization": f"Bearer {TOKEN}"}
    if body is not None:
        headers["Content-Type"] = "application/json"
    call = urllib.request.Request(
        BASE_URL + path, data=body, headers=headers, method=method
    )
    try:
        with urllib.request.urlopen(call, timeout=10) as response:
            content = response.read()
            return json.loads(content) if content else None
    except urllib.error.HTTPError as error:
        raise AssertionError(
            f"{method} {path}: {error.code} {error.read().decode()}"
        ) from error


def upload(path: str, metadata: dict[str, object], pdf: bytes) -> dict[str, str]:
    boundary = f"e2e-{uuid4().hex}"
    body = bytearray()
    for name, content_type, value, filename in (
        ("metadata", "application/json", json.dumps(metadata).encode(), None),
        ("file", "application/pdf", pdf, "paper.pdf"),
    ):
        body.extend(f"--{boundary}\r\n".encode())
        disposition = f'Content-Disposition: form-data; name="{name}"'
        if filename:
            disposition += f'; filename="{filename}"'
        body.extend(f"{disposition}\r\nContent-Type: {content_type}\r\n\r\n".encode())
        body.extend(value)
        body.extend(b"\r\n")
    body.extend(f"--{boundary}--\r\n".encode())
    call = urllib.request.Request(
        BASE_URL + path,
        data=bytes(body),
        headers={
            "Authorization": f"Bearer {TOKEN}",
            "Content-Type": f"multipart/form-data; boundary={boundary}",
        },
        method="POST",
    )
    with urllib.request.urlopen(call, timeout=15) as response:
        return json.loads(response.read())


def simple_pdf(text: str) -> bytes:
    escaped = text.replace("\\", "\\\\").replace("(", "\\(").replace(")", "\\)")
    stream = f"BT /F1 12 Tf 72 720 Td ({escaped}) Tj ET".encode()
    objects = [
        b"<< /Type /Catalog /Pages 2 0 R >>",
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>",
        b"<< /Length "
        + str(len(stream)).encode()
        + b" >>\nstream\n"
        + stream
        + b"\nendstream",
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
    ]
    output = bytearray(b"%PDF-1.4\n")
    offsets = [0]
    for number, obj in enumerate(objects, start=1):
        offsets.append(len(output))
        output.extend(f"{number} 0 obj\n".encode() + obj + b"\nendobj\n")
    xref = len(output)
    output.extend(f"xref\n0 {len(objects) + 1}\n".encode())
    output.extend(b"0000000000 65535 f \n")
    for offset in offsets[1:]:
        output.extend(f"{offset:010d} 00000 n \n".encode())
    output.extend(
        f"trailer\n<< /Size {len(objects) + 1} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n".encode()
    )
    return bytes(output)


def wait_for(path: str, predicate, timeout: float = 30) -> object:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = request("GET", path)
        if predicate(value):
            return value
        time.sleep(0.25)
    raise AssertionError(f"timed out waiting for {path}")


def create_work(title: str, doi: str, source_url: str) -> dict[str, object]:
    return request(
        "POST",
        "/api/v1/works",
        {
            "title": title,
            "work_type": "article",
            "publication_year": 1994,
            "doi": doi,
            "source_url": source_url,
            "source_kind": "curator_upload",
            "reliability": "primary",
            "rights_status": "open_metadata_only",
            "rights_basis": "Generated CI metadata fixture.",
            "ml_use_permitted": False,
            "redistribution_permitted": False,
        },
    )


def main() -> None:
    initial_counts = request("GET", "/api/v1/stats/overview")["counts"]
    suffix = uuid4().hex
    canonical = create_work(
        "A Syntactic Approach to Type Soundness",
        f"10.9999/{suffix}-canonical",
        f"https://example.invalid/{suffix}/canonical",
    )
    duplicate = create_work(
        "A Syntactic Approach to Type-Soundness",
        f"10.9999/{suffix}-duplicate",
        f"https://example.invalid/{suffix}/duplicate",
    )
    document = upload(
        "/api/v1/documents/upload",
        {
            "work_id": canonical["id"],
            "source_url": f"https://example.invalid/{suffix}/paper",
            "source_kind": "curator_upload",
            "reliability": "primary",
            "rights_status": "open_redistributable",
            "license_name": "CC0-1.0",
            "license_url": "https://creativecommons.org/publicdomain/zero/1.0/",
            "rights_basis": "Generated CI PDF fixture released as CC0.",
            "ml_use_permitted": True,
            "redistribution_permitted": True,
        },
        simple_pdf(
            "The progress and preservation theorems establish type soundness. "
            f"See 10.9999/example-citation. Run {suffix}."
        ),
    )

    candidates = wait_for(
        "/api/v1/proof-results/candidates",
        lambda rows: isinstance(rows, list) and len(rows) >= 2,
    )
    assert {row["result_kind"] for row in candidates} >= {
        "progress_and_preservation",
        "type_soundness",
    }
    suggestions = wait_for(
        "/api/v1/duplicate-suggestions",
        lambda rows: isinstance(rows, list) and len(rows) >= 1,
    )
    suggestion = next(
        row
        for row in suggestions
        if {row["left_work_id"], row["right_work_id"]}
        == {canonical["id"], duplicate["id"]}
    )
    request(
        "POST",
        f"/api/v1/duplicate-suggestions/{suggestion['id']}/resolve",
        {
            "decision": "accept",
            "canonical_work_id": canonical["id"],
            "actor": "ci-e2e",
            "reason": "Generated duplicate pair",
        },
    )
    request(
        "POST",
        f"/api/v1/documents/{document['document_id']}/takedown",
        {"actor": "ci-e2e", "reason": "Generated fixture cleanup"},
    )
    overview = request("GET", "/api/v1/stats/overview")
    assert (
        overview["counts"]["documents_taken_down"]
        == initial_counts["documents_taken_down"] + 1
    )
    assert (
        overview["counts"]["proofs_rejected"] >= initial_counts["proofs_rejected"] + 2
    )
    assert overview["counts"]["discovered"] >= 2
    evolution = request("GET", "/api/v1/stats/evolution?days=7")
    assert len(evolution["series"]) == 7
    with urllib.request.urlopen(BASE_URL + "/dashboard", timeout=10) as response:
        dashboard = response.read().decode()
    assert "Research funnel" in dashboard and "Corpus growth" in dashboard
    print("end-to-end lifecycle passed")


if __name__ == "__main__":
    main()
