use axum::{
    Router,
    http::{HeaderMap, HeaderValue, header},
    response::{Html, IntoResponse, Response},
    routing::get,
};

use crate::AppState;

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route("/curate", get(curator_portal))
        .route("/docs", get(api_docs))
        .route("/api/openapi.json", get(openapi))
}

fn html_response(document: &'static str) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; \
             img-src 'self'; connect-src 'self'; base-uri 'none'; frame-ancestors 'none'; \
             form-action 'self'",
        ),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    (headers, Html(document)).into_response()
}

async fn curator_portal() -> Response {
    html_response(include_str!("../assets/curate.html"))
}

async fn api_docs() -> Response {
    html_response(include_str!("../assets/docs.html"))
}

async fn openapi() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/vnd.oai.openapi+json")],
        include_str!("../assets/openapi.json"),
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn bundled_openapi_is_valid_json() {
        let document: serde_json::Value =
            serde_json::from_str(include_str!("../assets/openapi.json")).unwrap();
        assert_eq!(document["openapi"], "3.1.0");
        assert!(document["paths"].as_object().unwrap().len() > 20);
    }
}
