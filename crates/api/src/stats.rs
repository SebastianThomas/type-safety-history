use axum::{
    Json, Router,
    extract::{Query, State},
    response::Html,
    routing::get,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::FromRow;

use crate::{AppState, routes::ApiError};

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/stats/overview", get(overview))
        .route("/api/v1/stats/evolution", get(evolution))
        .route("/dashboard", get(dashboard))
}

#[derive(Debug, Serialize, FromRow)]
struct Overview {
    works: i64,
    discovered: i64,
    discovery_pending: i64,
    discovery_imported: i64,
    discovery_rejected: i64,
    proof_candidates: i64,
    proofs_approved: i64,
    proofs_rejected: i64,
    proofs_disputed: i64,
    active_documents: i64,
    documents_taken_down: i64,
    verified_artifacts: i64,
    approved_citations: i64,
    duplicate_suggestions: i64,
    entity_suggestions: i64,
    queued_jobs: i64,
    dead_letter_jobs: i64,
    coverage_runs: i64,
}

async fn load_overview(state: &AppState) -> Result<Overview, ApiError> {
    Ok(sqlx::query_as::<_, Overview>(
        r#"SELECT
           (SELECT count(*) FROM works WHERE merged_into_work_id IS NULL) AS works,
           (SELECT count(*) FROM discovery_candidates) AS discovered,
           (SELECT count(*) FROM discovery_candidates WHERE status = 'pending') AS discovery_pending,
           (SELECT count(*) FROM discovery_candidates WHERE status = 'imported') AS discovery_imported,
           (SELECT count(*) FROM discovery_candidates WHERE status = 'rejected') AS discovery_rejected,
           (SELECT count(*) FROM proof_results WHERE status = 'machine_candidate') AS proof_candidates,
           (SELECT count(*) FROM proof_results WHERE status = 'curator_approved') AS proofs_approved,
           (SELECT count(*) FROM proof_results WHERE status = 'rejected') AS proofs_rejected,
           (SELECT count(*) FROM proof_results WHERE status = 'disputed') AS proofs_disputed,
           (SELECT count(*) FROM documents WHERE status = 'active') AS active_documents,
           (SELECT count(*) FROM documents WHERE status = 'taken_down') AS documents_taken_down,
           (SELECT count(*) FROM formal_artifacts WHERE status = 'verified') AS verified_artifacts,
           (SELECT count(*) FROM citations WHERE status = 'curator_approved') AS approved_citations,
           (SELECT count(*) FROM duplicate_suggestions WHERE status = 'pending') AS duplicate_suggestions,
           (SELECT count(*) FROM entity_resolution_suggestions WHERE status = 'pending') AS entity_suggestions,
           (SELECT count(*) FROM jobs WHERE status IN ('queued', 'running', 'failed')) AS queued_jobs,
           (SELECT count(*) FROM jobs WHERE status = 'dead_letter') AS dead_letter_jobs,
           (SELECT count(*) FROM coverage_runs) AS coverage_runs"#,
    )
    .fetch_one(&state.db)
    .await?)
}

async fn overview(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let counts = load_overview(&state).await?;
    Ok(Json(json!({
        "counts": counts,
        "definitions": {
            "discovered": "Unique bibliographic candidates returned by permitted discovery adapters or curated seeds.",
            "proof_candidates": "Unreviewed machine-generated proof-result candidates.",
            "proofs_approved": "Proof results approved by a curator with evidence.",
            "works": "Canonical, non-merged scholarly works."
        }
    })))
}

#[derive(Deserialize)]
struct EvolutionQuery {
    #[serde(default = "default_days")]
    days: i32,
}

fn default_days() -> i32 {
    365
}

#[derive(Debug, Serialize, FromRow)]
struct EvolutionPoint {
    day: String,
    discovered: i64,
    works: i64,
    proof_results: i64,
    approval_events: i64,
    coverage_runs: i64,
    discovery_pending: i64,
    proof_candidates: i64,
    proofs_approved: i64,
    queued_jobs: i64,
}

