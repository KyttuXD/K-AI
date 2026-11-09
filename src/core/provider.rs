use super::{
    config::Config,
    engine::{self, Event},
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader},
    net::IpAddr,
    sync::{
        LazyLock, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::Sender,
    },
    time::{Duration, Instant},
};

fn loopback(host: &str) -> bool {
    host == "localhost" || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// Private-network hosts, where plain http stays on the local network: RFC 1918, link-local,
/// CGNAT/Tailscale (100.64/10), IPv6 unique/link-local, and mDNS or home names.
fn private_host(host: &str) -> bool {
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => {
            let [a, b, ..] = ip.octets();
            ip.is_private()
                || ip.is_link_local()
                || ip.is_loopback()
                || (a == 100 && (b & 0xC0) == 64)
        }
        Ok(IpAddr::V6(ip)) => {
            ip.is_loopback() || ip.is_unique_local() || ip.is_unicast_link_local()
        }
        Err(_) => [".local", ".lan", ".home.arpa"]
            .iter()
            .any(|s| host.to_ascii_lowercase().ends_with(s)),
    }
}

/// True when `config.trusted_endpoints` lists the host (and the port, when the entry has one).
/// Entries may be `host`, `host:port` or a URL.
fn trusted(config: &Config, host: &str, port: Option<u16>) -> bool {
    config.trusted_endpoints.iter().any(|entry| {
        let entry = entry.trim();
        let url = if entry.contains("://") {
            reqwest::Url::parse(entry)
        } else {
            reqwest::Url::parse(&format!("http://{entry}"))
        };
        !entry.is_empty()
            && url.is_ok_and(|u| {
                u.host_str().is_some_and(|h| {
                    h.trim_start_matches('[')
                        .trim_end_matches(']')
                        .eq_ignore_ascii_case(host)
                }) && (u.port().is_none() || u.port() == port)
            })
    })
}

/// The direct endpoint's root URL. Loopback always works; another host needs LAN inference
/// allowed, the host in the trusted list, and https unless it is a private-network address.
pub fn direct_base(config: &Config) -> Result<String> {
    let url = reqwest::Url::parse(config.direct_url.trim())?;
    ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none(),
        "Direct endpoints must be http(s) URLs without credentials or query strings"
    );
    let host = url
        .host_str()
        .context("The endpoint URL has no host")?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_owned();
    if !loopback(&host) {
        ensure!(
            config.allow_lan,
            "Direct endpoints must use localhost or a loopback IP unless LAN inference is allowed in Settings"
        );
        ensure!(
            trusted(config, &host, url.port_or_known_default()),
            "Add {host} to the trusted endpoints to use it"
        );
        ensure!(
            url.scheme() == "https" || private_host(&host),
            "{host} is not a private network address; use https://"
        );
    }
    ensure!(
        matches!(url.path(), "/" | "/v1" | "/v1/"),
        "Use a root URL such as http://127.0.0.1:8080"
    );
    Ok(config
        .direct_url
        .trim()
        .trim_end_matches('/')
        .trim_end_matches("/v1")
        .to_owned())
}

/// One HTTP client for every model request (llama.cpp and Ollama), so its connection pool,
/// worker thread and TLS setup are reused. Use `RequestBuilder::timeout` for shorter calls.
static CLIENT: LazyLock<reqwest::blocking::Client> = LazyLock::new(|| {
    reqwest::blocking::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(300))
        .build()
        .expect("model HTTP client")
});
pub fn http() -> &'static reqwest::blocking::Client {
    &CLIENT
}

pub fn models(config: &Config) -> Result<Vec<String>> {
    if !config.direct_backend {
        return engine::models();
    }
    let value: Value = CLIENT
        .get(format!("{}/v1/models", direct_base(config)?))
        .timeout(Duration::from_secs(5))
        .send()?
        .error_for_status()?
        .json()?;
    Ok(value["data"]
        .as_array()
        .context("Invalid /v1/models response")?
        .iter()
        .filter_map(|m| m["id"].as_str().map(str::to_owned))
        .collect())
}

