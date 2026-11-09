use crate::docs::Source;
use anyhow::{Context, Result, ensure};
use scraper::{Html, Selector};
use std::{io::Read, sync::LazyLock, time::Duration};

/// Generic User-Agent: no app name or version that would identify this install.
pub const USER_AGENT: &str = "Mozilla/5.0";
const QUERY_CHARS: usize = 120;

/// One client for every search, so connections and the TLS setup are reused. A short connect
/// timeout makes a bad network or captive portal fail fast instead of holding up the reply.
static CLIENT: LazyLock<reqwest::blocking::Client> = LazyLock::new(|| {
    reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(4))
        .timeout(Duration::from_secs(20))
        .user_agent(USER_AGENT)
        .build()
        .expect("web search HTTP client")
});

/// A short search query from a chat question: code blocks dropped, whitespace
/// collapsed, cut at a word boundary. Callers should still show it for review.
pub fn query_from(question: &str) -> String {
    let mut in_code = false;
    let text = question
        .lines()
        .filter(|line| {
            let fence = line.trim_start().starts_with("```");
            in_code ^= fence;
            !fence && !in_code
        })
        .flat_map(str::split_whitespace)
        .collect::<Vec<_>>()
        .join(" ");
    if text.chars().count() <= QUERY_CHARS {
        return text;
    }
    let cut: String = text.chars().take(QUERY_CHARS).collect();
    match cut.rfind(' ') {
        Some(space) if space > QUERY_CHARS / 2 => cut[..space].to_owned(),
        _ => cut,
    }
}

pub fn search(query: &str) -> Result<Vec<Source>> {
    ensure!(
        !query.trim().is_empty() && query.len() <= 1000,
        "Search query must be 1–1000 bytes"
    );
    let response = CLIENT
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
    #[test]
    fn query_drops_code_and_stays_short() {
        assert_eq!(
            query_from("Why does\n```rust\nlet x = 1;\n```\nthis   fail?"),
            "Why does this fail?"
        );
        let long = query_from(&"word ".repeat(100));
        assert!(long.len() <= QUERY_CHARS && long.ends_with("word"));
        assert_eq!(USER_AGENT, "Mozilla/5.0");
    }
    #[test]
    fn perf_parse_search_results() {
        // A results page about the size DuckDuckGo returns, most of it markup to skip.
        let mut page = String::from("<html><body>");
        for i in 0..30 {
            page.push_str(&format!(
                r#"<div class="result"><a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2F{i}">Title {i}</a><a class="result__snippet">{}</a></div>"#,
                "snippet text ".repeat(40)
            ));
            page.push_str(&"<div><span>filler</span></div>".repeat(40));
        }
        page.push_str("</body></html>");
        let start = std::time::Instant::now();
        let sources = parse_results(&page).unwrap();
        let elapsed = start.elapsed();
        eprintln!(
            "perf_parse_search_results: {} KB page in {:.2} ms",
            page.len() / 1024,
            elapsed.as_secs_f64() * 1000.0
        );
        assert_eq!(sources.len(), 4);
        assert!(elapsed < Duration::from_secs(2), "parsing took {elapsed:?}");
    }
}
