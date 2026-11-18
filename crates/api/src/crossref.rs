use std::{sync::Arc, time::Duration};

use anyhow::Context;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use url::Url;

#[derive(Clone)]
pub(crate) struct CrossrefClient {
    client: Client,
    mailto: String,
    limiter: Arc<tokio::sync::Mutex<tokio::time::Instant>>,
}

#[derive(Debug, Error)]
pub(crate) enum CrossrefError {
    #[error("DOI is not registered with Crossref")]
    NotFound,
    #[error("Crossref rate limit reached; retry later")]
    RateLimited,
    #[error("Crossref returned HTTP {0}")]
    Http(StatusCode),
    #[error("invalid Crossref response")]
    InvalidResponse(#[source] anyhow::Error),
    #[error("Crossref request failed")]
    Request(#[from] reqwest::Error),
}

#[derive(Debug, Deserialize)]
struct Envelope {
    message: CrossrefWork,
}

#[derive(Debug, Deserialize)]
struct SearchEnvelope {
    message: SearchMessage,
}

#[derive(Debug, Deserialize)]
struct SearchMessage {
    #[serde(default)]
    items: Vec<CrossrefWork>,
}

#[derive(Debug, Deserialize)]
struct CrossrefWork {
    #[serde(rename = "DOI")]
    doi: String,
    #[serde(default)]
    title: Vec<String>,
    #[serde(rename = "type")]
    work_type: Option<String>,
    #[serde(default)]
    author: Vec<CrossrefAuthor>,
    #[serde(rename = "published-print")]
    published_print: Option<DateParts>,
    published: Option<DateParts>,
    issued: Option<DateParts>,
    publisher: Option<String>,
    #[serde(rename = "container-title", default)]
    container_title: Vec<String>,
    #[serde(rename = "URL")]
    canonical_url: Option<String>,
    #[serde(default)]
    license: Vec<CrossrefLicense>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct CrossrefAuthor {
    pub given: Option<String>,
    pub family: Option<String>,
    #[serde(rename = "ORCID")]
    pub orcid: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DateParts {
    #[serde(rename = "date-parts")]
    date_parts: Vec<Vec<i32>>,
}

#[derive(Debug, Deserialize, Serialize)]
struct CrossrefLicense {
    #[serde(rename = "URL")]
    url: Option<String>,
    #[serde(rename = "start")]
    start: Option<DatePartsForExport>,
    #[serde(rename = "delay-in-days")]
    delay_in_days: Option<i64>,
}

#[derive(Debug, Deserialize, Serialize)]
struct DatePartsForExport {
    #[serde(rename = "date-parts")]
    date_parts: Vec<Vec<i32>>,
}

#[derive(Debug)]
pub(crate) struct ImportedWork {
    pub doi: String,
    pub title: String,
    pub work_type: &'static str,
    pub publication_year: Option<i32>,
    pub authors: Vec<CrossrefAuthor>,
    pub selected_metadata: Value,
}

impl CrossrefClient {
    pub(crate) fn new(mailto: String, user_agent: String) -> anyhow::Result<Self> {
        anyhow::ensure!(
            mailto.contains('@'),
            "CROSSREF_MAILTO must be an email address"
        );
        let client = Client::builder()
            .user_agent(user_agent)
            .timeout(Duration::from_secs(20))
            .build()
            .context("build Crossref HTTP client")?;
        Ok(Self {
            client,
            mailto,
            limiter: Arc::new(tokio::sync::Mutex::new(tokio::time::Instant::now())),
        })
    }

    pub(crate) async fn fetch(&self, doi: &str) -> Result<ImportedWork, CrossrefError> {
        // Serialize lookups and stay below even Crossref's public single-record
        // rate. The configured mailto still opts into and identifies the polite pool.
        let mut last_request = self.limiter.lock().await;
        let next_allowed = *last_request + Duration::from_millis(500);
        tokio::time::sleep_until(next_allowed).await;
        let mut url = Url::parse("https://api.crossref.org/v1/works/")
            .expect("static Crossref base URL is valid");
        url.path_segments_mut()
            .expect("Crossref URL supports path segments")
            .push(doi);
        url.query_pairs_mut().append_pair("mailto", &self.mailto);

        *last_request = tokio::time::Instant::now();
        let response = self.client.get(url).send().await?;
        match response.status() {
            StatusCode::NOT_FOUND => return Err(CrossrefError::NotFound),
            StatusCode::TOO_MANY_REQUESTS => return Err(CrossrefError::RateLimited),
            status if !status.is_success() => return Err(CrossrefError::Http(status)),
            _ => {}
        }
        let envelope: Envelope = response
            .json()
            .await
            .map_err(|error| CrossrefError::InvalidResponse(error.into()))?;
        ImportedWork::try_from(envelope.message)
    }

    pub(crate) async fn search(
        &self,
        query: &str,
        from_year: Option<i32>,
        until_year: Option<i32>,
        rows: u16,
    ) -> Result<Vec<ImportedWork>, CrossrefError> {
        let mut last_request = self.limiter.lock().await;
        let next_allowed = *last_request + Duration::from_millis(500);
        tokio::time::sleep_until(next_allowed).await;
        let mut url = Url::parse("https://api.crossref.org/v1/works")
            .expect("static Crossref works URL is valid");
        {
            let mut parameters = url.query_pairs_mut();
            parameters
                .append_pair("mailto", &self.mailto)
                .append_pair("query.bibliographic", query)
                .append_pair("rows", &rows.to_string())
                .append_pair(
                    "select",
                    "DOI,title,type,author,published,publisher,container-title,URL,license",
                );
            let mut filters = Vec::new();
            if let Some(year) = from_year {
                filters.push(format!("from-pub-date:{year}"));
            }
            if let Some(year) = until_year {
                filters.push(format!("until-pub-date:{year}"));
            }
            if !filters.is_empty() {
                parameters.append_pair("filter", &filters.join(","));
            }
        }
        *last_request = tokio::time::Instant::now();
        let response = self.client.get(url).send().await?;
        match response.status() {
            StatusCode::TOO_MANY_REQUESTS => return Err(CrossrefError::RateLimited),
            status if !status.is_success() => return Err(CrossrefError::Http(status)),
            _ => {}
        }
        let envelope: SearchEnvelope = response
            .json()
            .await
            .map_err(|error| CrossrefError::InvalidResponse(error.into()))?;
        Ok(envelope
            .message
            .items
            .into_iter()
            .filter_map(|work| ImportedWork::try_from(work).ok())
            .collect())
    }
}

impl TryFrom<CrossrefWork> for ImportedWork {
    type Error = CrossrefError;

    fn try_from(work: CrossrefWork) -> Result<Self, Self::Error> {
        let title = work
            .title
            .first()
            .map(|title| title.trim())
            .filter(|title| !title.is_empty())
            .ok_or_else(|| CrossrefError::InvalidResponse(anyhow::anyhow!("missing title")))?
            .to_owned();
        let publication_year = [&work.published_print, &work.published, &work.issued]
            .into_iter()
            .flatten()
            .find_map(|date| {
                date.date_parts
                    .first()
                    .and_then(|parts| parts.first())
                    .copied()
            });
        let mapped_type = match work.work_type.as_deref() {
            Some("journal-article") => "article",
            Some("proceedings-article") => "conference_paper",
            Some("posted-content") => "preprint",
            Some("dissertation") => "thesis",
            Some("report") => "technical_report",
            Some("book-chapter" | "reference-entry") => "book_chapter",
            _ => "other",
        };

        // Deliberately selected rather than raw: Crossref warns that deposited
        // abstracts can remain copyrighted. This cache never stores abstracts or full text.
        let selected_metadata = json!({
            "provider": "Crossref",
            "api_version": "v1",
            "doi": work.doi,
            "publisher": work.publisher,
            "container_title": work.container_title,
            "canonical_url": work.canonical_url,
            "license": work.license,
            "authors": work.author,
        });
        let doi = selected_metadata["doi"]
            .as_str()
            .expect("DOI was serialized as a string")
            .to_ascii_lowercase();
        let authors = serde_json::from_value(selected_metadata["authors"].clone())
            .map_err(|error| CrossrefError::InvalidResponse(error.into()))?;
        Ok(Self {
            doi,
            title,
            work_type: mapped_type,
            publication_year,
            authors,
            selected_metadata,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_selected_metadata_without_abstract() {
        let response: Envelope = serde_json::from_value(json!({
            "message": {
                "DOI": "10.1006/INCO.1994.1093",
                "title": ["A Syntactic Approach to Type Soundness"],
                "type": "journal-article",
                "published-print": {"date-parts": [[1994, 11]]},
                "author": [{"given": "Andrew K.", "family": "Wright"}],
                "abstract": "must not be retained"
            }
        }))
        .unwrap();
        let imported = ImportedWork::try_from(response.message).unwrap();
        assert_eq!(imported.publication_year, Some(1994));
        assert_eq!(imported.work_type, "article");
        assert_eq!(imported.doi, "10.1006/inco.1994.1093");
        assert!(imported.selected_metadata.get("abstract").is_none());
    }

    #[test]
    fn maps_search_results_without_abstracts() {
        let response: SearchEnvelope = serde_json::from_value(json!({
            "message": {"items": [{
                "DOI": "10.1006/inco.1994.1093",
                "title": ["A Syntactic Approach to Type Soundness"],
                "type": "journal-article",
                "published": {"date-parts": [[1994]]},
                "abstract": "must not be retained"
            }]}
        }))
        .unwrap();
        let imported: Vec<_> = response
            .message
            .items
            .into_iter()
            .filter_map(|work| ImportedWork::try_from(work).ok())
            .collect();
        assert_eq!(imported.len(), 1);
        assert!(imported[0].selected_metadata.get("abstract").is_none());
    }
}