/// Checks that the selected model is served, without a round trip when the app's own
/// llama-server serves it (its `--alias` is the model name) or a check passed recently.
pub fn ensure_model(config: &Config) -> Result<()> {
    static LAST_OK: Mutex<Option<(String, Instant)>> = Mutex::new(None);
    if config.direct_backend
        && !config.gguf_model.as_os_str().is_empty()
        && config.model == super::llama::model_alias(config)
    {
        return Ok(());
    }
    let key = format!(
        "{}|{}|{}",
        config.direct_backend,
        config.direct_url.trim(),
        config.model
    );
    let fresh = |last: &Option<(String, Instant)>| {
        last.as_ref()
            .is_some_and(|(k, at)| *k == key && at.elapsed() < Duration::from_secs(120))
    };
    if fresh(&LAST_OK.lock().unwrap_or_else(|e| e.into_inner())) {
        return Ok(());
    }
    ensure!(
        models(config)?.contains(&config.model),
        "Select an installed local chat model in Settings"
    );
    *LAST_OK.lock().unwrap_or_else(|e| e.into_inner()) = Some((key, Instant::now()));
    Ok(())
}

/// Ollama takes images as a base64 list beside plain-text content, not as content parts.
fn ollama_messages(messages: &[Value]) -> Vec<Value> {
    messages
        .iter()
        .map(|m| {
            let Some(parts) = m["content"].as_array() else {
                return m.clone();
            };
            let (mut text, mut images) = (String::new(), vec![]);
            for p in parts {
                match p["type"].as_str() {
                    Some("text") => {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(p["text"].as_str().unwrap_or(""));
                    }
                    Some("image_url") => {
                        if let Some(url) = p["image_url"]["url"].as_str() {
                            images.push(url.split_once("base64,").map_or(url, |(_, d)| d));
                        }
                    }
                    _ => {}
                }
            }
            let mut out = json!({"role":m["role"],"content":text});
            if !images.is_empty() {
                out["images"] = json!(images);
            }
            out
        })
        .collect()
}

fn openai_messages(messages: &[Value]) -> Vec<Value> {
    messages
        .iter()
        .map(|m| {
            let mut m = m.clone();
            if let Some(calls) = m["tool_calls"].as_array_mut() {
                for call in calls {
                    call["type"] = json!("function");
                    if !call["function"]["arguments"].is_string() {
                        call["function"]["arguments"] =
                            json!(call["function"]["arguments"].to_string());
                    }
                }
            }
            if let Some(object) = m.as_object_mut() {
                object.remove("tool_name");
            }
            m
        })
        .collect()
}

/// One model reply. `prompt_tokens` is the server's own count of the prompt, when it reports one.
pub struct Reply {
    pub content: String,
    pub calls: Vec<Value>,
    pub prompt_tokens: Option<usize>,
}

/// True when the direct endpoint is the app's own llama-server, which prefills a trailing
/// assistant message (the reply continues it) instead of starting a new turn.
pub fn prefills_assistant(config: &Config) -> bool {
    config.direct_backend
        && !config.gguf_model.as_os_str().is_empty()
        && config.model == super::llama::model_alias(config)
}

/// The last model whose chat template refused an assistant prefill.
static NO_PREFILL: Mutex<Option<String>> = Mutex::new(None);

/// False once this model refused a prefill, so later requests skip the failing first try.
pub fn prefill_ok(config: &Config) -> bool {
    NO_PREFILL.lock().unwrap().as_deref() != Some(config.model.as_str())
}

fn request_body(config: &Config, messages: &[Value], definitions: Option<&Value>) -> Value {
    let mut body = if config.direct_backend {
        // cache_prompt and a pinned slot let llama.cpp reuse the KV cache for the unchanged
        // prefix; return_progress streams prompt-processing chunks, so Stop works during it.
        json!({"model":config.model,"messages":openai_messages(messages),"stream":true,
            "stream_options":{"include_usage":true},"max_tokens":config.response_tokens,
            "temperature":config.temperature,"top_p":config.top_p,
            "repeat_penalty":config.repeat_penalty,"cache_prompt":true,"id_slot":0,
            "return_progress":true})
    } else {
        json!({"model":config.model,"messages":ollama_messages(messages),"stream":true,"options":{
            "num_ctx":config.context_tokens,"num_predict":config.response_tokens,
            "temperature":config.temperature,"top_p":config.top_p,
            "repeat_penalty":config.repeat_penalty}})
    };
    if let Some(definitions) = definitions {
        body["tools"] = definitions.clone();
    }
    body
}

