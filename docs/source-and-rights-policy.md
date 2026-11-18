# Source reliability and rights policy

Reliability and permission are independent. A reliable publisher may prohibit
copying, while an openly licensed page may not be reliable evidence.

## Reliability order

1. Publisher or official conference proceedings.
2. DOI registry metadata.
3. Institutional repository or author university page.
4. Established preprint repository.
5. Curated bibliography used for corroboration.
6. Search engines and informal pages for discovery only.

Historical and priority claims require a primary publication or independently
corroborated authoritative metadata. Model output, blogs and general reference
sites are never sufficient evidence by themselves.

## Acquisition rules

- Use documented APIs or bulk datasets when the site requires them.
- Respect terms, robots directives, authentication and rate limits.
- Never bypass a paywall, access control or technical protection.
- Do not retain full text when only metadata or linking is permitted.
- Unknown rights default to `unknown_requires_review` and no download.
- `link_only` and `prohibited` content cannot be registered as a local document.
- ML processing requires an explicit `ml_use_permitted` decision.
- Public exports must omit restricted documents and substantial source text.
- A takedown records the audit event and removes the governed local object and
  derived content while preserving lawful bibliographic metadata.

Every adapter must document its allowed endpoints, fields, rate limits,
retention, redistribution rules and terms URL before it is enabled.

## Enabled adapter: Crossref REST API v1

- Endpoint: one-record lookup at `https://api.crossref.org/v1/works/{doi}`.
- Identification: required `CROSSREF_MAILTO` query parameter plus an identifying
  user-agent. Anonymous operation is intentionally unsupported.
- Concurrency: requests are serialized in-process and locally limited to two
  requests per second. This stays below the documented limits for both
  single-record and more expensive multi-record queries. Each import call can
  import only one DOI; each discovery call retrieves at most 100 candidates.
- Caching: a normalized DOI is returned from the local database unless the
  curator explicitly requests a refresh.
- Backoff: HTTP 429 is surfaced as a retryable service-unavailable response; the
  application does not immediately retry it.
- Retained fields: DOI, title, work type, publication year, authors, publisher,
  container title, canonical URL and deposited licence metadata.
- Excluded fields: abstracts, full text and TDM links. Crossref notes that
  deposited abstracts may remain copyrighted, and a TDM link does not establish
  access permission at the publisher.
- Reliability: authoritative bibliographic metadata, not primary evidence for
  the contents or priority of a theorem.
- Terms/documentation URL:
  `https://www.crossref.org/documentation/retrieve-metadata/rest-api/`.

Crossref discovery uses `/v1/works` with `query.bibliographic`, an inclusive
publication-year filter, and `select` so abstracts and unneeded fields never
arrive. Results enter `pending` discovery state. Search metadata alone cannot
become a canonical work or proof: a curator must separately import its DOI,
after which the candidate is linked to the resulting work, or reject it with an
audited reason.

## Local ML

No document content or embedding is sent to a hosted inference or OCR service.
The baseline has no downloaded model weights: it uses deterministic
scikit-learn hashing/TF-IDF algorithms (BSD-3-Clause) and the locally installed
Tesseract OCR engine (Apache-2.0). PDF text and rendering use pypdf
(BSD-3-Clause) and pypdfium2 (Apache-2.0 OR BSD-3-Clause, including the licence
notices shipped with its PDFium wheel). Library versions, algorithms and
licences are recorded with pipeline output. Runtime ML does not need outbound network access.
Documents whose terms do not permit local ML are excluded from jobs.

The initial `local-term-baseline-v1` pipeline is deliberately deterministic and
conservative. It detects terminology associated with progress, preservation,
subject reduction and type soundness, records only page/character coordinates
and a paraphrase in PostgreSQL, and leaves every result in `machine_candidate`.
It is a discovery aid, not evidence that a theorem actually appears or is valid.

Embedded PDF text is preferred. Tesseract is used only as a local fallback for
pages with little embedded text, and OCR usage is recorded per document. DOI
mentions become hashed-context citation candidates; no citation is promoted
without curator review. Similarity, duplicate and entity-resolution scores are
also suggestions, never canonical assertions. Curator labels can be used to
calculate local precision, recall and F1 for a recorded pipeline version.

## Curator-supplied documents

A local PDF is accepted only when the curator records a reliable source URL and
a specific rights basis. `open_redistributable` requires a licence name and URL.
`internal_research_copy` can never be marked redistributable. Metadata-only,
link-only, unknown and prohibited sources cannot be uploaded. File bytes are
not fetched by the service from the recorded URL; acquisition remains a human
action performed under the source's applicable terms.

