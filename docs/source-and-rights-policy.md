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
- A takedown records the audit event, tombstones the database record, disables
  further ML/redistribution and removes the governed local object and derived
  content. Evidence hashes, coordinates and paraphrases remain for provenance;
  extracted source text does not.

Every adapter must document its allowed endpoints, fields, rate limits,
retention, redistribution rules and terms URL before it is enabled.

## Enabled adapter: Crossref REST API v1

- Endpoint: one-record lookup at `https://api.crossref.org/v1/works/{doi}`.
- Identification: required `CROSSREF_MAILTO` query parameter plus an identifying
  user-agent. Anonymous operation is intentionally unsupported.
- Concurrency: requests are serialized in-process and locally limited to two
  requests per second. This stays below the documented limits for both
  single-record and more expensive multi-record queries. Each import call can
  import only one DOI; each discovery call retrieves at most 1,000 candidates.
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
arrive. Results enter local ML triage. Likely and possible records may become
unreviewed candidate works automatically; search metadata alone can never
approve a proof or historical claim.

Saved searches use the same endpoint, field selection, contact identification
and in-process rate limiter. Their cadence cannot be shorter than one hour.
Scheduling does not grant permission to download linked documents. The separate
source resolver requires an explicit allowlisted location-level licence.

## Enabled adapter: Unpaywall REST API v2

- Endpoint: `https://api.unpaywall.org/v2/{doi}`.
- Identification: the monitored `CROSSREF_MAILTO` value is supplied as the
  required `email` query parameter; requests are serialized and limited to five
  per second.
- Purpose: locate an explicitly licensed PDF for candidate works with a DOI.
  The best location is preferred, with fallback across reported OA locations
  when the best record has no direct PDF. Unpaywall metadata does not itself
  become scholarly evidence.
- Automatic allowlist: `cc0`, `public-domain`, and `cc-by*` except any `-nd`
  variant. Missing, implied, publisher-specific, unknown and ND licences are
  never downloaded automatically.
- Network boundary: downloads must remain credential-free HTTPS across every
  redirect and every resolved address must be public. Responses must declare
  `application/pdf`, begin with PDF magic and remain within 50 MiB.
- Retention: exact location/licence metadata, rights basis, SHA-256 and the
  content-addressed object key are retained. Automatic acquisitions permit
  local ML but are not marked for redistribution.
- Failure behavior: missing copies and DOI-less works are recorded; ambiguous
  rights enter `rights_review`; these terminal-looking outcomes are retried
  monthly because availability and licence metadata can improve. Transient
  failures use bounded backoff and expiring leases.
- Documentation and terms:
  `https://data.unpaywall.org/products/api`,
  `https://data.unpaywall.org/data-format`, and
  `https://unpaywall.org/legal/terms-of-service`.

## Optional adapter: OpenAlex Works API

- Endpoint: `https://api.openalex.org/works` with `search`, publication-date
  filters, `per_page`, and an explicit `select` list.
- Authentication: `OPENALEX_API_KEY` is required by this application so usage
  and the provider's daily service budget remain attributable and bounded.
- Rate: requests are serialized in-process and limited to one per second; each
  application call retrieves at most 100 records.
- Retained fields: OpenAlex ID, DOI, title, publication year, work type,
  authorship identifiers/names and primary-location landing-page/licence
  metadata.
- Excluded fields: abstracts, inverted abstracts, PDF/full-text URLs and full
  text. OpenAlex discovery never authorizes acquisition of a linked document.
- Licence: OpenAlex publishes its catalog metadata under CC0. The adapter marks
  its selected metadata `CC0-1.0` while preserving any separate location-level
  licence only as a discovery fact requiring curator verification.
- Documentation and terms:
  `https://help.openalex.org/api/`,
  `https://help.openalex.org/data/how-its-built/`, and
  `https://openalex.org/OpenAlex_termsofservice.pdf`.

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
link-only, unknown and prohibited sources cannot be uploaded. File bytes may
also be acquired by the Unpaywall adapter under the strict explicit-licence
policy above. All other acquisition remains a human action performed under the
source's applicable terms.

