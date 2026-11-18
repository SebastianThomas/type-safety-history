use std::{sync::Arc, time::Duration};

use anyhow::Context;
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};
use thiserror::Error;
use url::Url;

#[derive(Clone)]
pub(crate) struct OpenAlexClient {
    client: Client,
    api_key: String,
    limiter: Arc<tokio::sync::Mutex<tokio::time::Instant>>,
}

#[derive(Debug, Error)]
pub(crate) enum OpenAlexError {
    #[error("OpenAlex rate limit or daily budget reached")]
    RateLimited,
    #[error("OpenAlex returned HTTP {0}")]
    Http(StatusCode),
    #[error("invalid OpenAlex response")]
    InvalidResponse(#[source] anyhow::Error),
    #[error("OpenAlex request failed")]
    Request(#[from] reqwest::Error),
}

#[derive(Debug, Deserialize)]
struct SearchEnvelope {
    #[serde(default)]
    results: Vec<OpenAlexWork>,
}

#[derive(Debug, Deserialize)]
struct OpenAlexWork {
    id: String,
    doi: Option<String>,
    title: Option<String>,
    publication_year: Option<i32>,
    #[serde(rename = "type")]
    work_type: Option<String>,
    #[serde(default)]
    authorships: Vec<Authorship>,
    primary_location: Option<Location>,
}

#[derive(Debug, Deserialize)]
struct Authorship {
    author_position: Option<String>,
    author: Author,
}

#[derive(Debug, Deserialize)]
struct Author {
    id: Option<String>,
    display_name: Option<String>,
    orcid: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Location {
    landing_page_url: Option<String>,
    license: Option<String>,
}

#[derive(Debug)]
pub(crate) struct DiscoveredWork {
    pub external_id: String,
    pub title: String,
    pub publication_year: Option<i32>,
    pub work_type: &'static str,
    pub selected_metadata: Value,
}

impl OpenAlexClient {
    pub(crate) fn new(api_key: String, user_agent: String) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !api_key.trim().is_empty(),
            "OPENALEX_API_KEY must not be empty"
        );
        let client = Client::builder()
            .user_agent(user_agent)
            .timeout(Duration::from_secs(20))
            .build()
            .context("build OpenAlex HTTP client")?;
        Ok(Self {
            client,
            api_key,
            limiter: Arc::new(tokio::sync::Mutex::new(tokio::time::Instant::now())),
        })
    }

    pub(crate) async fn search(
        &self,
        query: &str,
        from_year: i32,
        until_year: i32,
        rows: u16,
    ) -> Result<Vec<DiscoveredWork>, OpenAlexError> {
        let mut last_request = self.limiter.lock().await;
        tokio::time::sleep_until(*last_request + Duration::from_secs(1)).await;
        let mut url = Url::parse("https://api.openalex.org/works")
            .expect("static OpenAlex works URL is valid");
        url.query_pairs_mut()
            .append_pair("api_key", &self.api_key)
            .append_pair("search", query)
            .append_pair(
                "filter",
                &format!(
                    "from_publication_date:{from_year}-01-01,to_publication_date:{until_year}-12-31"
                ),
            )
            .append_pair("per_page", &rows.to_string())
            .append_pair(
                "select",
                "id,doi,title,publication_year,type,authorships,primary_location",
            );
        *last_request = tokio::time::Instant::now();
        let response = self.client.get(url).send().await?;
        match response.status() {
            StatusCode::TOO_MANY_REQUESTS | StatusCode::PAYMENT_REQUIRED => {
                return Err(OpenAlexError::RateLimited);
            }
            status if !status.is_success() => return Err(OpenAlexError::Http(status)),
            _ => {}
        }
        let envelope: SearchEnvelope = response
            .json()
            .await
            .map_err(|error| OpenAlexError::InvalidResponse(error.into()))?;
        Ok(envelope
            .results
            .into_iter()
            .filter_map(DiscoveredWork::from_response)
            .collect())
    }
}

impl DiscoveredWork {
    fn from_response(work: OpenAlexWork) -> Option<Self> {
        let title = work.title?.trim().to_owned();
        if title.is_empty() {
            return None;
        }
        let external_id = work
            .doi
            .as_deref()
            .map(normalize_doi)
            .unwrap_or_else(|| work.id.clone());
        let work_type = match work.work_type.as_deref() {
            Some("article") => "article",
            Some("book-chapter") => "book_chapter",
            Some("dissertation") => "thesis",
            Some("preprint") => "preprint",
            Some("report") => "technical_report",
            _ => "other",
        };
        let authors = work
            .authorships
            .iter()
            .map(|authorship| {
                json!({
                    "position": authorship.author_position,
                    "openalex_id": authorship.author.id,
                    "display_name": authorship.author.display_name,
                    "orcid": authorship.author.orcid
                })
            })
            .collect::<Vec<_>>();
        let selected_metadata = json!({
            "provider": "OpenAlex",
            "license": "CC0-1.0",
            "openalex_id": work.id,
            "doi": work.doi.as_deref().map(normalize_doi),
            "authors": authors,
            "primary_location": work.primary_location.map(|location| json!({
                "landing_page_url": location.landing_page_url,
                "license": location.license
            }))
        });
        Some(Self {
            external_id,
            title,
            publication_year: work.publication_year,
            work_type,
            selected_metadata,
        })
    }
}

fn normalize_doi(doi: &str) -> String {
    doi.trim()
        .trim_start_matches("https://doi.org/")
        .trim_start_matches("http://doi.org/")
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_selected_metadata_without_abstract_or_fulltext() {
        let response: SearchEnvelope = serde_json::from_value(json!({
            "results": [{
                "id": "https://openalex.org/W1",
                "doi": "https://doi.org/10.1006/INCO.1994.1093",
                "title": "A Syntactic Approach to Type Soundness",
                "publication_year": 1994,
                "type": "article",
                "authorships": [],
                "abstract_inverted_index": {"must": [0], "not": [1], "persist": [2]},
                "best_oa_location": {"pdf_url": "https://example.invalid/paper.pdf"}
            }]
        }))
        .unwrap();
        let mapped =
            DiscoveredWork::from_response(response.results.into_iter().next().unwrap()).unwrap();
        assert_eq!(mapped.external_id, "10.1006/inco.1994.1093");
        assert!(
            mapped
                .selected_metadata
                .get("abstract_inverted_index")
                .is_none()
        );
        assert!(mapped.selected_metadata.get("best_oa_location").is_none());
    }
}
