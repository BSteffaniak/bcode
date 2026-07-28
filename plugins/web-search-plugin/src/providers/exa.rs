//! Exa Search API adapter.

use crate::{WebError, html_text};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};

const SEARCH_ENDPOINT: &str = "https://api.exa.ai/search";
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;
const MAX_RESPONSE_BODY_BYTES: usize = 2 * 1024 * 1024;
const DEFAULT_CONTENT_CHARACTERS: usize = 500;
const MAX_CONTENT_CHARACTERS: usize = 20_000;
const MAX_DOMAINS: usize = 1_200;
const MAX_TEXT_FILTERS: usize = 1;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ExaSearchOptions {
    search_type: Option<ExaSearchType>,
    category: Option<ExaCategory>,
    include_domains: Vec<String>,
    exclude_domains: Vec<String>,
    start_published_date: Option<String>,
    end_published_date: Option<String>,
    start_crawl_date: Option<String>,
    end_crawl_date: Option<String>,
    include_text: Vec<String>,
    exclude_text: Vec<String>,
    content: Option<ExaContentMode>,
    max_characters: Option<usize>,
    max_age_hours: Option<i64>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
enum ExaSearchType {
    Auto,
    Fast,
    Instant,
    DeepLite,
    Deep,
    DeepReasoning,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum ExaCategory {
    Company,
    People,
    Publication,
    News,
    PersonalSite,
    FinancialReport,
}

impl ExaCategory {
    const fn api_value(self) -> &'static str {
        match self {
            Self::Company => "company",
            Self::People => "people",
            Self::Publication => "publication",
            Self::News => "news",
            Self::PersonalSite => "personal site",
            Self::FinancialReport => "financial report",
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ExaContentMode {
    Text,
    Highlights,
    Summary,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ExaSearchRequest {
    query: String,
    num_results: usize,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    search_type: Option<ExaSearchType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    category: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    include_domains: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    exclude_domains: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    start_published_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    end_published_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    start_crawl_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    end_crawl_date: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    include_text: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    exclude_text: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    user_location: Option<String>,
    contents: ExaContents,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ExaContents {
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<ExaTextContents>,
    #[serde(skip_serializing_if = "Option::is_none")]
    highlights: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_age_hours: Option<i64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ExaTextContents {
    max_characters: usize,
}

#[derive(Debug, Deserialize)]
struct ExaSearchResponse {
    #[serde(default)]
    results: Vec<ExaResult>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExaResult {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    url: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    highlights: Vec<String>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    published_date: Option<String>,
}

#[derive(Debug)]
pub struct NormalizedResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
    pub published: Option<String>,
}

pub struct SearchInput<'a> {
    pub query: &'a str,
    pub max_results: usize,
    pub site: Option<&'a str>,
    pub freshness: Option<&'a str>,
    pub region: Option<&'a str>,
    pub safe_search: Option<&'a str>,
    pub provider_options: Option<Value>,
}

pub async fn search(
    client: &Client,
    api_key: &str,
    input: SearchInput<'_>,
) -> Result<Vec<NormalizedResult>, WebError> {
    search_at_endpoint(client, api_key, input, SEARCH_ENDPOINT).await
}

async fn search_at_endpoint(
    client: &Client,
    api_key: &str,
    input: SearchInput<'_>,
    endpoint: &str,
) -> Result<Vec<NormalizedResult>, WebError> {
    let max_results = input.max_results;
    let body = build_request(input)?;
    let response = client
        .post(endpoint)
        .header("Accept", "application/json")
        .header("x-api-key", api_key)
        .json(&body)
        .send()
        .await?;
    let status = response.status();
    let content_length = response.content_length();
    let limit = if status.is_success() {
        MAX_RESPONSE_BODY_BYTES
    } else {
        MAX_ERROR_BODY_BYTES
    };
    if content_length.is_some_and(|length| length > limit as u64) {
        return Err(WebError::InvalidRequest(format!(
            "Exa response exceeded the {limit}-byte limit"
        )));
    }
    let response_body = read_bounded_body(response, limit).await?;
    if !status.is_success() {
        let message = match status.as_u16() {
            401 | 403 => "Exa authentication failed; verify EXA_API_KEY".to_string(),
            429 => "Exa rate limit or quota was exceeded; retry after the provider limit resets"
                .to_string(),
            code if code >= 500 => "Exa is temporarily unavailable".to_string(),
            _ => format!(
                "Exa rejected the search request: {}",
                sanitize_error_body(&response_body)
            ),
        };
        return Err(WebError::Http {
            status: status.as_u16(),
            body: message,
        });
    }
    normalize_response(&response_body, max_results)
}

async fn read_bounded_body(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<String, WebError> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len().saturating_add(chunk.len()) > limit {
            return Err(WebError::InvalidRequest(format!(
                "Exa response exceeded the {limit}-byte limit"
            )));
        }
        body.extend_from_slice(&chunk);
    }
    String::from_utf8(body)
        .map_err(|_| WebError::InvalidRequest("Exa returned non-UTF-8 content".to_string()))
}

fn sanitize_error_body(body: &str) -> String {
    let text = html_text(body);
    let text = text
        .split_whitespace()
        .filter(|part| !part.to_ascii_lowercase().contains("api_key"))
        .collect::<Vec<_>>()
        .join(" ");
    truncate_chars(&text, 500)
}

fn build_request(input: SearchInput<'_>) -> Result<Value, WebError> {
    build_request_at(input, SystemTime::now())
}

fn build_request_at(input: SearchInput<'_>, now: SystemTime) -> Result<Value, WebError> {
    let SearchInput {
        query,
        max_results,
        site,
        freshness,
        region,
        safe_search,
        provider_options,
    } = input;
    let mut options =
        provider_options.map_or_else(|| Ok(ExaSearchOptions::default()), serde_json::from_value)?;
    validate_options(&options)?;

    if safe_search.is_some_and(|value| !value.trim().is_empty()) {
        return Err(WebError::InvalidRequest(
            "safe_search is not supported by Exa".to_string(),
        ));
    }
    if let Some(site) = site.map(str::trim).filter(|site| !site.is_empty()) {
        validate_domain(site)?;
        if !options.include_domains.is_empty()
            && !options.include_domains.iter().any(|domain| domain == site)
        {
            return Err(WebError::InvalidRequest(
                "site conflicts with provider_options.include_domains".to_string(),
            ));
        }
        if options.include_domains.is_empty() {
            options.include_domains.push(site.to_string());
        }
    }
    if let Some(freshness) = freshness.map(str::trim).filter(|value| !value.is_empty()) {
        if options.start_published_date.is_some() {
            return Err(WebError::InvalidRequest(
                "freshness conflicts with provider_options.start_published_date".to_string(),
            ));
        }
        options.start_published_date = Some(freshness_start(freshness, now)?);
    }
    let user_location = region
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(validate_region)
        .transpose()?;

    let max_characters = options.max_characters.unwrap_or(DEFAULT_CONTENT_CHARACTERS);
    let mode = options.content.unwrap_or(ExaContentMode::Highlights);
    let contents = match mode {
        ExaContentMode::Text => ExaContents {
            text: Some(ExaTextContents { max_characters }),
            highlights: None,
            summary: None,
            max_age_hours: options.max_age_hours,
        },
        ExaContentMode::Highlights => ExaContents {
            text: None,
            highlights: Some(true),
            summary: None,
            max_age_hours: options.max_age_hours,
        },
        ExaContentMode::Summary => ExaContents {
            text: None,
            highlights: None,
            summary: Some(true),
            max_age_hours: options.max_age_hours,
        },
    };
    let request = ExaSearchRequest {
        query: query.trim().to_string(),
        num_results: max_results,
        search_type: options.search_type,
        category: options
            .category
            .map(|category| category.api_value().to_string()),
        include_domains: options.include_domains,
        exclude_domains: options.exclude_domains,
        start_published_date: options.start_published_date,
        end_published_date: options.end_published_date,
        start_crawl_date: options.start_crawl_date,
        end_crawl_date: options.end_crawl_date,
        include_text: options.include_text,
        exclude_text: options.exclude_text,
        user_location,
        contents,
    };
    serde_json::to_value(request).map_err(WebError::Decode)
}

fn validate_options(options: &ExaSearchOptions) -> Result<(), WebError> {
    validate_domains("include_domains", &options.include_domains)?;
    validate_domains("exclude_domains", &options.exclude_domains)?;
    if options.include_text.len() > MAX_TEXT_FILTERS
        || options.exclude_text.len() > MAX_TEXT_FILTERS
    {
        return Err(WebError::InvalidRequest(
            "Exa accepts at most one include_text and one exclude_text value".to_string(),
        ));
    }
    if options
        .include_text
        .iter()
        .chain(&options.exclude_text)
        .any(|value| value.trim().is_empty())
    {
        return Err(WebError::InvalidRequest(
            "Exa text filters must not be empty".to_string(),
        ));
    }
    if let Some(max_age_hours) = options.max_age_hours
        && max_age_hours < -1
    {
        return Err(WebError::InvalidRequest(
            "provider_options.max_age_hours must be -1 or greater".to_string(),
        ));
    }
    if let Some(max_characters) = options.max_characters
        && !(1..=MAX_CONTENT_CHARACTERS).contains(&max_characters)
    {
        return Err(WebError::InvalidRequest(format!(
            "provider_options.max_characters must be between 1 and {MAX_CONTENT_CHARACTERS}"
        )));
    }
    for (name, value) in [
        ("start_published_date", &options.start_published_date),
        ("end_published_date", &options.end_published_date),
        ("start_crawl_date", &options.start_crawl_date),
        ("end_crawl_date", &options.end_crawl_date),
    ] {
        if let Some(value) = value {
            validate_iso_date(name, value)?;
        }
    }
    validate_range(
        "published date",
        options.start_published_date.as_deref(),
        options.end_published_date.as_deref(),
    )?;
    validate_range(
        "crawl date",
        options.start_crawl_date.as_deref(),
        options.end_crawl_date.as_deref(),
    )?;
    if matches!(
        options.category,
        Some(ExaCategory::Company | ExaCategory::People)
    ) && (options.start_published_date.is_some()
        || options.end_published_date.is_some()
        || !options.exclude_domains.is_empty())
    {
        return Err(WebError::InvalidRequest(
            "Exa company and people categories do not support publication dates or exclude_domains"
                .to_string(),
        ));
    }
    Ok(())
}

fn validate_domains(name: &str, domains: &[String]) -> Result<(), WebError> {
    if domains.len() > MAX_DOMAINS {
        return Err(WebError::InvalidRequest(format!(
            "provider_options.{name} accepts at most {MAX_DOMAINS} domains"
        )));
    }
    for domain in domains {
        validate_domain(domain)?;
    }
    Ok(())
}

fn validate_domain(domain: &str) -> Result<(), WebError> {
    let domain = domain.trim();
    if domain.is_empty()
        || domain.contains(char::is_whitespace)
        || domain.contains('/')
        || domain.contains(':')
    {
        return Err(WebError::InvalidRequest(format!(
            "invalid Exa domain filter: {domain}"
        )));
    }
    Ok(())
}

fn validate_region(region: &str) -> Result<String, WebError> {
    if region.len() != 2 || !region.bytes().all(|byte| byte.is_ascii_alphabetic()) {
        return Err(WebError::InvalidRequest(
            "Exa region must be a two-letter country code".to_string(),
        ));
    }
    Ok(region.to_ascii_uppercase())
}

fn validate_iso_date(name: &str, value: &str) -> Result<(), WebError> {
    let (date, time) = value
        .split_once('T')
        .map_or((value, None), |(date, time)| (date, Some(time)));
    let mut parts = date.split('-');
    let year = parse_date_part(parts.next(), 4);
    let month = parse_date_part(parts.next(), 2);
    let day = parse_date_part(parts.next(), 2);
    let valid_date = parts.next().is_none()
        && year.is_some()
        && month.is_some_and(|month| (1..=12).contains(&month))
        && day.is_some_and(|day| {
            day >= 1
                && month.is_some_and(|month| day <= days_in_month(year.unwrap_or_default(), month))
        });
    let valid_time = time.is_none_or(valid_iso_time);
    if !valid_date || !valid_time {
        return Err(WebError::InvalidRequest(format!(
            "provider_options.{name} must be an ISO 8601 date or timestamp"
        )));
    }
    Ok(())
}

fn parse_date_part(value: Option<&str>, width: usize) -> Option<u32> {
    let value = value?;
    (value.len() == width && value.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| value.parse().ok())
        .flatten()
}

const fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        4 | 6 | 9 | 11 => 30,
        2 if year.is_multiple_of(400) || (year.is_multiple_of(4) && !year.is_multiple_of(100)) => {
            29
        }
        2 => 28,
        _ => 31,
    }
}

fn valid_iso_time(value: &str) -> bool {
    let (clock, zone_valid) = value.strip_suffix('Z').map_or_else(
        || {
            if value.len() >= 6 {
                let zone_start = value.len() - 6;
                let (clock, zone) = value.split_at(zone_start);
                let zone_bytes = zone.as_bytes();
                let valid_zone = matches!(zone_bytes[0], b'+' | b'-')
                    && zone_bytes[3] == b':'
                    && parse_date_part(Some(&zone[1..3]), 2).is_some_and(|hour| hour <= 23)
                    && parse_date_part(Some(&zone[4..6]), 2).is_some_and(|minute| minute <= 59);
                (clock, valid_zone)
            } else {
                (value, false)
            }
        },
        |clock| (clock, true),
    );
    let base_clock = clock.split_once('.').map_or(clock, |(base, fraction)| {
        if fraction.is_empty() || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
            ""
        } else {
            base
        }
    });
    let mut parts = base_clock.split(':');
    let hour = parse_date_part(parts.next(), 2);
    let minute = parse_date_part(parts.next(), 2);
    let second = parse_date_part(parts.next(), 2);
    zone_valid
        && parts.next().is_none()
        && hour.is_some_and(|hour| hour <= 23)
        && minute.is_some_and(|minute| minute <= 59)
        && second.is_some_and(|second| second <= 59)
}

fn validate_range(name: &str, start: Option<&str>, end: Option<&str>) -> Result<(), WebError> {
    if let (Some(start), Some(end)) = (start, end)
        && start > end
    {
        return Err(WebError::InvalidRequest(format!(
            "Exa {name} start must not be after end"
        )));
    }
    Ok(())
}

fn freshness_start(value: &str, now: SystemTime) -> Result<String, WebError> {
    let days = match value.to_ascii_lowercase().as_str() {
        "day" => 1,
        "week" => 7,
        "month" => 30,
        "year" => 365,
        _ => {
            return Err(WebError::InvalidRequest(
                "Exa freshness must be one of day, week, month, or year".to_string(),
            ));
        }
    };
    let seconds = now
        .duration_since(UNIX_EPOCH)
        .map_err(|_| WebError::InvalidRequest("system clock is before Unix epoch".to_string()))?
        .as_secs();
    let current_days = i64::try_from(seconds / 86_400)
        .map_err(|_| WebError::InvalidRequest("system clock is out of range".to_string()))?;
    let (year, month, day) = civil_from_days(current_days - days);
    Ok(format!("{year:04}-{month:02}-{day:02}T00:00:00Z"))
}

// Howard Hinnant's civil-from-days algorithm. Input is days since 1970-01-01.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

fn normalize_response(body: &str, max_results: usize) -> Result<Vec<NormalizedResult>, WebError> {
    let decoded = serde_json::from_str::<ExaSearchResponse>(body)?;
    Ok(decoded
        .results
        .into_iter()
        .filter(|result| !result.url.trim().is_empty())
        .take(max_results)
        .map(|result| {
            let snippet = result
                .summary
                .filter(|value| !value.trim().is_empty())
                .or_else(|| (!result.highlights.is_empty()).then(|| result.highlights.join(" ")))
                .or(result.text)
                .map_or_else(String::new, |value| {
                    truncate_chars(&html_text(&value), MAX_CONTENT_CHARACTERS)
                });
            NormalizedResult {
                title: result
                    .title
                    .map_or_else(String::new, |value| html_text(&value)),
                url: result.url,
                snippet,
                published: result.published_date,
            }
        })
        .collect())
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    value.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn fixed_now() -> SystemTime {
        UNIX_EPOCH + Duration::from_hours(482_136) // 2025-01-01T00:00:00Z
    }

    #[test]
    fn minimal_request_uses_bounded_highlights() {
        let request = build_request_at(
            SearchInput {
                query: "rust agents",
                max_results: 3,
                site: None,
                freshness: None,
                region: None,
                safe_search: None,
                provider_options: None,
            },
            fixed_now(),
        )
        .expect("request");
        assert_eq!(request["query"], "rust agents");
        assert_eq!(request["numResults"], 3);
        assert_eq!(request["contents"]["highlights"], true);
        assert!(request.get("includeDomains").is_none());
    }

    #[test]
    fn generic_site_region_and_freshness_use_native_fields() {
        let request = build_request_at(
            SearchInput {
                query: "rust agents",
                max_results: 3,
                site: Some("example.com"),
                freshness: Some("week"),
                region: Some("us"),
                safe_search: None,
                provider_options: None,
            },
            fixed_now(),
        )
        .expect("request");
        assert_eq!(request["query"], "rust agents");
        assert_eq!(
            request["includeDomains"],
            serde_json::json!(["example.com"])
        );
        assert_eq!(request["startPublishedDate"], "2024-12-25T00:00:00Z");
        assert_eq!(request["userLocation"], "US");
    }

    #[test]
    fn all_freshness_values_are_deterministic() {
        for (freshness, expected) in [
            ("day", "2024-12-31T00:00:00Z"),
            ("week", "2024-12-25T00:00:00Z"),
            ("month", "2024-12-02T00:00:00Z"),
            ("year", "2024-01-02T00:00:00Z"),
        ] {
            let request = build_request_at(
                SearchInput {
                    query: "q",
                    max_results: 1,
                    site: None,
                    freshness: Some(freshness),
                    region: None,
                    safe_search: None,
                    provider_options: None,
                },
                fixed_now(),
            )
            .expect("request");
            assert_eq!(request["startPublishedDate"], expected);
        }
    }

    #[test]
    fn advanced_options_serialize_with_api_field_names() {
        let options = serde_json::json!({
            "search_type": "deep-lite",
            "category": "publication",
            "exclude_domains": ["spam.example"],
            "start_crawl_date": "2024-01-01T00:00:00Z",
            "include_text": ["Rust"],
            "content": "text",
            "max_characters": 1500,
            "max_age_hours": 24
        });
        let request = build_request_at(
            SearchInput {
                query: "q",
                max_results: 5,
                site: None,
                freshness: None,
                region: None,
                safe_search: None,
                provider_options: Some(options),
            },
            fixed_now(),
        )
        .expect("request");
        assert_eq!(request["type"], "deep-lite");
        assert_eq!(request["category"], "publication");
        assert_eq!(
            request["excludeDomains"],
            serde_json::json!(["spam.example"])
        );
        assert_eq!(request["contents"]["text"]["maxCharacters"], 1500);
        assert_eq!(request["contents"]["maxAgeHours"], 24);
    }

    #[test]
    fn unknown_options_and_conflicts_are_rejected() {
        let invalid_age = build_request_at(
            SearchInput {
                query: "q",
                max_results: 1,
                site: None,
                freshness: None,
                region: None,
                safe_search: None,
                provider_options: Some(serde_json::json!({"max_age_hours": -2})),
            },
            fixed_now(),
        );
        assert!(invalid_age.is_err());
        let unknown = build_request_at(
            SearchInput {
                query: "q",
                max_results: 1,
                site: None,
                freshness: None,
                region: None,
                safe_search: None,
                provider_options: Some(serde_json::json!({"future": true})),
            },
            fixed_now(),
        );
        assert!(unknown.is_err());
        let conflict = build_request_at(
            SearchInput {
                query: "q",
                max_results: 1,
                site: Some("a.example"),
                freshness: None,
                region: None,
                safe_search: None,
                provider_options: Some(serde_json::json!({"include_domains": ["b.example"]})),
            },
            fixed_now(),
        );
        assert!(conflict.is_err());
        assert!(
            build_request_at(
                SearchInput {
                    query: "q",
                    max_results: 1,
                    site: None,
                    freshness: Some("hour"),
                    region: None,
                    safe_search: None,
                    provider_options: None
                },
                fixed_now()
            )
            .is_err()
        );
        assert!(
            build_request_at(
                SearchInput {
                    query: "q",
                    max_results: 1,
                    site: None,
                    freshness: None,
                    region: None,
                    safe_search: Some("strict"),
                    provider_options: None
                },
                fixed_now()
            )
            .is_err()
        );
    }

    #[test]
    fn dates_are_semantically_validated() {
        for valid in [
            "2024-02-29",
            "2025-01-01T12:30:59Z",
            "2025-01-01T12:30:59.123+05:30",
        ] {
            validate_iso_date("date", valid).expect("valid date");
        }
        for invalid in [
            "2025-02-29",
            "2024-13-01",
            "2024-01-32",
            "2024-01-01T25:00:00Z",
            "2024-01-01junk",
        ] {
            assert!(validate_iso_date("date", invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn category_restrictions_are_rejected_before_network_use() {
        let options = serde_json::json!({
            "category": "people",
            "start_published_date": "2024-01-01"
        });
        assert!(
            build_request_at(
                SearchInput {
                    query: "q",
                    max_results: 1,
                    site: None,
                    freshness: None,
                    region: None,
                    safe_search: None,
                    provider_options: Some(options)
                },
                fixed_now()
            )
            .is_err()
        );
    }

    #[test]
    fn response_normalization_prefers_summary_then_highlights_then_text() {
        let response = r#"{
            "results": [
                {"title":"<b>One</b>","url":"https://one.example","summary":"summary","highlights":["highlight"],"text":"text","publishedDate":"2025-01-01"},
                {"url":"https://two.example","highlights":["first", "second"],"text":"text"},
                {"url":"https://three.example","text":"plain"},
                {"url":""}
            ],
            "futureField": true
        }"#;
        let results = normalize_response(response, 10).expect("response");
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].title, "One");
        assert_eq!(results[0].snippet, "summary");
        assert_eq!(results[1].snippet, "first second");
        assert_eq!(results[2].snippet, "plain");
        assert_eq!(results[0].published.as_deref(), Some("2025-01-01"));
    }

    async fn mock_response(status: &str, body: &str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let status = status.to_string();
        let body = body.to_string();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut request = vec![0_u8; 8 * 1024];
            let _ = socket.read(&mut request).await.expect("read");
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.expect("write");
        });
        format!("http://{address}/search")
    }

    fn test_input() -> SearchInput<'static> {
        SearchInput {
            query: "q",
            max_results: 1,
            site: None,
            freshness: None,
            region: None,
            safe_search: None,
            provider_options: None,
        }
    }

    #[tokio::test]
    async fn http_errors_are_normalized_without_provider_bodies() {
        let client = Client::new();
        for (status, expected) in [
            ("401 Unauthorized", "authentication failed"),
            ("429 Too Many Requests", "rate limit or quota"),
            ("503 Service Unavailable", "temporarily unavailable"),
        ] {
            let endpoint = mock_response(status, r#"{"error":"api_key=reflected-secret"}"#).await;
            let error = search_at_endpoint(&client, "test-key", test_input(), &endpoint)
                .await
                .expect_err("error response");
            let message = error.to_string();
            assert!(message.contains(expected));
            assert!(!message.contains("reflected-secret"));
            assert!(!message.contains("test-key"));
        }
    }

    #[tokio::test]
    #[ignore = "requires EXA_API_KEY and consumes live Exa quota"]
    async fn live_exa_smoke() {
        let api_key = std::env::var("EXA_API_KEY").expect("EXA_API_KEY must be set");
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("client");
        let basic = search(
            &client,
            &api_key,
            SearchInput {
                query: "Rust programming language official website",
                max_results: 2,
                site: Some("rust-lang.org"),
                freshness: None,
                region: None,
                safe_search: None,
                provider_options: None,
            },
        )
        .await
        .expect("live basic Exa search");
        assert!(!basic.is_empty());
        assert!(basic.len() <= 2);
        assert!(basic.iter().all(|result| {
            result.url.starts_with("https://")
                && !result.title.trim().is_empty()
                && result.snippet.chars().count() <= MAX_CONTENT_CHARACTERS
        }));

        let rich = search(
            &client,
            &api_key,
            SearchInput {
                query: "recent Rust language release",
                max_results: 1,
                site: Some("blog.rust-lang.org"),
                freshness: Some("year"),
                region: None,
                safe_search: None,
                provider_options: Some(serde_json::json!({
                    "search_type": "fast",
                    "content": "text",
                    "max_characters": 500
                })),
            },
        )
        .await
        .expect("live filtered Exa search");
        assert!(!rich.is_empty());
        assert!(rich.len() <= 1);
        assert!(rich.iter().all(|result| {
            result.url.starts_with("https://")
                && !result.title.trim().is_empty()
                && !result.snippet.trim().is_empty()
                && result.snippet.chars().count() <= 500
        }));
    }

    #[tokio::test]
    async fn request_timeout_is_reported_as_network_failure() {
        use tokio::io::AsyncReadExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut request = vec![0_u8; 8 * 1024];
            let _ = socket.read(&mut request).await.expect("read");
            tokio::time::sleep(Duration::from_millis(100)).await;
        });
        let client = Client::builder()
            .timeout(Duration::from_millis(5))
            .build()
            .expect("client");
        let error = search_at_endpoint(
            &client,
            "test-key",
            test_input(),
            &format!("http://{address}/search"),
        )
        .await
        .expect_err("timeout");
        let message = error.to_string();
        assert!(message.contains("network request failed"));
        assert!(!message.contains("test-key"));
    }

    #[tokio::test]
    async fn mock_http_success_decodes_normalized_results() {
        let endpoint = mock_response(
            "200 OK",
            r#"{"results":[{"title":"Result","url":"https://example.com","highlights":["useful"]}]}"#,
        )
        .await;
        let results = search_at_endpoint(&Client::new(), "test-key", test_input(), &endpoint)
            .await
            .expect("success");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].snippet, "useful");
    }

    #[test]
    fn provider_error_sanitization_is_bounded_and_removes_key_markers() {
        let body = format!("bad api_key=secret {}", "x".repeat(1_000));
        let sanitized = sanitize_error_body(&body);
        assert!(!sanitized.contains("api_key"));
        assert!(sanitized.chars().count() <= 500);
    }

    #[test]
    fn malformed_response_is_rejected_and_results_are_limited() {
        assert!(normalize_response("not json", 2).is_err());
        let body = r#"{"results":[{"url":"https://a"},{"url":"https://b"},{"url":"https://c"}]}"#;
        assert_eq!(normalize_response(body, 2).expect("response").len(), 2);
    }
}