/// Streams one reply. `definitions` is the tool list offered (built once per run by the engine).
/// A trailing assistant message is a prefill; when the server refuses prefill (for example a
/// template with thinking enabled), the request is sent again without it.
pub fn generate(
    config: &Config,
    messages: &[Value],
    definitions: Option<&Value>,
    tx: &Sender<Event>,
    cancel: &AtomicBool,
) -> Result<Reply> {
    let url = if config.direct_backend {
        format!("{}/v1/chat/completions", direct_base(config)?)
    } else {
        format!("{}/api/chat", engine::OLLAMA)
    };
    let send = |messages: &[Value]| -> Result<reqwest::blocking::Response> {
        CLIENT
            .post(&url)
            .json(&request_body(config, messages, definitions))
            .send()
            .context("Model server is not responding. Check the provider in Settings.")
    };
    let mut response = send(messages)?;
    if !response.status().is_success() {
        let text: String = response.text()?.chars().take(2000).collect();
        let prefilled = config.direct_backend
            && messages.len() > 1
            && messages.last().is_some_and(|m| m["role"] == "assistant");
        ensure!(
            prefilled && text.to_ascii_lowercase().contains("prefill"),
            "Model server: {text}"
        );
        *NO_PREFILL.lock().unwrap() = Some(config.model.clone());
        response = send(&messages[..messages.len() - 1])?;
        if !response.status().is_success() {
            anyhow::bail!(
                "Model server: {}",
                response.text()?.chars().take(2000).collect::<String>()
            );
        }
    }
    let streamed = read_stream(BufReader::new(response), config.direct_backend, tx, cancel)?;
    if let Some(note) = streamed.speed {
        let _ = tx.send(Event::Activity(note));
    }
    let (mut content, mut calls) = (streamed.content, streamed.calls);
    for call in &mut calls {
        // Bad JSON becomes a marker that the engine answers with a retryable tool error.
        let args = &call["function"]["arguments"];
        if !args.is_object() {
            let fixed = match args {
                Value::String(raw) => parse_arguments(raw),
                Value::Null => json!({}),
                other => json!({"_invalid_json": other.to_string()}),
            };
            call["function"]["arguments"] = fixed;
        }
    }
    if calls.is_empty()
        && let Some(definitions) = definitions
    {
        let (rest, found) = text_tool_calls(&content, definitions);
        if !found.is_empty() {
            let _ = tx.send(Event::Activity(
                "Read a tool call the model wrote as text".into(),
            ));
            content = rest;
            calls = found;
        }
    }
    ensure!(calls.len() <= 16, "Too many tool calls in one response");
    for (i, call) in calls.iter_mut().enumerate() {
        if !call["id"].is_string() {
            call["id"] = json!(format!("call_{}_{}", messages.len(), i));
        }
    }
    Ok(Reply {
        content,
        calls,
        prompt_tokens: streamed.prompt_tokens,
    })
}

/// What one streamed response carried, before tool-call arguments are parsed.
struct Streamed {
    content: String,
    calls: Vec<Value>,
    prompt_tokens: Option<usize>,
    speed: Option<String>,
}

/// A streamed tool call, built from fragments with plain string appends (no JSON per token).
#[derive(Default)]
struct PartialCall {
    id: Option<String>,
    name: String,
    arguments: String,
}

/// Shortest gap between two prompt-progress Activity lines.
const PROGRESS_EVERY: Duration = Duration::from_millis(1500);

