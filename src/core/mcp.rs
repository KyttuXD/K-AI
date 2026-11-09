//! Model Context Protocol client over stdio (JSON-RPC 2.0, one JSON message per line).
//! Server processes are started on first use, cached per server name and stopped when the
//! cache entry is dropped (config change, failure or [`shutdown_all`]).

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Arc, LazyLock, Mutex,
        mpsc::{self, Receiver, RecvTimeoutError},
    },
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct McpServer {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub enabled: bool,
}

const PROTOCOL: &str = "2025-06-18";
const START_TIMEOUT: Duration = Duration::from_secs(30);
const CALL_TIMEOUT: Duration = Duration::from_secs(120);
/// Characters of tool output handed to the model.
const OUTPUT_BUDGET: usize = 16_000;

type Entry = (McpServer, Arc<Mutex<Conn>>);
static CONNS: LazyLock<Mutex<HashMap<String, Entry>>> = LazyLock::new(Default::default);

struct Conn {
    name: String,
    child: Child,
    /// The server and whatever it started (npx, uvx and docker wrap the real server).
    tree: super::terminal::Tree,
    stdin: Option<ChildStdin>,
    rx: Receiver<Value>,
    next: u64,
    tools: Option<Vec<Value>>,
    stderr: Arc<Mutex<String>>,
    /// Set on a timeout, exit or broken pipe; the cache then drops this process.
    broken: bool,
}

