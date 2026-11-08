use crate::docs::Source;
use anyhow::{Context, Result, ensure};
use scraper::{Html, Selector};
use std::{io::Read, time::Duration};

pub fn search(query: &str) -> Result<Vec<Source>> {
    ensure!(
        !query.trim().is_empty() && query.len() <= 1000,
        "Search query must be 1–1000 bytes"
    );
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent("KYTTU-AI/0.2 (local desktop assistant)")
        .build()?;
    let response = client
        .get("https://html.duckduckgo.com/html/")
        .query(&[("q", query)])
        .send()?
        .error_for_status()?;
    let mut html = String::new();
    response.take(1_000_000).read_to_string(&mut html)?;
    parse_results(&html)
}

pub fn parse_results(raw: &str) -> Result<Vec<Source>> {
    let html = Html::parse_document(raw);
    let container = Selector::parse(".result").unwrap();
    let link = Selector::parse(".result__a").unwrap();
    let snippet = Selector::parse(".result__snippet").unwrap();
    let mut results = Vec::new();
    for result in html.select(&container) {
        let Some(a) = result.select(&link).next() else {
            continue;
        };
        let href = a.value().attr("href").unwrap_or("");
        let url = normalize_url(href);
        let Some(url) = url else {
            continue;
        };
        let title = a.text().collect::<Vec<_>>().join(" ");
        let text = result
            .select(&snippet)
            .next()
            .map(|s| s.text().collect::<Vec<_>>().join(" "))
            .unwrap_or_default();
        results.push(Source {
            path: url,
            line: 0,
            text: format!("{title}\n{text}"),
        });
        if results.len() == 4 {
            break;
        }
    }
    ensure!(
        !results.is_empty(),
        "Search returned no readable results. The provider may be rate-limiting or requiring a CAPTCHA. Try again later or use local docs."
    );
    Ok(results)
}
fn normalize_url(href: &str) -> Option<String> {
    let href = if href.starts_with("//") {
        format!("https:{href}")
    } else {
        href.to_owned()
    };
    let mut url = reqwest::Url::parse(&href).ok()?;
    if url
        .host_str()
        .is_some_and(|h| h == "duckduckgo.com" || h.ends_with(".duckduckgo.com"))
        && let Some((_, target)) = url.query_pairs().find(|(k, _)| k == "uddg")
    {
        url = reqwest::Url::parse(&target).ok()?;
    }
    if !matches!(url.scheme(), "https" | "http") {
        return None;
    }
    url.host_str().context("missing host").ok()?;
    Some(url.to_string())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_search_references_without_executing_markup() -> Result<()> {
        let sources = parse_results(
            r#"<div class="result"><a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fdocs">Docs</a><a class="result__snippet">Useful &amp; current</a></div>"#,
        )?;
        assert_eq!(sources[0].path, "https://example.com/docs");
        assert!(sources[0].text.contains("Useful & current"));
        assert!(normalize_url("javascript:alert(1)").is_none());
        Ok(())
    }
}