/// Reads an SSE (llama.cpp) or NDJSON (Ollama) chat stream, sending tokens as they arrive.
/// The cancel flag is checked on every line, llama.cpp's prompt-progress chunks included.
fn read_stream(
    reader: impl BufRead,
    direct: bool,
    tx: &Sender<Event>,
    cancel: &AtomicBool,
) -> Result<Streamed> {
    let (mut content, mut calls, mut partial) =
        (String::new(), vec![], BTreeMap::<usize, PartialCall>::new());
    let (mut done, mut speed, mut prompt_tokens) = (false, None, None);
    let mut last_progress: Option<Instant> = None;
    for line in reader.lines() {
        ensure!(!cancel.load(Ordering::Relaxed), "Stopped");
        let line = line?;
        let data = if direct {
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            if data.trim() == "[DONE]" {
                done = true;
                break;
            }
            data.trim()
        } else {
            line.trim()
        };
        if data.is_empty() {
            continue;
        }
        let v: Value = serde_json::from_str(data)?;
        if !v["error"].is_null() {
            anyhow::bail!("Model server error: {}", v["error"]);
        }
        if direct {
            let progress = &v["prompt_progress"];
            if progress.is_object() {
                let n = |k: &str| progress[k].as_u64().unwrap_or(0);
                let (total, cache, processed) = (n("total"), n("cache"), n("processed"));
                if total > 0
                    && processed < total
                    && last_progress.is_none_or(|at| at.elapsed() >= PROGRESS_EVERY)
                {
                    last_progress = Some(Instant::now());
                    let mut note = format!("Reading prompt {}%", processed * 100 / total);
                    if cache > 0 {
                        note.push_str(&format!(" ({cache} of {total} tokens from cache)"));
                    }
                    let _ = tx.send(Event::Activity(note));
                }
            }
            let t = &v["timings"];
            if t.is_object() {
                let n = |k: &str| t[k].as_f64().unwrap_or(0.0);
                speed = speed_note(
                    n("predicted_n"),
                    n("predicted_ms"),
                    n("prompt_n"),
                    n("prompt_ms"),
                    t["cache_n"].as_f64(),
                )
                .or(speed);
            }
            if let Some(p) = v["usage"]["prompt_tokens"].as_u64() {
                prompt_tokens = Some(p as usize);
            }
            let delta = &v["choices"][0]["delta"];
            if let Some(token) = delta["content"].as_str() {
                content.push_str(token);
                let _ = tx.send(Event::Token(token.into()));
            }
            if let Some(fragments) = delta["tool_calls"].as_array() {
                for fragment in fragments {
                    let index = fragment["index"].as_u64().unwrap_or(0) as usize;
                    ensure!(index < 16, "Too many tool calls in one response");
                    let call = partial.entry(index).or_default();
                    if let Some(id) = fragment["id"].as_str() {
                        call.id = Some(id.to_owned());
                    }
                    if let Some(text) = fragment["function"]["name"].as_str() {
                        call.name.push_str(text);
                    }
                    if let Some(text) = fragment["function"]["arguments"].as_str() {
                        call.arguments.push_str(text);
                    }
                }
            }
            if !v["choices"][0]["finish_reason"].is_null() {
                done = true;
            }
        } else {
            if let Some(token) = v["message"]["content"].as_str() {
                content.push_str(token);
                let _ = tx.send(Event::Token(token.into()));
            }
            if let Some(tools) = v["message"]["tool_calls"].as_array() {
                calls.extend(tools.iter().cloned());
            }
            if v["done"] == true {
                // Ollama reports nanoseconds and leaves cached tokens out of prompt_eval_count.
                let n = |k: &str| v[k].as_f64().unwrap_or(0.0);
                speed = speed_note(
                    n("eval_count"),
                    n("eval_duration") / 1e6,
                    n("prompt_eval_count"),
                    n("prompt_eval_duration") / 1e6,
                    None,
                );
                done = true;
                break;
            }
        }
    }
    ensure!(done, "Model stream ended before completion");
    calls.extend(partial.into_values().map(|c| {
        let mut call = json!({"function":{"name":c.name,"arguments":c.arguments}});
        if let Some(id) = c.id {
            call["id"] = json!(id);
        }
        call
    }));
    Ok(Streamed {
        content,
        calls,
        prompt_tokens,
        speed,
    })
}