async fn load_evolution(state: &AppState, days: i32) -> Result<Vec<EvolutionPoint>, ApiError> {
    Ok(sqlx::query_as::<_, EvolutionPoint>(
        r#"WITH days AS (
              SELECT generate_series(current_date - ($1::integer - 1), current_date, '1 day')::date AS day
           )
           SELECT days.day::text AS day,
              (SELECT count(*) FROM discovery_candidates dc WHERE dc.discovered_at < days.day + 1) AS discovered,
              (SELECT count(*) FROM works w WHERE w.created_at < days.day + 1
                  AND (w.merged_at IS NULL OR w.merged_at >= days.day + 1)) AS works,
              (SELECT count(*) FROM proof_results pr WHERE pr.created_at < days.day + 1) AS proof_results,
              (SELECT count(*) FROM audit_log a WHERE a.entity_type = 'proof_result'
                  AND a.after_value->>'status' = 'curator_approved'
                  AND a.occurred_at < days.day + 1) AS approval_events,
              (SELECT count(*) FROM coverage_runs cr WHERE cr.finished_at < days.day + 1) AS coverage_runs,
              COALESCE(snapshot.discovery_pending, 0) AS discovery_pending,
              COALESCE(snapshot.proof_candidates, 0) AS proof_candidates,
              COALESCE(snapshot.proofs_approved, 0) AS proofs_approved,
              COALESCE(snapshot.queued_jobs, 0) AS queued_jobs
           FROM days
           LEFT JOIN LATERAL (
              SELECT discovery_pending, proof_candidates, proofs_approved, queued_jobs
              FROM corpus_metric_snapshots snapshot
              WHERE snapshot.recorded_at < days.day + 1
              ORDER BY snapshot.recorded_at DESC LIMIT 1
           ) snapshot ON true
           ORDER BY days.day"#,
    )
    .bind(days)
    .fetch_all(&state.db)
    .await?)
}

async fn evolution(
    State(state): State<AppState>,
    Query(query): Query<EvolutionQuery>,
) -> Result<Json<Value>, ApiError> {
    let days = query.days.clamp(7, 3650);
    let points = load_evolution(&state, days).await?;
    Ok(Json(json!({
        "days": days,
        "series": points,
        "note": "Discovery/work/proof/approval series are cumulative. Pending and queue series are durable minute-bucketed state snapshots."
    })))
}

