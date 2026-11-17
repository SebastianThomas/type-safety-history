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

## Local ML

No document content or embedding is sent to a hosted inference or OCR service.
Model weights are acquired in a controlled preparation step and recorded with
source, version, checksum and licence. Runtime pods do not need outbound network
access. Documents whose terms do not permit local ML are excluded from jobs.