fn speed_note(
    predicted: f64,
    predicted_ms: f64,
    prompt: f64,
    prompt_ms: f64,
    cached: Option<f64>,
) -> Option<String> {
    if predicted <= 0.0 || predicted_ms <= 0.0 {
        return None;
    }
    let mut note = format!(
        "Speed: {:.1} tokens/s ({predicted:.0} tokens in {:.1} s)",
        predicted * 1000.0 / predicted_ms,
        predicted_ms / 1000.0
    );
    if prompt > 0.0 {
        note.push_str(&format!(
            " · prompt {prompt:.0} tokens in {:.1} s",
            prompt_ms / 1000.0
        ));
    }
    if let Some(cached) = cached.filter(|c| *c > 0.0) {
        note.push_str(&format!(" · {cached:.0} reused from cache"));
    }
    Some(note)
}

fn parse_arguments(raw: &str) -> Value {
    let raw = raw.trim();
    if raw.is_empty() {
        return json!({});
    }
    match serde_json::from_str::<Value>(raw) {
        Ok(v) if v.is_object() => v,
        // Some models double-encode the arguments object.
        Ok(Value::String(inner)) if inner.trim() != raw => parse_arguments(&inner),
        _ => json!({"_invalid_json": raw.chars().take(2000).collect::<String>()}),
    }
}

/// Recovers tool calls a model wrote into its reply text, inside `<tool_call>` tags or as a
/// bare JSON object or array. Returns the remaining text and the calls found.
fn text_tool_calls(content: &str, definitions: &Value) -> (String, Vec<Value>) {
    const OPEN: &str = "<tool_call>";
    const CLOSE: &str = "</tool_call>";
    if content.contains(OPEN) {
        let (mut rest, mut found, mut tail) = (String::new(), vec![], content);
        while let Some(start) = tail.find(OPEN) {
            rest.push_str(&tail[..start]);
            let after = &tail[start + OPEN.len()..];
            let (body, next) = match after.find(CLOSE) {
                Some(end) => (&after[..end], &after[end + CLOSE.len()..]),
                None => (after, ""),
            };
            found.extend(json_calls(body, None));
            tail = next;
        }
        rest.push_str(tail);
        return (rest.trim().to_owned(), found);
    }
    // Bare JSON only counts when every entry names an offered tool, so real answers survive.
    let known: Vec<&str> = definitions
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|d| d["function"]["name"].as_str())
        .collect();
    let found = json_calls(content, Some(&known));
    if found.is_empty() {
        (content.to_owned(), found)
    } else {
        (String::new(), found)
    }
}