impl Conn {
    fn start(spec: &McpServer) -> Result<Self> {
        let command = spec.command.trim();
        ensure!(
            !command.is_empty(),
            "MCP server {} has no command",
            spec.name
        );
        let program = super::tools::resolve_program(Path::new("."), command)
            .unwrap_or_else(|| command.into());
        let mut cmd = Command::new(&program);
        cmd.args(&spec.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x08000000);
        }
        super::terminal::Tree::prepare(&mut cmd);
        let mut child = cmd
            .spawn()
            .with_context(|| format!("Could not start MCP server {} ({command})", spec.name))?;
        let tree = super::terminal::Tree::attach(&child);
        let stdout = child.stdout.take().context("MCP server has no stdout")?;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                // Servers sometimes log plain text to stdout; only JSON lines are messages.
                if let Ok(v) = serde_json::from_str::<Value>(line.trim())
                    && tx.send(v).is_err()
                {
                    break;
                }
            }
        });
        let stderr = Arc::new(Mutex::new(String::new()));
        if let Some(err) = child.stderr.take() {
            let buf = stderr.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(err).lines().map_while(Result::ok) {
                    let mut b = buf.lock().unwrap_or_else(|e| e.into_inner());
                    b.push_str(&line);
                    b.push('\n');
                    keep_tail(&mut b, 2000);
                }
            });
        }
        let mut conn = Self {
            name: spec.name.clone(),
            stdin: child.stdin.take(),
            child,
            tree,
            rx,
            next: 0,
            tools: None,
            stderr,
            broken: false,
        };
        conn.request(
            "initialize",
            json!({"protocolVersion":PROTOCOL,"capabilities":{},
                "clientInfo":{"name":"kyttu-ai","version":env!("CARGO_PKG_VERSION")}}),
            START_TIMEOUT,
        )?;
        conn.send(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}))?;
        Ok(conn)
    }

    fn send(&mut self, message: &Value) -> Result<()> {
        let mut line = serde_json::to_string(message)?;
        line.push('\n');
        let written = self
            .stdin
            .as_mut()
            .context("MCP server input is closed")
            .and_then(|s| Ok(s.write_all(line.as_bytes()).and_then(|_| s.flush())?));
        if written.is_err() {
            self.broken = true;
        }
        written.with_context(|| format!("MCP server {} stopped reading{}", self.name, self.log()))
    }

    fn log(&self) -> String {
        let text = self
            .stderr
            .lock()
            .map(|s| s.trim().to_owned())
            .unwrap_or_default();
        if text.is_empty() {
            String::new()
        } else {
            format!("\n{text}")
        }
    }

    fn request(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        self.next += 1;
        let id = self.next;
        self.send(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))?;
        let deadline = Instant::now() + timeout;
        loop {
            let mut msg = match self
                .rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                Ok(msg) => msg,
                Err(RecvTimeoutError::Timeout) => {
                    self.broken = true;
                    bail!(
                        "MCP server {} did not answer {method} within {} s",
                        self.name,
                        timeout.as_secs()
                    )
                }
                Err(RecvTimeoutError::Disconnected) => {
                    self.broken = true;
                    bail!("MCP server {} exited{}", self.name, self.log())
                }
            };
            let is_request = msg.get("method").is_some();
            if !is_request && msg["id"] == json!(id) {
                if let Some(err) = msg.get("error") {
                    bail!(
                        "MCP {method} failed: {}",
                        err["message"]
                            .as_str()
                            .map_or_else(|| err.to_string(), str::to_owned)
                    );
                }
                return Ok(msg.get_mut("result").map(Value::take).unwrap_or_default());
            }
            if is_request && !msg["id"].is_null() {
                // Answer server-to-client requests (ping, sampling, roots) so the server never waits.
                let reply = if msg["method"] == "ping" {
                    json!({"jsonrpc":"2.0","id":msg["id"],"result":{}})
                } else {
                    json!({"jsonrpc":"2.0","id":msg["id"],"error":{"code":-32601,"message":"Not supported by this client"}})
                };
                self.send(&reply)?;
            }
        }
    }

    fn list(&mut self) -> Result<Vec<Value>> {
        if let Some(tools) = &self.tools {
            return Ok(tools.clone());
        }
        let (mut tools, mut cursor) = (vec![], Value::Null);
        for _ in 0..20 {
            let params = if cursor.is_string() {
                json!({"cursor":cursor})
            } else {
                json!({})
            };
            let mut page = self.request("tools/list", params, START_TIMEOUT)?;
            if let Some(Value::Array(list)) = page.get_mut("tools").map(Value::take) {
                tools.extend(list);
            }
            cursor = page
                .get_mut("nextCursor")
                .map(Value::take)
                .unwrap_or_default();
            if !cursor.is_string() {
                break;
            }
        }
        self.tools = Some(tools.clone());
        Ok(tools)
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        // Closing stdin asks a stdio server (and the children it started) to exit cleanly.
        drop(self.stdin.take());
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline && matches!(self.child.try_wait(), Ok(None)) {
            std::thread::sleep(Duration::from_millis(25));
        }
        // Also when the direct child exited: a wrapper may leave the real server running.
        self.tree.kill();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn keep_tail(text: &mut String, max: usize) {
    if text.len() > max * 2 {
        let mut cut = text.len() - max;
        while !text.is_char_boundary(cut) {
            cut += 1;
        }
        text.drain(..cut);
    }
}

/// Runs `f` on the cached process for `server`, starting it (or restarting it after a settings
/// change) as needed. A process that timed out or exited is dropped so the next call restarts it.
fn with_conn<T>(server: &McpServer, f: impl FnOnce(&mut Conn) -> Result<T>) -> Result<T> {
    let cached = |map: &HashMap<String, Entry>| {
        map.get(&server.name)
            .filter(|(spec, _)| spec == server)
            .map(|(_, conn)| conn.clone())
    };
    let found = cached(&CONNS.lock().unwrap_or_else(|e| e.into_inner()));
    let conn = match found {
        Some(conn) => conn,
        None => {
            // Started without the lock, so a slow server never blocks the others.
            let fresh = Arc::new(Mutex::new(Conn::start(server)?));
            let (conn, unused) = {
                let mut map = CONNS.lock().unwrap_or_else(|e| e.into_inner());
                match cached(&map) {
                    // Another thread won the race; its process is kept.
                    Some(conn) => (conn, Some(fresh)),
                    None => {
                        // The replaced, outdated entry is stopped after the lock is released.
                        let old = map.insert(server.name.clone(), (server.clone(), fresh.clone()));
                        (fresh, old.map(|(_, c)| c))
                    }
                }
            };
            drop(unused);
            conn
        }
    };
    let (result, broken) = {
        let mut guard = conn.lock().unwrap_or_else(|e| e.into_inner());
        let result = f(&mut guard);
        (result, guard.broken)
    };
    if broken {
        let removed = {
            let mut map = CONNS.lock().unwrap_or_else(|e| e.into_inner());
            let same = map
                .get(&server.name)
                .is_some_and(|(_, c)| Arc::ptr_eq(c, &conn));
            if same { map.remove(&server.name) } else { None }
        };
        drop(removed);
    }
    result
}

/// The server's tools as MCP tool objects (`name`, `description`, `inputSchema`), cached.
pub fn list_tools(server: &McpServer) -> Result<Vec<Value>> {
    with_conn(server, Conn::list)
}

pub fn call_tool(server: &McpServer, tool: &str, args: &Value) -> Result<String> {
    let args = if args.is_object() {
        args.clone()
    } else {
        json!({})
    };
    let result = with_conn(server, |c| {
        c.request(
            "tools/call",
            json!({"name":tool,"arguments":args}),
            CALL_TIMEOUT,
        )
    })?;
    let text = render_result(&result);
    if result["isError"] == true {
        bail!("{text}");
    }
    Ok(text)
}

/// Stops every MCP server process. Call on app exit.
pub fn shutdown_all() {
    let entries: Vec<Entry> = CONNS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .drain()
        .map(|(_, e)| e)
        .collect();
    drop(entries);
}

/// The function name offered to the model: `mcp__<server>__<tool>`, limited to `[A-Za-z0-9_-]`.
pub fn function_name(server: &str, tool: &str) -> String {
    let clean = |s: &str| -> String {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    };
    format!("mcp__{}__{}", clean(server.trim()), clean(tool))
}

/// Text of a tools/call result; non-text content is noted, long output is cut with a note.
fn render_result(result: &Value) -> String {
    let mut parts: Vec<String> = vec![];
    for c in result["content"].as_array().into_iter().flatten() {
        match c["type"].as_str() {
            Some("text") => parts.push(c["text"].as_str().unwrap_or("").to_owned()),
            Some("resource") => parts.push(c["resource"]["text"].as_str().map_or_else(
                || format!("[resource {}]", c["resource"]["uri"].as_str().unwrap_or("")),
                str::to_owned,
            )),
            Some(kind) => parts.push(format!("[{kind} content omitted]")),
            None => {}
        }
    }
    if parts.is_empty() && !result["structuredContent"].is_null() {
        parts.push(result["structuredContent"].to_string());
    }
    let text = parts.join("\n");
    if text.chars().count() <= OUTPUT_BUDGET {
        return if text.is_empty() {
            "(no output)".into()
        } else {
            text
        };
    }
    let total = text.chars().count();
    let mut cut: String = text.chars().take(OUTPUT_BUDGET).collect();
    cut.push_str(&format!(
        "\n[truncated: {OUTPUT_BUDGET} of {total} characters shown]"
    ));
    cut
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_safe_function_names() {
        assert_eq!(
            function_name(" My Files ", "read.file"),
            "mcp__My_Files__read_file"
        );
        assert_eq!(function_name("git", "log-1"), "mcp__git__log-1");
    }

    #[test]
    fn renders_tool_results() {
        let r = json!({"content":[{"type":"text","text":"a"},{"type":"image","data":"x"},
            {"type":"resource","resource":{"uri":"file:///x","text":"b"}}]});
        assert_eq!(render_result(&r), "a\n[image content omitted]\nb");
        assert_eq!(render_result(&json!({"content":[]})), "(no output)");
        assert_eq!(
            render_result(&json!({"structuredContent":{"n":1}})),
            "{\"n\":1}"
        );
        let long = render_result(&json!({"content":[{"type":"text","text":"x".repeat(20_000)}]}));
        assert!(long.ends_with("[truncated: 16000 of 20000 characters shown]"));
    }

    #[test]
    fn stderr_keeps_the_tail() {
        let mut s = "é".repeat(3000);
        keep_tail(&mut s, 1000);
        assert!(s.len() <= 1001 && s.chars().all(|c| c == 'é'));
    }

    #[test]
    fn missing_command_is_an_error() {
        let server = McpServer {
            name: "none".into(),
            ..Default::default()
        };
        assert!(list_tools(&server).is_err());
    }
}