async fn dashboard(State(state): State<AppState>) -> Result<Html<String>, ApiError> {
    let counts = load_overview(&state).await?;
    let points = load_evolution(&state, 365).await?;
    let chart = svg_chart(&points);
    let backlog_chart = svg_backlog_chart(&points);
    let recent = points
        .iter()
        .rev()
        .take(14)
        .map(|point| {
            format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                point.day,
                point.discovery_pending,
                point.proof_candidates,
                point.proofs_approved,
                point.queued_jobs
            )
        })
        .collect::<String>();
    let html = format!(
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width">
<meta http-equiv="refresh" content="60"><title>Type Safety History — Corpus state</title>
<style>
:root{{--ink:#18212b;--muted:#687482;--paper:#f4f1e9;--card:#fffdf8;--line:#d8d2c4;--blue:#285f8f;--green:#39755a;--gold:#a66b19;--red:#a64536}}
*{{box-sizing:border-box}} body{{margin:0;background:var(--paper);color:var(--ink);font:15px/1.5 ui-sans-serif,system-ui,sans-serif}}
main{{max-width:1180px;margin:auto;padding:42px 24px 72px}} header{{display:flex;justify-content:space-between;gap:24px;align-items:end;margin-bottom:30px}}
h1{{font:700 36px/1.05 ui-serif,Georgia,serif;margin:0 0 8px}} h2{{font:700 22px ui-serif,Georgia,serif;margin:0 0 14px}}
p{{margin:0;color:var(--muted)}} .stamp{{text-align:right;font-size:13px}} .grid{{display:grid;grid-template-columns:repeat(4,1fr);gap:14px;margin:22px 0 34px}}
.card{{background:var(--card);border:1px solid var(--line);border-radius:12px;padding:18px;box-shadow:0 2px 10px #342b1c0b}}
.value{{font:700 32px/1 ui-serif,Georgia,serif;margin:8px 0}} .label{{color:var(--muted);font-size:13px;text-transform:uppercase;letter-spacing:.06em}}
.wide{{grid-column:span 2}} .panel{{background:var(--card);border:1px solid var(--line);border-radius:12px;padding:22px;margin-top:16px;overflow:auto}}
.funnel{{display:grid;grid-template-columns:repeat(4,1fr);gap:10px}} .stage{{border-left:4px solid var(--blue);padding:10px 12px;background:#285f8f0b}}
.stage:nth-child(2){{border-color:var(--gold)}} .stage:nth-child(3){{border-color:var(--green)}} .stage:nth-child(4){{border-color:var(--red)}}
.stage strong{{display:block;font-size:24px}} svg{{width:100%;min-width:620px;height:auto}} table{{width:100%;border-collapse:collapse;font-variant-numeric:tabular-nums}}
th,td{{text-align:right;padding:8px 10px;border-bottom:1px solid var(--line)}} th:first-child,td:first-child{{text-align:left}} th{{color:var(--muted);font-size:12px;text-transform:uppercase}}
.legend{{display:flex;gap:18px;flex-wrap:wrap;font-size:13px;color:var(--muted);margin:8px 0 16px}} .dot{{display:inline-block;width:9px;height:9px;border-radius:50%;margin-right:5px}}
@media(max-width:800px){{.grid{{grid-template-columns:repeat(2,1fr)}}header{{display:block}}.stamp{{text-align:left;margin-top:8px}}.funnel{{grid-template-columns:1fr 1fr}}}}
</style></head><body><main>
<header><div><h1>Type Safety History</h1><p>Evidence-backed corpus state and research throughput.</p></div>
<p class="stamp">Auto-refreshes every 60 seconds<br>365-day cumulative view</p></header>
<section class="grid">
<div class="card"><div class="label">Canonical works</div><div class="value">{works}</div><p>Non-merged records</p></div>
<div class="card"><div class="label">Found candidates</div><div class="value">{discovered}</div><p>Unique discovery records</p></div>
<div class="card"><div class="label">Approved proofs</div><div class="value">{approved}</div><p>Curator verified</p></div>
<div class="card"><div class="label">Awaiting proof review</div><div class="value">{proof_candidates}</div><p>Machine candidates</p></div>
<div class="card"><div class="label">Active documents</div><div class="value">{documents}</div><p>{taken_down} taken down</p></div>
<div class="card"><div class="label">Verified artifacts</div><div class="value">{artifacts}</div><p>Formal material</p></div>
<div class="card"><div class="label">Coverage runs</div><div class="value">{coverage}</div><p>Recorded searches</p></div>
<div class="card"><div class="label">Queue health</div><div class="value">{jobs}</div><p>{dead} dead-letter jobs</p></div>
</section>
<section class="panel"><h2>Research funnel</h2><div class="funnel">
<div class="stage"><span>Found</span><strong>{discovered}</strong></div>
<div class="stage"><span>Pending import</span><strong>{pending}</strong></div>
<div class="stage"><span>Imported</span><strong>{imported}</strong></div>
<div class="stage"><span>Rejected</span><strong>{rejected}</strong></div>
</div></section>
<section class="panel"><h2>Corpus growth</h2>
<div class="legend"><span><i class="dot" style="background:#285f8f"></i>Found</span><span><i class="dot" style="background:#39755a"></i>Works</span><span><i class="dot" style="background:#a66b19"></i>Proof results</span><span><i class="dot" style="background:#a64536"></i>Approval events</span></div>
{chart}</section>
<section class="panel"><h2>Review backlog over time</h2>
<div class="legend"><span><i class="dot" style="background:#285f8f"></i>Pending discovery</span><span><i class="dot" style="background:#a66b19"></i>Proof candidates</span><span><i class="dot" style="background:#a64536"></i>Active jobs</span></div>
{backlog_chart}</section>
<section class="panel"><h2>Last 14 days</h2><table><thead><tr><th>Date</th><th>Pending discovery</th><th>Proof queue</th><th>Approved proofs</th><th>Active jobs</th></tr></thead><tbody>{recent}</tbody></table></section>
<section class="panel"><h2>Review queues</h2><div class="funnel">
<div class="stage"><span>Duplicate suggestions</span><strong>{duplicates}</strong></div>
<div class="stage"><span>Entity suggestions</span><strong>{entities}</strong></div>
<div class="stage"><span>Citations approved</span><strong>{citations}</strong></div>
<div class="stage"><span>Proofs disputed</span><strong>{disputed}</strong></div>
</div></section>
</main></body></html>"#,
        works = counts.works,
        discovered = counts.discovered,
        approved = counts.proofs_approved,
        proof_candidates = counts.proof_candidates,
        documents = counts.active_documents,
        taken_down = counts.documents_taken_down,
        artifacts = counts.verified_artifacts,
        coverage = counts.coverage_runs,
        jobs = counts.queued_jobs,
        dead = counts.dead_letter_jobs,
        pending = counts.discovery_pending,
        imported = counts.discovery_imported,
        rejected = counts.discovery_rejected,
        duplicates = counts.duplicate_suggestions,
        entities = counts.entity_suggestions,
        citations = counts.approved_citations,
        disputed = counts.proofs_disputed,
    );
    Ok(Html(html))
}

fn svg_chart(points: &[EvolutionPoint]) -> String {
    svg_lines(
        points,
        &[
            (|point: &EvolutionPoint| point.discovered, "#285f8f"),
            (|point: &EvolutionPoint| point.works, "#39755a"),
            (|point: &EvolutionPoint| point.proof_results, "#a66b19"),
            (|point: &EvolutionPoint| point.approval_events, "#a64536"),
        ],
        "Cumulative corpus growth over 365 days",
    )
}

fn svg_backlog_chart(points: &[EvolutionPoint]) -> String {
    svg_lines(
        points,
        &[
            (|point: &EvolutionPoint| point.discovery_pending, "#285f8f"),
            (|point: &EvolutionPoint| point.proof_candidates, "#a66b19"),
            (|point: &EvolutionPoint| point.queued_jobs, "#a64536"),
        ],
        "Review backlog over 365 days",
    )
}

type Metric = fn(&EvolutionPoint) -> i64;

fn svg_lines(points: &[EvolutionPoint], series: &[(Metric, &str)], label: &str) -> String {
    let width = 1000_f64;
    let height = 300_f64;
    let inset = 28_f64;
    let maximum = points
        .iter()
        .map(|point| {
            series
                .iter()
                .map(|(value, _)| value(point))
                .max()
                .unwrap_or(0)
        })
        .max()
        .unwrap_or(1)
        .max(1) as f64;
    let line = |value: fn(&EvolutionPoint) -> i64, color: &str| {
        let coordinates = points
            .iter()
            .enumerate()
            .map(|(index, point)| {
                let x = inset
                    + index as f64 / (points.len().saturating_sub(1).max(1)) as f64
                        * (width - inset * 2.0);
                let y = height - inset - value(point) as f64 / maximum * (height - inset * 2.0);
                format!("{x:.1},{y:.1}")
            })
            .collect::<Vec<_>>()
            .join(" ");
        format!(
            r#"<polyline points="{coordinates}" fill="none" stroke="{color}" stroke-width="3" vector-effect="non-scaling-stroke"/>"#
        )
    };
    let paths = series
        .iter()
        .map(|(value, color)| line(*value, color))
        .collect::<String>();
    format!(
        r##"<svg viewBox="0 0 1000 300" role="img" aria-label="{label}">
<line x1="28" y1="272" x2="972" y2="272" stroke="#d8d2c4"/>
<line x1="28" y1="28" x2="28" y2="272" stroke="#d8d2c4"/>
{paths}
<text x="30" y="22" fill="#687482" font-size="12">max {}</text>
</svg>"##,
        maximum as i64
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_chart_is_valid_svg() {
        let chart = svg_chart(&[]);
        assert!(chart.starts_with("<svg"));
        assert!(chart.contains("max 1"));
    }
}