fn json_calls(text: &str, known: Option<&[&str]>) -> Vec<Value> {
    let text = text.trim();
    let text = text
        .strip_prefix("```json")
        .or_else(|| text.strip_prefix("```"))
        .map(|t| t.trim_end().trim_end_matches("```"))
        .unwrap_or(text)
        .trim();
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return vec![];
    };
    let items = match value {
        Value::Array(items) => items,
        other => vec![other],
    };
    let mut calls = vec![];
    for item in &items {
        let f = if item["function"].is_object() {
            &item["function"]
        } else {
            item
        };
        let Some(name) = f["name"].as_str() else {
            return vec![];
        };
        let args = if f["arguments"].is_null() {
            &f["parameters"]
        } else {
            &f["arguments"]
        };
        if let Some(known) = known
            && (args.is_null() || !known.contains(&name))
        {
            return vec![];
        }
        let args = match args {
            Value::String(raw) => parse_arguments(raw),
            Value::Null => json!({}),
            Value::Object(_) => args.clone(),
            other => json!({"_invalid_json": other.to_string()}),
        };
        calls.push(json!({"function":{"name":name,"arguments":args}}));
    }
    calls
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::tools;
    #[test]
    fn direct_provider_rejects_external_hosts() {
        let mut c = Config {
            direct_url: "http://192.168.1.5:8080".into(),
            ..Default::default()
        };
        assert!(direct_base(&c).is_err());
        c.direct_url = "http://localhost:8080/v1/".into();
        assert_eq!(direct_base(&c).unwrap(), "http://localhost:8080");
        c.direct_url = "http://[::1]:8080".into();
        assert!(direct_base(&c).is_ok());
        c.direct_url = "http://user:pw@127.0.0.1:8080".into();
        assert!(direct_base(&c).is_err());
    }
    #[test]
    fn lan_endpoints_need_opt_in_trust_and_https_when_public() {
        let mut c = Config {
            direct_url: "http://192.168.1.5:8080".into(),
            trusted_endpoints: vec!["192.168.1.5:8080".into()],
            ..Default::default()
        };
        assert!(direct_base(&c).is_err(), "allow_lan is off");
        c.allow_lan = true;
        assert_eq!(direct_base(&c).unwrap(), "http://192.168.1.5:8080");
        c.direct_url = "http://192.168.1.5:9000".into();
        assert!(direct_base(&c).is_err(), "port not trusted");
        c.trusted_endpoints = vec!["http://192.168.1.5".into(), "gpu.example.com".into()];
        assert!(
            direct_base(&c).is_ok(),
            "entry without a port trusts any port"
        );
        c.direct_url = "http://10.0.0.2:8080".into();
        assert!(direct_base(&c).is_err(), "host not trusted");
        c.direct_url = "http://gpu.example.com:8080".into();
        assert!(direct_base(&c).is_err(), "public host needs https");
        c.direct_url = "https://gpu.example.com:8080".into();
        assert!(direct_base(&c).is_ok());
        assert!(
            private_host("100.101.1.2") && private_host("fd00::1") && private_host("mac.local")
        );
        assert!(!private_host("8.8.8.8") && !private_host("example.com"));
    }
    #[test]
    fn images_become_ollama_image_lists() {
        let m = json!({"role":"user","content":[{"type":"text","text":"what is this?"},
            {"type":"image_url","image_url":{"url":"data:image/png;base64,QUJD"}}]});
        let out = ollama_messages(&[m, json!({"role":"assistant","content":"x"})]);
        assert_eq!(
            out[0],
            json!({"role":"user","content":"what is this?","images":["QUJD"]})
        );
        assert_eq!(out[1]["content"], "x");
    }
    #[test]
    fn serializes_tool_arguments_for_compatible_servers() {
        let result = openai_messages(&[
            json!({"role":"assistant","tool_calls":[{"id":"call_1","function":{"name":"read_file","arguments":{"path":"main.rs"}}}]}),
        ]);
        assert_eq!(
            result[0]["tool_calls"][0]["function"]["arguments"],
            "{\"path\":\"main.rs\"}"
        );
    }
    #[test]
    fn malformed_arguments_become_a_marker() {
        assert_eq!(parse_arguments(""), json!({}));
        assert_eq!(parse_arguments(r#"{"path":"a"}"#), json!({"path":"a"}));
        assert_eq!(
            parse_arguments(r#""{\"path\":\"a\"}""#),
            json!({"path":"a"})
        );
        assert!(
            parse_arguments(r#"{"path":"#)
                .get("_invalid_json")
                .is_some()
        );
    }
    #[test]
    fn recovers_tool_calls_written_as_text() {
        let defs = tools::definitions(&tools::ToolSet {
            docs: false,
            web: false,
        });
        let (rest, calls) = text_tool_calls(
            "Let me look.\n<tool_call>\n{\"name\":\"read_file\",\"arguments\":{\"path\":\"a.rs\"}}\n</tool_call>",
            &defs,
        );
        assert_eq!(rest, "Let me look.");
        assert_eq!(calls[0]["function"]["name"], "read_file");
        assert_eq!(calls[0]["function"]["arguments"]["path"], "a.rs");
        let (rest, calls) = text_tool_calls(
            "```json\n{\"name\":\"list_files\",\"arguments\":{}}\n```",
            &defs,
        );
        assert!(rest.is_empty());
        assert_eq!(calls.len(), 1);
        // Plain JSON that does not name an offered tool stays an ordinary answer.
        let answer = r#"{"name":"Alice","arguments":[]}"#;
        let (rest, calls) = text_tool_calls(answer, &defs);
        assert!(calls.is_empty());
        assert_eq!(rest, answer);
        let web = r#"{"name":"web_search","arguments":{"query":"x"}}"#;
        assert!(text_tool_calls(web, &defs).1.is_empty());
    }
    #[test]
    fn formats_speed_from_timings() {
        assert_eq!(
            speed_note(100.0, 4000.0, 512.0, 1000.0, Some(256.0)).unwrap(),
            "Speed: 25.0 tokens/s (100 tokens in 4.0 s) · prompt 512 tokens in 1.0 s · 256 reused from cache"
        );
        assert!(speed_note(0.0, 0.0, 0.0, 0.0, None).is_none());
    }

    /// A llama.cpp SSE stream: prompt progress, `tokens` content tokens, then a tool call
    /// whose arguments arrive in `tokens` fragments, then usage and timings.
    fn sse_stream(tokens: usize) -> String {
        let mut s = String::new();
        let line = |s: &mut String, v: Value| s.push_str(&format!("data: {v}\n\n"));
        line(
            &mut s,
            json!({"choices":[],"prompt_progress":{"total":1000,"cache":400,"processed":500,"time_ms":10}}),
        );
        for i in 0..tokens {
            line(
                &mut s,
                json!({"choices":[{"index":0,"delta":{"content":format!("tok{i} ")}}]}),
            );
        }
        line(
            &mut s,
            json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"write_file","arguments":"{\"path\":\"a.rs\",\"content\":\""}}]}}]}),
        );
        for _ in 0..tokens {
            line(
                &mut s,
                json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"fn main() {}\\n"}}]}}]}),
            );
        }
        line(
            &mut s,
            json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"}"}}]},"finish_reason":"tool_calls"}],
                "usage":{"prompt_tokens":1000},
                "timings":{"predicted_n":4000,"predicted_ms":2000,"prompt_n":600,"prompt_ms":300,"cache_n":400}}),
        );
        s.push_str("data: [DONE]\n\n");
        s
    }

    #[test]
    fn reads_sse_stream_with_progress_and_fragmented_tool_calls() {
        let (tx, rx) = std::sync::mpsc::channel();
        let cancel = AtomicBool::new(false);
        let out = read_stream(sse_stream(3).as_bytes(), true, &tx, &cancel).unwrap();
        assert_eq!(out.content, "tok0 tok1 tok2 ");
        assert_eq!(out.prompt_tokens, Some(1000));
        assert_eq!(out.calls.len(), 1);
        assert_eq!(out.calls[0]["id"], "c1");
        assert_eq!(out.calls[0]["function"]["name"], "write_file");
        let args = parse_arguments(out.calls[0]["function"]["arguments"].as_str().unwrap());
        assert_eq!(args["content"], "fn main() {}\n".repeat(3));
        assert!(out.speed.unwrap().contains("400 reused from cache"));
        let activity: Vec<String> = rx
            .try_iter()
            .filter_map(|e| match e {
                Event::Activity(a) => Some(a),
                _ => None,
            })
            .collect();
        assert_eq!(
            activity,
            ["Reading prompt 50% (400 of 1000 tokens from cache)"]
        );
        // Stop is honoured on the first line, before any token.
        cancel.store(true, Ordering::Relaxed);
        let stopped = read_stream(sse_stream(3).as_bytes(), true, &tx, &cancel);
        assert!(stopped.err().unwrap().to_string().contains("Stopped"));
        // A stream cut off before finish_reason or [DONE] is an error.
        cancel.store(false, Ordering::Relaxed);
        let cut = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"x\"}}]}\n";
        assert!(read_stream(cut.as_bytes(), true, &tx, &cancel).is_err());
    }

    #[test]
    fn perf_sse_stream_parsing() {
        let stream = sse_stream(2000);
        let (tx, rx) = std::sync::mpsc::channel();
        let cancel = AtomicBool::new(false);
        let start = Instant::now();
        let out = read_stream(stream.as_bytes(), true, &tx, &cancel).unwrap();
        let elapsed = start.elapsed();
        drop(rx);
        eprintln!(
            "perf_sse_stream_parsing: 2000 tokens + 2000 tool-call fragments ({} KB) in {:.1} ms",
            stream.len() / 1024,
            elapsed.as_secs_f64() * 1000.0
        );
        assert_eq!(out.content.matches("tok").count(), 2000);
        assert!(
            out.calls[0]["function"]["arguments"]
                .as_str()
                .unwrap()
                .len()
                > 2000 * 14
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "SSE parsing took {elapsed:?}"
        );
    }
}
