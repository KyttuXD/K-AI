use super::{
    config::Config,
    engine::{self, Event},
    tools,
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::Sender,
    },
    time::Duration,
};

pub fn direct_base(config: &Config) -> Result<String> {
    let url = reqwest::Url::parse(config.direct_url.trim())?;
    let loopback = url.host_str().is_some_and(|h| {
        h == "localhost"
            || h == "[::1]"
            || h == "::1"
            || h.parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    ensure!(
        loopback
            && matches!(url.scheme(), "http" | "https")
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none(),
        "Direct endpoints must use localhost or a loopback IP, without credentials or query strings"
    );
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
fn client() -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(300))
        .build()?)
}
pub fn models(config: &Config) -> Result<Vec<String>> {
    if !config.direct_backend {
        return engine::models();
    }
    let value: Value = reqwest::blocking::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
        .build()?
        .get(format!("{}/v1/models", direct_base(config)?))
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

pub fn generate(
    config: &Config,
    messages: &[Value],
    tool_set: Option<&tools::ToolSet>,
    tx: &Sender<Event>,
    cancel: &AtomicBool,
) -> Result<Reply> {
    let (url, mut body) = if config.direct_backend {
        (
            format!("{}/v1/chat/completions", direct_base(config)?),
            // cache_prompt and a pinned slot let llama.cpp reuse the KV cache for the unchanged prefix.
            json!({"model":config.model,"messages":openai_messages(messages),"stream":true,
                "stream_options":{"include_usage":true},"max_tokens":config.response_tokens,
                "temperature":config.temperature,"top_p":config.top_p,
                "repeat_penalty":config.repeat_penalty,"cache_prompt":true,"id_slot":0}),
        )
    } else {
        (
            format!("{}/api/chat", engine::OLLAMA),
            json!({"model":config.model,"messages":messages,"stream":true,"options":{
                "num_ctx":config.context_tokens,"num_predict":config.response_tokens,
                "temperature":config.temperature,"top_p":config.top_p,
                "repeat_penalty":config.repeat_penalty}}),
        )
    };
    let definitions = tool_set.map(tools::definitions);
    if let Some(definitions) = &definitions {
        body["tools"] = definitions.clone();
    }
    let response = client()?
        .post(url)
        .json(&body)
        .send()
        .context("Model server is not responding. Check the provider in Settings.")?;
    if !response.status().is_success() {
        anyhow::bail!(
            "Model server: {}",
            response.text()?.chars().take(2000).collect::<String>()
        );
    }
    let (mut content, mut calls, mut partial) =
        (String::new(), vec![], BTreeMap::<usize, Value>::new());
    let (mut done, mut speed, mut prompt_tokens) = (false, None, None);
    for line in BufReader::new(response).lines() {
        ensure!(!cancel.load(Ordering::Relaxed), "Stopped");
        let line = line?;
        let data = if config.direct_backend {
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
        if config.direct_backend {
            let t = &v["timings"];
            if t.is_object() {
                let n = |k: &str| t[k].as_f64().unwrap_or(0.0);
                speed = speed_note(
                    n("predicted_n"),
                    n("predicted_ms"),
                    n("prompt_n"),
                    n("prompt_ms"),
                    t["cache_n"].as_f64(),
                );
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
                    let call = partial
                        .entry(index)
                        .or_insert_with(|| json!({"function":{"name":"","arguments":""}}));
                    if let Some(id) = fragment["id"].as_str() {
                        call["id"] = json!(id);
                    }
                    for key in ["name", "arguments"] {
                        if let Some(text) = fragment["function"][key].as_str() {
                            call["function"][key] = json!(format!(
                                "{}{text}",
                                call["function"][key].as_str().unwrap_or("")
                            ));
                        }
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
    if let Some(note) = speed {
        let _ = tx.send(Event::Activity(note));
    }
    calls.extend(partial.into_values());
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
        && let Some(definitions) = &definitions
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
        prompt_tokens,
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
    #[test]
    fn direct_provider_rejects_external_hosts() {
        let mut c = Config {
            direct_url: "http://192.168.1.5:8080".into(),
            ..Default::default()
        };
        assert!(direct_base(&c).is_err());
        c.direct_url = "http://localhost:8080/v1/".into();
        assert_eq!(direct_base(&c).unwrap(), "http://localhost:8080");
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
}
