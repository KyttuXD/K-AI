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

pub fn generate(
    config: &Config,
    messages: &[Value],
    coding: bool,
    tx: &Sender<Event>,
    cancel: &AtomicBool,
) -> Result<(String, Vec<Value>)> {
    let (url, mut body) = if config.direct_backend {
        (
            format!("{}/v1/chat/completions", direct_base(config)?),
            json!({"model":config.model,"messages":openai_messages(messages),"stream":true,"max_tokens":config.response_tokens,"temperature":0.2}),
        )
    } else {
        (
            format!("{}/api/chat", engine::OLLAMA),
            json!({"model":config.model,"messages":messages,"stream":true,"options":{"num_ctx":config.context_tokens,"num_predict":config.response_tokens,"temperature":0.2}}),
        )
    };
    if coding {
        body["tools"] = tools::definitions();
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
    let mut done = false;
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
                done = true;
                break;
            }
        }
    }
    ensure!(done, "Model stream ended before completion");
    for (_, mut call) in partial {
        call["function"]["arguments"] =
            serde_json::from_str(call["function"]["arguments"].as_str().unwrap_or("{}"))?;
        calls.push(call);
    }
    ensure!(calls.len() <= 16, "Too many tool calls in one response");
    for (i, call) in calls.iter_mut().enumerate() {
        if !call["id"].is_string() {
            call["id"] = json!(format!("call_{}_{}", messages.len(), i));
        }
    }
    Ok((content, calls))
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
}
