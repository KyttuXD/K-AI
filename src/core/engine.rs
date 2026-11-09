use super::{
    mcp::{self, McpServer},
    personas::{LoreEntry, Persona},
    prompts,
};
use crate::{
    config::Config,
    docs::{Library, Source},
    tools,
};
use anyhow::{Context, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    ops::Range,
    path::Path,
    sync::{
        Arc, LazyLock, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Sender},
    },
    time::Duration,
};

pub const OLLAMA: &str = "http://127.0.0.1:11434";

pub struct Approval {
    pub title: String,
    pub detail: String,
    pub reply: Sender<bool>,
}
pub enum Event {
    Token(String),
    Sources(Vec<Source>),
    Activity(String),
    Approval(Approval),
    Finished(Result<Vec<Value>, String>),
}

fn ollama_tags() -> Result<Value> {
    Ok(super::provider::http()
        .get(format!("{OLLAMA}/api/tags"))
        .timeout(Duration::from_secs(4))
        .send()?
        .error_for_status()?
        .json()?)
}

pub fn models() -> Result<Vec<String>> {
    let v = ollama_tags()?;
    Ok(v["models"]
        .as_array()
        .context("Invalid model list")?
        .iter()
        .filter(|m| !m["remote_host"].is_string() && !m["remote_model"].is_string())
        .filter(|m| {
            !m["capabilities"]
                .as_array()
                .is_some_and(|c| !c.iter().any(|v| v == "completion"))
        })
        .filter_map(|m| m["name"].as_str())
        .filter(|n| !n.contains("embed") && !n.ends_with("cloud"))
        .map(str::to_owned)
        .collect())
}

pub fn embedding_models() -> Result<Vec<String>> {
    let v = ollama_tags()?;
    Ok(v["models"]
        .as_array()
        .context("Invalid model list")?
        .iter()
        .filter(|m| !m["remote_host"].is_string() && !m["remote_model"].is_string())
        .filter(|m| {
            m["capabilities"]
                .as_array()
                .is_some_and(|c| c.iter().any(|v| v == "embedding"))
                || m["name"].as_str().is_some_and(|n| n.contains("embed"))
        })
        .filter_map(|m| m["name"].as_str().map(str::to_owned))
        .collect())
}

/// Tools a caller supplies for one request (the Discord bot's music, moderation and game
/// tools). When a request carries a host, only its tools are offered (workspace and MCP tools
/// never are), also in chat mode, and calls run without the desktop approval dialog: the host
/// enforces its own permissions. `call` runs on the engine's worker thread.
pub trait ToolHost: Send + Sync {
    /// OpenAI-style function definitions: `{"type":"function","function":{"name":..}}`.
    fn definitions(&self) -> Vec<Value>;
    fn call(&self, name: &str, args: &Value) -> Result<String>;
}

pub struct Request {
    pub config: Config,
    pub library: Library,
    pub question: String,
    pub history: Vec<Value>,
    pub coding: bool,
    pub use_docs: bool,
    pub pack: String,
    pub cancel: Arc<AtomicBool>,
    pub persona: Persona,
    pub use_web: bool,
    pub use_embeddings: bool,
    /// Host-provided tools (Discord only); `None` everywhere else.
    pub tool_host: Option<Arc<dyn ToolHost>>,
}
pub fn spawn(mut request: Request, tx: Sender<Event>) {
    // The app's own llama-server runs with the low-end profile and per-model preset applied,
    // so budget and sampling must match it.
    if request.config.direct_backend && !request.config.gguf_model.as_os_str().is_empty() {
        request.config = super::llama::effective(&request.config);
    }
    std::thread::spawn(move || {
        let result = run(&request, &tx).map_err(|e| format!("{e:#}"));
        let _ = tx.send(Event::Finished(result));
    });
}

/// The user turn that resumes a run that stopped early (step limit, error or Stop).
pub const CONTINUE: &str = "Continue.";

/// A request that resumes `prev`. Set `prev.history` to the history the stopped run returned
/// first; no new documentation or web search is made for the "Continue." turn.
pub fn continue_request(prev: Request) -> Request {
    prev.cancel.store(false, Ordering::Relaxed);
    Request {
        question: CONTINUE.into(),
        ..prev
    }
}

/// Marks a trailing history entry holding images for the next question.
const ATTACHMENT: &str = "kyttu_attachment";

/// Attaches images (mime type, bytes) to the next question. Call it on `Request::history`
/// before spawning: the engine merges them into the question as OpenAI content parts
/// (llama.cpp) or an `images` list (Ollama). Calling it again adds more images.
pub fn attach_images(history: &mut Vec<Value>, images: &[(String, Vec<u8>)]) {
    let parts = images
        .iter()
        .filter(|(mime, bytes)| mime.starts_with("image/") && !bytes.is_empty())
        .map(|(mime, bytes)| {
            json!({"type":"image_url","image_url":{"url":format!("data:{mime};base64,{}", STANDARD.encode(bytes))}})
        });
    if history.last().is_some_and(|last| last[ATTACHMENT] == true) {
        if let Some(list) = history
            .last_mut()
            .and_then(|last| last["content"].as_array_mut())
        {
            list.extend(parts);
        }
        return;
    }
    let parts: Vec<Value> = parts.collect();
    if !parts.is_empty() {
        history.push(json!({"role":"user","content":parts,ATTACHMENT:true}));
    }
}

/// Splits off a pending image attachment: (earlier history, image parts).
fn split_attachment(history: &[Value]) -> (&[Value], Vec<Value>) {
    match history.split_last() {
        Some((last, prior)) if last[ATTACHMENT] == true => (
            prior,
            last["content"].as_array().cloned().unwrap_or_default(),
        ),
        _ => (history, vec![]),
    }
}

/// The text of a message's content, plain or OpenAI content parts.
fn text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn prepend_text(m: &mut Value, note: &str) {
    if let Some(text) = m["content"].as_str() {
        let text = format!("{note}{text}");
        m["content"] = json!(text);
    } else if let Some(parts) = m["content"].as_array_mut() {
        if let Some(p) = parts.iter_mut().find(|p| p["type"] == "text") {
            let text = format!("{note}{}", p["text"].as_str().unwrap_or(""));
            p["text"] = json!(text);
        } else {
            parts.insert(0, json!({"type":"text","text":note}));
        }
    }
}

/// Lorebook entries whose keys appear (case-insensitive, whole words) in `text`, capped.
fn lore_for(entries: &[LoreEntry], text: &str) -> String {
    crate::core::personas::matching_lore(entries, text)
}

/// The character's memory notes, one `- note` line each (empty when it has none). They do not
/// change between messages, so the system prompt stays byte-identical and cached.
fn memory_lines(persona: &Persona) -> String {
    persona
        .memory
        .iter()
        .map(|m| m.trim())
        .filter(|m| !m.is_empty())
        .map(|m| format!("- {m}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Lore matching the user's message, as a reference block for that user turn. It goes in the
/// turn (stored in history, so it stays the same bytes later) rather than the system prompt,
/// where a change would make the server re-read the whole conversation.
fn lore_block(persona: &Persona, question: &str) -> Option<String> {
    let lore = lore_for(&persona.lorebook, question);
    (!lore.is_empty()).then(|| prompts::lore_text(&lore))
}

/// Extra environment variable names allowed for the current workspace (item 61).
fn workspace_env(config: &Config) -> Vec<String> {
    let workspace = &config.workspace;
    let canonical = workspace.canonicalize().ok();
    config
        .workspace_env
        .iter()
        .find(|(key, _)| {
            let key = Path::new(key.trim());
            key == workspace.as_path()
                || canonical
                    .as_deref()
                    .is_some_and(|c| key.canonicalize().is_ok_and(|k| k == c))
        })
        .map(|(_, names)| {
            names
                .iter()
                .map(|n| n.trim().to_owned())
                .filter(|n| !n.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Turns kept in the history handed back to the caller (what is sent is budgeted separately).
const KEPT_TURNS: usize = 40;
/// Prompt characters an attached image is counted as (not its base64 length).
const IMAGE_CHARS: usize = 3000;
/// Tools offered from MCP servers in one request.
const MAX_MCP_TOOLS: usize = 64;
/// Consecutive failing tool calls after which a run stops instead of using up its steps.
const MAX_TOOL_FAILURES: usize = 6;

/// An MCP server tool offered to the model as `mcp__<server>__<tool>`.
struct McpTool {
    name: String,
    server: McpServer,
    tool: String,
    definition: Value,
}

/// Per-turn state: one running citation list, repeat detection, declined actions, the MCP
/// tools on offer and whether the workspace was checkpointed.
#[derive(Default)]
struct Turn {
    sources: Vec<Source>,
    reads: HashSet<String>,
    declined: HashSet<String>,
    /// Calls that failed this turn (by name and arguments) and why.
    failed: HashMap<String, String>,
    mcp: Vec<McpTool>,
    checkpointed: bool,
}
impl Turn {
    /// Numbers sources with one running sequence for the whole turn (docs, web and tool results),
    /// reusing the number of a source already cited, and reports only new ones to the UI so the
    /// Sources panel order matches the numbers.
    fn cite(&mut self, sources: Vec<Source>, tx: &Sender<Event>) -> String {
        let mut fresh = Vec::new();
        let text = sources
            .into_iter()
            .map(|s| {
                let known = self
                    .sources
                    .iter()
                    .position(|k| k.path == s.path && k.line == s.line);
                let n = match known {
                    Some(i) => i + 1,
                    None => {
                        self.sources.push(s.clone());
                        fresh.push(s.clone());
                        self.sources.len()
                    }
                };
                format!("[{n}] {} (text line {})\n{}", s.path, s.line, s.text)
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        if !fresh.is_empty() {
            let _ = tx.send(Event::Sources(fresh));
        }
        text
    }
}

/// Lists the tools of every enabled MCP server; a server that fails is noted and skipped.
fn mcp_tools(config: &Config, tx: &Sender<Event>) -> Vec<McpTool> {
    let mut out: Vec<McpTool> = vec![];
    for server in config.mcp_servers.iter().filter(|s| s.enabled) {
        let _ = tx.send(Event::Activity(format!(
            "MCP: connecting to {}…",
            server.name
        )));
        let list = match mcp::list_tools(server) {
            Ok(list) => list,
            Err(e) => {
                let _ = tx.send(Event::Activity(format!("MCP {}: {e:#}", server.name)));
                continue;
            }
        };
        for t in list {
            let Some(tool) = t["name"].as_str() else {
                continue;
            };
            let name = mcp::function_name(&server.name, tool);
            if out.len() >= MAX_MCP_TOOLS || name.len() > 64 || out.iter().any(|o| o.name == name) {
                continue;
            }
            let description: String = format!(
                "[MCP {}] {}",
                server.name,
                t["description"].as_str().unwrap_or("")
            )
            .chars()
            .take(1024)
            .collect();
            let parameters = if t["inputSchema"].is_object() {
                t["inputSchema"].clone()
            } else {
                json!({"type":"object","properties":{}})
            };
            out.push(McpTool {
                definition: json!({"type":"function","function":{"name":name,"description":description,"parameters":parameters}}),
                name,
                server: server.clone(),
                tool: tool.to_owned(),
            });
        }
    }
    out
}

fn approved(tx: &Sender<Event>, cancel: &AtomicBool, title: String, detail: String) -> bool {
    let (reply, rx) = mpsc::channel();
    if tx
        .send(Event::Approval(Approval {
            title,
            detail,
            reply,
        }))
        .is_err()
    {
        return false;
    }
    loop {
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(v) => return v,
            Err(mpsc::RecvTimeoutError::Disconnected) => return false,
            _ => {}
        }
    }
}

/// Shows a diff for review, then writes with a stale check against the reviewed text. The first
/// approved write of a turn checkpoints a git workspace first.
fn propose_write(
    req: &Request,
    tx: &Sender<Event>,
    turn: &mut Turn,
    title: String,
    path: &str,
    old: Option<&str>,
    new: &str,
) -> Result<String> {
    let diff = similar::TextDiff::from_lines(old.unwrap_or(""), new)
        .unified_diff()
        .header(path, path)
        .to_string();
    if !approved(tx, &req.cancel, title, diff) {
        return Ok("User declined this change. Do not retry it without a new request.".into());
    }
    if !turn.checkpointed {
        turn.checkpointed = true;
        let note = match tools::checkpoint_before_write(&req.config.workspace) {
            Ok(Some(id)) => Some(format!("Checkpoint {id} saved before the first change")),
            Ok(None) => None,
            Err(e) => Some(format!("Checkpoint failed: {e:#}")),
        };
        if let Some(note) = note {
            let _ = tx.send(Event::Activity(note));
        }
    }
    tools::write(&req.config.workspace, path, old, new)
}

fn execute(
    name: &str,
    args: &Value,
    req: &Request,
    tx: &Sender<Event>,
    turn: &mut Turn,
) -> Result<String> {
    if let Some(raw) = args.get("_invalid_json") {
        anyhow::bail!(
            "The arguments were not valid JSON: {raw}. Retry the call with a valid JSON object."
        );
    }
    ensure!(args.is_object(), "Arguments must be a JSON object");
    // A host replaces the workspace tools entirely, so no workspace or MCP call can run here.
    if let Some(host) = &req.tool_host {
        return host_call(host.as_ref(), name, args);
    }
    let arg = |name: &str| -> Result<&str> {
        args[name]
            .as_str()
            .context(format!("Missing argument: {name}"))
    };
    let line = |name: &str| -> Option<usize> {
        args[name]
            .as_u64()
            .or_else(|| args[name].as_str()?.trim().parse().ok())
            .map(|n| n as usize)
    };
    let root = &req.config.workspace;
    match name {
        "list_files" => Ok(tools::list(root)),
        "read_file" => {
            let (start, end) = (line("start_line"), line("end_line"));
            if start.is_some() || end.is_some() {
                tools::read_range(root, arg("path")?, start, end)
            } else {
                tools::read(root, arg("path")?)
            }
        }
        "search_code" => {
            let case_sensitive = args["case_sensitive"].as_bool().unwrap_or(false);
            tools::search_code(root, arg("query")?, case_sensitive, 60)
        }
        "search_docs" => {
            ensure!(
                req.use_docs,
                "Documentation is disabled for this conversation"
            );
            ensure!(
                req.library.matches_sources(&req.config.doc_sources),
                "Build the documentation index first"
            );
            let sources = retrieve(req, arg("query")?, tx)?;
            if sources.is_empty() {
                return Ok("No matching documentation found.".into());
            }
            Ok(turn.cite(sources, tx))
        }
        "web_search" => {
            ensure!(
                req.use_web,
                "Web search is disabled. Ask the user to enable it."
            );
            let query = arg("query")?;
            if !approved(
                tx,
                &req.cancel,
                "Search the web".into(),
                format!("This model-generated query will be sent to DuckDuckGo:\n\n{query}"),
            ) {
                return Ok("User declined this web search. Do not retry it.".into());
            }
            let sources = web_search(query.to_owned(), &req.cancel)?;
            if sources.is_empty() {
                return Ok("No web results found.".into());
            }
            Ok(turn.cite(sources, tx))
        }
        "write_file" => {
            let path = arg("path")?;
            let content = arg("content")?;
            ensure!(content.len() <= 128_000, "Proposed file exceeds 128 KB");
            let resolved = tools::resolve(root, path)?;
            let old = if resolved.exists() {
                ensure!(
                    std::fs::metadata(&resolved)?.len() <= 128_000,
                    "Existing file exceeds review limit"
                );
                Some(std::fs::read_to_string(&resolved)?)
            } else {
                None
            };
            propose_write(
                req,
                tx,
                turn,
                format!("Write {path}"),
                path,
                old.as_deref(),
                content,
            )
        }
        "edit_file" => {
            let path = arg("path")?;
            let (current, updated) =
                tools::plan_edit(root, path, arg("old_text")?, arg("new_text")?)?;
            ensure!(
                current.len() <= 128_000 && updated.len() <= 128_000,
                "File exceeds the 128 KB review limit"
            );
            propose_write(
                req,
                tx,
                turn,
                format!("Edit {path}"),
                path,
                Some(current.as_str()),
                &updated,
            )
        }
        "run_command" => {
            let program = arg("program")?;
            // Some models send the list as a JSON string ("[\"a\", \"b\"]") or leave it out.
            let list = match &args["args"] {
                Value::String(raw) => serde_json::from_str::<Value>(raw).unwrap_or(Value::Null),
                Value::Null => json!([]),
                other => other.clone(),
            };
            let args = list
                .as_array()
                .context("args must be an array of strings, e.g. [\"-l\"]")?
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_owned)
                        .context("Each argument must be text")
                })
                .collect::<Result<Vec<_>>>()?;
            let resolved_path = tools::resolve_program(root, program);
            // A PATHEXT match such as build.cmd runs through cmd.exe, so it counts too.
            let interpreter = tools::is_interpreter(program)
                || resolved_path.as_deref().is_some_and(|p| {
                    tools::is_interpreter(&p.to_string_lossy())
                        || p.extension().is_some_and(|e| {
                            e.eq_ignore_ascii_case("bat") || e.eq_ignore_ascii_case("cmd")
                        })
                });
            let resolved = resolved_path.map_or_else(
                || "not found on PATH or in the workspace (the command will probably fail)".into(),
                |p| p.display().to_string(),
            );
            let extra_env = workspace_env(&req.config);
            let detail = format!(
                "Working directory: {}\n\nProgram: {program}\nResolved path: {resolved}\nArguments: {}\n\nThis program runs with your account's permissions and can access files and the network outside the workspace.{}{}",
                root.display(),
                serde_json::to_string_pretty(&args)?,
                if extra_env.is_empty() {
                    String::new()
                } else {
                    format!(
                        "\n\nExtra environment variables passed for this workspace: {}",
                        extra_env.join(", ")
                    )
                },
                if interpreter {
                    "\n\nWARNING: this is a shell or script interpreter. Its arguments (for example -c, -e or -Command) can run any code, which bypasses the no-shell-expansion rule. Read every argument before approving."
                } else {
                    ""
                }
            );
            let title = if interpreter {
                format!("Run {program} (interpreter)")
            } else {
                format!("Run {program}")
            };
            if !approved(tx, &req.cancel, title, detail) {
                return Ok(
                    "User declined the command. Do not retry it without a new request.".into(),
                );
            }
            tools::command_with_env(root, program, &args, &extra_env)
        }
        mcp_name if mcp_name.starts_with("mcp__") => {
            let tool = turn
                .mcp
                .iter()
                .find(|t| t.name == mcp_name)
                .with_context(|| format!("Unknown tool: {mcp_name}"))?;
            let command = std::iter::once(tool.server.command.as_str())
                .chain(tool.server.args.iter().map(String::as_str))
                .collect::<Vec<_>>()
                .join(" ");
            let detail = format!(
                "Server: {}\nCommand: {command}\nTool: {}\n\nArguments:\n{}\n\nMCP servers run with your account's permissions.",
                tool.server.name,
                tool.tool,
                serde_json::to_string_pretty(args)?
            );
            if !approved(
                tx,
                &req.cancel,
                format!("MCP: {} · {}", tool.server.name, tool.tool),
                detail,
            ) {
                return Ok(
                    "User declined this MCP call. Do not retry it without a new request.".into(),
                );
            }
            mcp::call_tool(&tool.server, &tool.tool, args)
        }
        _ => anyhow::bail!("Unknown tool: {name}"),
    }
}

/// The name of an OpenAI-style (`function.name`) or flat (`name`) tool definition.
fn tool_name(definition: &Value) -> Option<&str> {
    definition["function"]["name"]
        .as_str()
        .or_else(|| definition["name"].as_str())
}

/// Runs a host tool. Only names the host offered are accepted; the host checks permissions.
fn host_call(host: &dyn ToolHost, name: &str, args: &Value) -> Result<String> {
    ensure!(
        host.definitions()
            .iter()
            .any(|d| tool_name(d) == Some(name)),
        "Unknown tool: {name}"
    );
    host.call(name, args)
}

/// The tool list offered for a run: the host's tools alone when a host is set (in chat mode
/// too), else the workspace and MCP tools in coding mode, else none.
fn offered_tools(req: &Request, mcp: &[McpTool]) -> Option<Value> {
    if let Some(host) = &req.tool_host {
        let definitions = host
            .definitions()
            .into_iter()
            .filter(|d| tool_name(d).is_some())
            .collect::<Vec<_>>();
        return (!definitions.is_empty()).then_some(Value::Array(definitions));
    }
    req.coding.then(|| {
        let mut defs = tools::definitions(&tools::ToolSet {
            docs: req.use_docs,
            web: req.use_web,
        });
        if let Value::Array(list) = &mut defs {
            list.extend(mcp.iter().map(|t| t.definition.clone()));
        }
        defs
    })
}

/// Runs a web search on its own thread so Stop is honoured while it waits on the network.
/// A stopped search finishes in the background and its result is dropped.
fn web_search(query: String, cancel: &AtomicBool) -> Result<Vec<Source>> {
    let (done, result) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = done.send(crate::integrations::web::search(&query));
    });
    loop {
        ensure!(!cancel.load(Ordering::Relaxed), "Stopped");
        match result.recv_timeout(Duration::from_millis(200)) {
            Ok(sources) => return sources,
            Err(mpsc::RecvTimeoutError::Disconnected) => anyhow::bail!("Web search failed"),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

/// The web search made for a new message.
enum WebOutcome {
    Skipped,
    Found(Vec<Source>),
    Failed,
}

/// Asks for approval when configured, then searches the web for the message. Only Stop is an
/// error; a failed search is reported and noted in the turn.
fn web_references(req: &Request, tx: &Sender<Event>) -> Result<WebOutcome> {
    let query = crate::integrations::web::query_from(&req.question);
    let confirmed = !req.config.confirm_web_query
        || approved(
            tx,
            &req.cancel,
            "Search the web".into(),
            format!("This query from your message will be sent to DuckDuckGo:\n\n{query}"),
        );
    if !confirmed {
        let _ = tx.send(Event::Activity("Web search skipped".into()));
        return Ok(WebOutcome::Skipped);
    }
    let _ = tx.send(Event::Activity(format!(
        "Web search query sent to DuckDuckGo: {query}"
    )));
    match web_search(query, &req.cancel) {
        Ok(sources) => Ok(WebOutcome::Found(sources)),
        Err(_) if req.cancel.load(Ordering::Relaxed) => anyhow::bail!("Stopped"),
        Err(e) => {
            let _ = tx.send(Event::Activity(format!("Web search unavailable: {e}")));
            Ok(WebOutcome::Failed)
        }
    }
}

fn retrieve(req: &Request, query: &str, tx: &Sender<Event>) -> Result<Vec<Source>> {
    if !req.use_embeddings {
        return req.library.search(query, &req.pack, 3);
    }
    let candidates = req.library.search(query, &req.pack, 12)?;
    let (sources, note) = crate::knowledge::embeddings::rerank_or_lexical(
        &crate::config::data_dir().join("embeddings.sqlite"),
        &req.config,
        query,
        candidates,
        3,
    );
    if let Some(note) = note {
        let _ = tx.send(Event::Activity(note));
    }
    Ok(sources)
}

/// Approximate prompt characters of a message; an image counts as IMAGE_CHARS.
fn size(m: &Value) -> usize {
    match m["content"].as_array() {
        Some(parts) => {
            40 + parts
                .iter()
                .map(|p| {
                    if p["type"] == "image_url" {
                        IMAGE_CHARS
                    } else {
                        p.to_string().len()
                    }
                })
                .sum::<usize>()
        }
        None => m.to_string().len(),
    }
}

/// A long tool output cut to its start with a note, or None when it is short already.
fn trimmed(m: &Value) -> Option<Value> {
    let text = m["content"].as_str().filter(|_| m["role"] == "tool")?;
    if text.chars().count() <= 400 {
        return None;
    }
    let head: String = text.chars().take(200).collect();
    let mut m = m.clone();
    m["content"] = json!(format!(
        "{head}\n[Older tool output trimmed to fit the context window]"
    ));
    Some(m)
}

/// Replaces a long tool output with its start and a note. Returns the bytes saved.
fn shorten_tool_output(m: &mut Value) -> usize {
    let Some(short) = trimmed(m) else {
        return 0;
    };
    let saved = size(m).saturating_sub(size(&short));
    *m = short;
    saved
}

/// The user's own words in a turn that may carry injected reference text.
fn question_of(text: &str) -> &str {
    let text = text.rsplit("User question:\n").next().unwrap_or(text);
    text.split("\n\nWeb references")
        .next()
        .unwrap_or(text)
        .trim()
}

/// Once history must be cut, it is cut down to this share of the budget (in tenths), so the
/// cut point and its note stay byte-identical for several turns instead of moving every turn.
const LOW_WATER_TENTHS: usize = 7;

/// Where the history was cut last time. Kept across runs (see `STICKY`) so the server's prompt
/// cache keeps matching: a moving cut or a newly trimmed message changes the prompt from that
/// point on and makes the server read everything after it again.
#[derive(Default)]
struct Sticky {
    /// Key of the first kept turn's opening message, and the note put on it.
    cut: Option<(u64, String)>,
    /// Keys of older tool outputs that were sent trimmed.
    trimmed: HashSet<u64>,
}

static STICKY: LazyLock<Mutex<Sticky>> = LazyLock::new(Default::default);

/// A stable key for a history message: its role, call id, size and text.
fn message_key(m: &Value, size: usize) -> u64 {
    let text = format!(
        "{}\u{1f}{}\u{1f}{size}\u{1f}{}",
        m["role"].as_str().unwrap_or(""),
        m["tool_call_id"].as_str().unwrap_or(""),
        text_of(&m["content"])
    );
    super::llama::fnv1a(text.as_bytes())
}

/// Earlier turns, measured once per run: message sizes and keys, each long tool output's
/// trimmed form and saving, and each turn's range. Fitting then only clones what is sent.
struct Prior<'a> {
    msgs: &'a [Value],
    sizes: Vec<usize>,
    keys: Vec<u64>,
    trimmed: Vec<Option<Value>>,
    savings: Vec<usize>,
    turns: Vec<Range<usize>>,
}

impl<'a> Prior<'a> {
    fn new(prior: &'a [Value]) -> Self {
        let start = prior
            .iter()
            .position(|m| m["role"] == "user")
            .unwrap_or(prior.len());
        let msgs = &prior[start..];
        let sizes: Vec<usize> = msgs.iter().map(size).collect();
        let keys = msgs
            .iter()
            .zip(&sizes)
            .map(|(m, &s)| message_key(m, s))
            .collect();
        let short: Vec<Option<Value>> = msgs.iter().map(trimmed).collect();
        let savings = short
            .iter()
            .zip(&sizes)
            .map(|(t, &s)| t.as_ref().map_or(0, |t| s.saturating_sub(size(t))))
            .collect();
        let mut turns: Vec<Range<usize>> = vec![];
        for (i, m) in msgs.iter().enumerate() {
            if m["role"] != "user"
                && let Some(t) = turns.last_mut()
            {
                t.end = i + 1;
            } else {
                turns.push(i..i + 1);
            }
        }
        Self {
            msgs,
            sizes,
            keys,
            trimmed: short,
            savings,
            turns,
        }
    }

    /// Fits earlier turns plus the current turn into `budget` bytes. Earlier turns stay whole
    /// so tool calls and results remain paired. Old tool outputs are trimmed first, then the
    /// oldest turns are dropped and a short note is added; the current turn is never dropped.
    /// Unchanged turns keep identical bytes, so the server can reuse its prompt cache: once
    /// cutting is needed, it goes down to the low-water mark, and `sticky` keeps the same cut,
    /// note and trimmed outputs on later calls while they still fit.
    /// Returns the messages, how many turns were left out, and their estimated size.
    fn fit(
        &self,
        current: &[Value],
        budget: usize,
        sticky: &mut Sticky,
    ) -> (Vec<Value>, usize, usize) {
        let low = budget / 10 * LOW_WATER_TENTHS;
        let mut total: usize =
            self.sizes.iter().sum::<usize>() + current.iter().map(size).sum::<usize>();
        let mut cut = vec![false; self.msgs.len()];
        if total > budget {
            // Outputs trimmed last time stay trimmed; more go, oldest first, to the low-water mark.
            for (i, &saving) in self.savings.iter().enumerate() {
                if saving > 0 && sticky.trimmed.contains(&self.keys[i]) {
                    cut[i] = true;
                    total -= saving;
                }
            }
            if total > budget {
                for (i, &saving) in self.savings.iter().enumerate() {
                    if total <= low {
                        break;
                    }
                    if saving > 0 && !cut[i] {
                        cut[i] = true;
                        total -= saving;
                    }
                }
            }
        }
        sticky.trimmed = (0..cut.len())
            .filter(|&i| cut[i])
            .map(|i| self.keys[i])
            .collect();
        let turn_size = |t: usize| {
            self.turns[t]
                .clone()
                .map(|i| self.sizes[i] - if cut[i] { self.savings[i] } else { 0 })
                .sum::<usize>()
        };
        // The fewest turns to drop, then the most worth dropping (down to the low-water mark,
        // keeping the latest earlier turn), and the previous cut when it lies between the two.
        let mut need = 0;
        while total > budget && need < self.turns.len() {
            total -= turn_size(need);
            need += 1;
        }
        let mut first = need;
        if need > 0 {
            let most = need.max(self.turns.len().saturating_sub(1));
            let (mut deep, mut deep_total) = (need, total);
            while deep_total > low && deep < most {
                deep_total -= turn_size(deep);
                deep += 1;
            }
            let previous = sticky.cut.as_ref().and_then(|(key, _)| {
                (need..=deep)
                    .filter(|&t| t < self.turns.len())
                    .find(|&t| self.keys[self.turns[t].start] == *key)
            });
            first = previous.unwrap_or(deep);
            total -= (need..first).map(turn_size).sum::<usize>();
        }
        let mut out: Vec<Value> = self.turns[first..]
            .iter()
            .flat_map(|r| r.clone())
            .map(|i| {
                self.trimmed[i]
                    .as_ref()
                    .filter(|_| cut[i])
                    .unwrap_or(&self.msgs[i])
                    .clone()
            })
            .collect();
        let earlier = out.len();
        out.extend(current.iter().cloned());
        let keep = out.len().saturating_sub(1);
        for m in &mut out[earlier..keep] {
            if total <= budget {
                break;
            }
            total -= shorten_tool_output(m);
        }
        if first > 0 {
            // The first kept turn keeps the note it was sent with, byte for byte.
            let key = (first < self.turns.len()).then(|| self.keys[self.turns[first].start]);
            let note = match &sticky.cut {
                Some((k, note)) if Some(*k) == key => note.clone(),
                _ => {
                    let topics = self.turns[first.saturating_sub(6)..first]
                        .iter()
                        .map(|r| {
                            let text = text_of(&self.msgs[r.start]["content"]);
                            question_of(&text).chars().take(80).collect::<String>()
                        })
                        .collect::<Vec<_>>()
                        .join("; ");
                    format!(
                        "[Note: {first} earlier turn(s) were left out to fit the context window. They covered: {topics}]\n\n"
                    )
                }
            };
            if let Some(m) = out.first_mut() {
                prepend_text(m, &note);
            }
            sticky.cut = key.map(|k| (k, note));
        } else {
            sticky.cut = None;
        }
        (out, first, total)
    }
}

/// Answers tool calls left without a result when a run stops mid-step, so history stays valid.
fn close_pending_calls(messages: &mut Vec<Value>) {
    let Some(i) = messages
        .iter()
        .rposition(|m| m["role"] == "assistant" && m["tool_calls"].is_array())
    else {
        return;
    };
    let answered: HashSet<String> = messages[i + 1..]
        .iter()
        .filter_map(|m| m["tool_call_id"].as_str().map(str::to_owned))
        .collect();
    let pending: Vec<Value> = messages[i]["tool_calls"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| !c["id"].as_str().is_some_and(|id| answered.contains(id)))
        .map(|c| json!({"role":"tool","tool_name":c["function"]["name"],"tool_call_id":c["id"],"content":"Not run: the run stopped before this call."}))
        .collect();
    messages.extend(pending);
}

/// Keeps the last `max` turns of history (a turn starts at a user message).
fn keep_recent(mut history: Vec<Value>, max: usize) -> Vec<Value> {
    let users: Vec<usize> = history
        .iter()
        .enumerate()
        .filter(|(_, m)| m["role"] == "user")
        .map(|(i, _)| i)
        .collect();
    if users.len() > max {
        history = history.split_off(users[users.len() - max]);
    }
    history
}

/// The system prompt, built by [`prompts::assemble`] from the editable prompt files. It is
/// always the general agent first (identity, facts, the rules of this mode: no workspace path
/// outside Code mode, which small models otherwise echo back), then the core rules (SOUL),
/// then the character as a role layered on top, and the SOUL reminder last. The SOUL applies
/// to a character, and to the plain agent only when `soul_when_no_character` is set. It
/// depends only on settings, never on the message, so llama.cpp reuses it across turns.
fn system_prompt(req: &Request) -> String {
    let config = &req.config;
    build_prompt(
        req,
        |workspace| match workspace {
            Some(workspace) => prompts::soul_text(workspace, config),
            None => prompts::soul_text_global(config),
        },
        prompts::assemble,
    )
}

/// [`system_prompt`] reading prompt copies and SOUL from `base` instead of the data folder.
#[cfg(test)]
fn system_prompt_in(base: &Path, req: &Request) -> String {
    build_prompt(
        req,
        |workspace| prompts::soul_in(base, workspace, req.config.soul_enabled),
        |parts| prompts::assemble_in(base, parts),
    )
}

fn build_prompt(
    req: &Request,
    soul: impl Fn(Option<&Path>) -> Option<String>,
    assemble: impl Fn(&prompts::PromptParts) -> String,
) -> String {
    let persona = &req.persona;
    let host = req.tool_host.is_some();
    // Only Code mode without a host works in the workspace, so only it reads the project SOUL.
    let workspace = (req.coding && !host).then_some(req.config.workspace.as_path());
    let soul = (persona.is_character() || req.config.soul_when_no_character)
        .then(|| soul(workspace))
        .flatten();
    let memory = memory_lines(persona);
    assemble(&prompts::PromptParts {
        model: &req.config.model,
        workspace,
        coding: req.coding,
        host,
        docs_or_web: req.use_docs || req.use_web,
        character: persona.is_character().then_some((
            persona.name.as_str(),
            persona.instructions.as_str(),
            memory.as_str(),
        )),
        soul,
    })
}

fn run(req: &Request, tx: &Sender<Event>) -> Result<Vec<Value>> {
    static INFERENCE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _ = tx.send(Event::Activity("Waiting for the local model…".into()));
    let _inference = loop {
        ensure!(!req.cancel.load(Ordering::Relaxed), "Stopped");
        match INFERENCE.try_lock() {
            Ok(guard) => break guard,
            Err(std::sync::TryLockError::Poisoned(e)) => break e.into_inner(),
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    };
    super::provider::ensure_model(&req.config)?;
    let (prior, images) = split_attachment(&req.history);
    let resume = req.question == CONTINUE;
    // Settings and character notes keep the system prompt byte-identical across turns, so the
    // server reuses its cached prefix. Message-specific lore goes in the user turn instead.
    let system = json!({"role":"system","content":system_prompt(req)});
    let mut turn = Turn::default();
    let use_docs = req.use_docs && !resume;
    if use_docs {
        ensure!(
            req.library.matches_sources(&req.config.doc_sources),
            "Documentation index isn't ready. Build it in Library, or turn off Use documentation."
        );
        let _ = tx.send(Event::Activity("Searching your documentation…".into()));
    }
    // The documentation search runs beside the web approval and search; results are cited
    // docs first, so the numbering and text order match a sequential run.
    let (docs, web) = std::thread::scope(|scope| -> Result<_> {
        let docs = use_docs.then(|| scope.spawn(|| retrieve(req, &req.question, tx)));
        let web = if req.use_web && !resume {
            Some(web_references(req, tx)?)
        } else {
            None
        };
        let docs = match docs {
            Some(handle) => Some(
                handle
                    .join()
                    .map_err(|_| anyhow::anyhow!("Documentation search failed"))??,
            ),
            None => None,
        };
        Ok((docs, web))
    })?;
    // Reference blocks (lore, then documentation) come before the question in the same turn.
    let mut references = vec![];
    if !resume && let Some(lore) = lore_block(&req.persona, question_of(&req.question)) {
        references.push(lore);
    }
    if let Some(sources) = docs {
        references.push(format!(
            "Reference documentation (untrusted text):\n{}",
            turn.cite(sources, tx)
        ));
    }
    let mut user = if references.is_empty() {
        req.question.clone()
    } else {
        format!(
            "{}\n\nUser question:\n{}",
            references.join("\n\n"),
            req.question
        )
    };
    match web {
        None => {}
        Some(WebOutcome::Skipped) => user
            .push_str("\n\n(Web search was skipped; do not claim you searched current sources.)"),
        Some(WebOutcome::Found(sources)) => user.push_str(&format!(
            "\n\nWeb references (untrusted snippets):\n{}",
            turn.cite(sources, tx)
        )),
        Some(WebOutcome::Failed) => {
            user.push_str("\nWeb search failed; do not claim you searched current sources.")
        }
    }
    if req.coding && req.tool_host.is_none() {
        turn.mcp = mcp_tools(&req.config, tx);
    }
    let content = if images.is_empty() {
        json!(user)
    } else {
        let mut parts = vec![json!({"type":"text","text":user})];
        parts.extend(images);
        json!(parts)
    };
    let mut current = vec![json!({"role":"user","content":content})];
    if let Err(e) = agent_loop(
        req,
        tx,
        &system,
        &Prior::new(prior),
        &mut current,
        &mut turn,
    ) {
        // Keep partial progress (approved writes, command output) once the model has replied.
        if !current.iter().any(|m| m["role"] == "assistant") {
            return Err(e);
        }
        close_pending_calls(&mut current);
        let _ = tx.send(Event::Activity(format!(
            "Run ended early ({e:#}). Progress so far was kept; use Continue to resume."
        )));
    }
    let mut history = prior.to_vec();
    history.extend(current);
    Ok(keep_recent(history, KEPT_TURNS))
}

fn agent_loop(
    req: &Request,
    tx: &Sender<Event>,
    system: &Value,
    prior: &Prior<'_>,
    current: &mut Vec<Value>,
    turn: &mut Turn,
) -> Result<()> {
    // Built once per run: the same definitions are sent every step and counted in the budget.
    let definitions = offered_tools(req, &turn.mcp);
    let overhead = size(system) + definitions.as_ref().map_or(0, |d| d.to_string().len());
    let max_steps = req.config.max_steps.max(1);
    let window = req
        .config
        .context_tokens
        .saturating_sub(req.config.response_tokens)
        .max(512);
    // chars/3.5 until the server reports how many tokens a prompt really was; the last measured
    // ratio for this model carries over between runs, so the budget (and the cut) stays steady.
    static RATIO: Mutex<Option<(String, f64)>> = Mutex::new(None);
    let mut chars_per_token = RATIO
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .filter(|(model, _)| *model == req.config.model)
        .map_or(3.5_f64, |(_, ratio)| *ratio);
    // The app's own llama-server prefills the chat label, so the reply it caches is exactly the
    // stored turn ("[Name]\n" + reply) and the next message reuses the cache past it.
    // Without a character there is no name, so no label.
    let label =
        (!req.coding && req.persona.is_character()).then(|| format!("[{}]\n", req.persona.name));
    // A prefilled reply may not be parsed for tool calls, so a run offering tools skips it.
    let prefill = label.as_ref().filter(|_| {
        definitions.is_none()
            && super::provider::prefills_assistant(&req.config)
            && super::provider::prefill_ok(&req.config)
    });
    let (mut steps, mut rounds, mut noted) = (0, 0, 0);
    // Tool calls that failed one after another; the run stops when a model cannot recover.
    let mut failures = 0;
    while steps < max_steps && rounds < max_steps * 2 {
        rounds += 1;
        ensure!(!req.cancel.load(Ordering::Relaxed), "Stopped");
        let budget = ((window as f64 * chars_per_token) as usize).saturating_sub(overhead);
        let (fitted, dropped, total) = prior.fit(
            current,
            budget,
            &mut STICKY.lock().unwrap_or_else(|e| e.into_inner()),
        );
        if dropped > noted {
            noted = dropped;
            let _ = tx.send(Event::Activity(format!(
                "Context: left out {dropped} older turn(s) to fit {} tokens",
                req.config.context_tokens
            )));
        }
        let mut messages = Vec::with_capacity(fitted.len() + 2);
        messages.push(system.clone());
        messages.extend(fitted);
        if let Some(label) = prefill {
            messages.push(json!({"role":"assistant","content":label}));
        }
        let sent = total + overhead;
        let _ = tx.send(Event::Activity("Generating locally…".into()));
        let reply = super::provider::generate(
            &req.config,
            &messages,
            definitions.as_ref(),
            tx,
            &req.cancel,
        )?;
        if let Some(tokens) = reply.prompt_tokens.filter(|&t| t > 0) {
            chars_per_token = (sent as f64 / tokens as f64).clamp(2.0, 4.5);
            *RATIO.lock().unwrap_or_else(|e| e.into_inner()) =
                Some((req.config.model.clone(), chars_per_token));
        }
        let calls = reply.calls;
        // A step that only calls tools carries no label, so the model is not taught to write it.
        let content = match &label {
            Some(label) if calls.is_empty() || !reply.content.is_empty() => {
                format!("{label}{}", reply.content)
            }
            _ => reply.content,
        };
        let mut assistant = json!({"role":"assistant","content":content});
        if !calls.is_empty() {
            assistant["tool_calls"] = json!(calls);
        }
        current.push(assistant);
        if calls.is_empty() {
            return Ok(());
        }
        ensure!(definitions.is_some(), "Unexpected tool call in chat mode");
        let _ = tx.send(Event::Token("\n\n".into()));
        let mut fresh = false;
        for call in &calls {
            ensure!(!req.cancel.load(Ordering::Relaxed), "Stopped");
            let name = call["function"]["name"].as_str().unwrap_or("");
            let args = &call["function"]["arguments"];
            let fingerprint = format!("{name}{args}");
            let repeat_read = matches!(name, "read_file" | "list_files");
            let result = if turn.declined.contains(&fingerprint) {
                "This exact action was already declined. Do not retry it.".into()
            } else if let Some(why) = turn.failed.get(&fingerprint) {
                format!(
                    "This exact call already failed this turn ({why}). Do not repeat it: change the arguments, use a different tool, or tell the user what is blocking you."
                )
            } else if repeat_read && turn.reads.contains(&fingerprint) {
                "Already done: this identical call ran earlier in this turn. Use the earlier result above.".into()
            } else {
                fresh = true;
                let _ = tx.send(Event::Activity(format!("Tool: {name}")));
                match execute(name, args, req, tx, turn) {
                    Ok(text) if text.starts_with("User declined") => {
                        turn.declined.insert(fingerprint);
                        text
                    }
                    Ok(text) => {
                        if repeat_read {
                            turn.reads.insert(fingerprint);
                        } else if matches!(name, "write_file" | "edit_file" | "run_command")
                            || name.starts_with("mcp__")
                        {
                            // Files may have changed, so earlier reads are no longer current.
                            turn.reads.clear();
                        }
                        text
                    }
                    Err(e) => {
                        let why = format!("{e:#}");
                        turn.failed.insert(fingerprint, why.clone());
                        format!("Tool error: {why}")
                    }
                }
            };
            if result.starts_with("Tool error:")
                || result.starts_with("This exact call already failed")
            {
                failures += 1;
            } else {
                failures = 0;
            }
            ensure!(
                failures < MAX_TOOL_FAILURES,
                "Stopped: the model kept making tool calls that fail ({result}). Try rephrasing, or use a larger model."
            );
            let _ = tx.send(Event::Activity(format!(
                "{name}: {}",
                result.chars().take(180).collect::<String>()
            )));
            current.push(
                json!({"role":"tool","tool_name":name,"tool_call_id":call["id"],"content":result}),
            );
        }
        // A step made only of repeats does not count against the limit.
        if fresh {
            steps += 1;
        }
    }
    let _ = tx.send(Event::Activity(format!(
        "Reached the {max_steps}-step tool limit. Progress so far was kept; use Continue to resume."
    )));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(path: &str, line: usize) -> Source {
        Source {
            path: path.into(),
            line,
            text: "t".into(),
        }
    }

    fn request(root: &Path) -> Request {
        Request {
            config: Config {
                workspace: root.to_path_buf(),
                docs: root.to_path_buf(),
                model: "test".into(),
                ..Default::default()
            },
            library: Library::new(root.join("index.sqlite")),
            question: String::new(),
            history: vec![],
            coding: true,
            use_docs: false,
            pack: String::new(),
            cancel: Arc::new(AtomicBool::new(false)),
            persona: Default::default(),
            use_web: false,
            use_embeddings: false,
            tool_host: None,
        }
    }

    #[test]
    fn chat_prompt_hides_workspace_and_names_the_model() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let mut req = request(dir.path());
        req.coding = false;
        req.config.model = "Llama-3.2-1B-Instruct-IQ3_M".into();
        req.config.soul_enabled = true;
        req.persona = Persona {
            name: "Test Character".into(),
            instructions: "Speaks in short sentences.".into(),
            memory: vec!["Likes tea".into(), " ".into()],
            ..Default::default()
        };
        let chat = system_prompt_in(&data, &req);
        // The general agent comes first; the character is a role after the rules.
        assert!(chat.starts_with("You are KYTTU, a general-purpose AI agent"));
        let role = chat
            .find("speak as the character \"Test Character\"")
            .unwrap();
        assert!(role > chat.find("Do not repeat").unwrap());
        assert!(chat.contains("Speaks in short sentences.") && chat.contains("- Likes tea"));
        assert!(chat.contains("Llama-3.2-1B-Instruct-IQ3_M"));
        let path = dir.path().display().to_string();
        assert!(
            !chat.contains(&path),
            "chat mode must not expose the workspace path"
        );
        // Stable across turns: depends on settings only.
        assert_eq!(chat, system_prompt_in(&data, &req));
        req.coding = true;
        assert!(system_prompt_in(&data, &req).contains(&path));
        // Every character shares the general-agent prefix up to the core rules.
        let with_role = system_prompt_in(&data, &req);
        req.persona = Persona::default();
        let plain = system_prompt_in(&data, &req);
        assert!(!plain.contains("speak as the character"));
        let rules = prompts::default_text(prompts::PromptId::SoulFrame)
            .split("{{soul}}")
            .next()
            .unwrap()
            .trim();
        let cut = with_role.find(rules).unwrap();
        assert!(with_role[..cut].trim_end() == plain.trim_end());
    }

    #[test]
    fn soul_follows_the_character_and_the_settings() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let mut req = request(dir.path());
        req.coding = false;
        let reminder = prompts::default_text(prompts::PromptId::SoulReminder).trim();
        // No character: the plain agent, no core rules unless asked for.
        req.config.soul_enabled = true;
        req.config.soul_when_no_character = false;
        assert!(!system_prompt_in(&data, &req).contains(reminder));
        req.config.soul_when_no_character = true;
        assert!(system_prompt_in(&data, &req).ends_with(reminder));
        req.config.soul_enabled = false;
        assert!(!system_prompt_in(&data, &req).contains(reminder));
        // A character always gets the core rules last, unless the SOUL is switched off.
        req.persona = Persona {
            name: "Ada".into(),
            ..Default::default()
        };
        assert!(!system_prompt_in(&data, &req).contains(reminder));
        req.config.soul_enabled = true;
        let s = system_prompt_in(&data, &req);
        assert!(s.ends_with(reminder));
        assert!(s.find("Ada").unwrap() < s.rfind(reminder).unwrap());
        // The project SOUL.md is read only in Code mode without a host.
        std::fs::write(dir.path().join("SOUL.md"), "PROJECT-RULE").unwrap();
        assert!(!system_prompt_in(&data, &req).contains("PROJECT-RULE"));
        req.coding = true;
        assert!(system_prompt_in(&data, &req).contains("PROJECT-RULE"));
        req.tool_host = Some(Arc::new(Host(Mutex::new(vec![]), true)) as Arc<dyn ToolHost>);
        let hosted = system_prompt_in(&data, &req);
        assert!(!hosted.contains("PROJECT-RULE") && !hosted.contains("Workspace folder"));
        assert!(hosted.contains(prompts::default_text(prompts::PromptId::ModeHost).trim()));
    }

    /// A host offering one tool (plus one nameless definition that must be dropped) that
    /// records every call it receives.
    struct Host(Mutex<Vec<String>>, bool);
    impl ToolHost for Host {
        fn definitions(&self) -> Vec<Value> {
            if !self.1 {
                return vec![];
            }
            vec![
                json!({"type":"function","function":{"name":"roll_dice","description":"Roll","parameters":{"type":"object","properties":{"sides":{"type":"integer"}},"required":[]}}}),
                json!({"type":"function","function":{"description":"no name"}}),
            ]
        }
        fn call(&self, name: &str, args: &Value) -> Result<String> {
            self.0.lock().unwrap().push(format!("{name}{args}"));
            Ok("rolled 4".into())
        }
    }

    fn names(defs: &Value) -> Vec<String> {
        defs.as_array()
            .unwrap()
            .iter()
            .filter_map(|d| tool_name(d).map(str::to_owned))
            .collect()
    }

    #[test]
    fn host_tools_replace_workspace_tools() -> Result<()> {
        let root = std::env::temp_dir().join(format!("localai-host-{}", std::process::id()));
        std::fs::create_dir_all(&root)?;
        std::fs::write(root.join("file.txt"), "keep this")?;
        let mut req = request(&root);
        // Without a host: workspace tools in coding mode, none in chat mode.
        assert!(names(&offered_tools(&req, &[]).unwrap()).contains(&"read_file".to_string()));
        req.coding = false;
        assert!(offered_tools(&req, &[]).is_none());
        assert!(
            system_prompt_in(Path::new("."), &req)
                .contains(prompts::default_text(prompts::PromptId::ModeChat).trim())
        );
        let host = Arc::new(Host(Mutex::new(vec![]), true));
        req.tool_host = Some(host.clone() as Arc<dyn ToolHost>);
        for coding in [false, true] {
            req.coding = coding;
            let offered = offered_tools(&req, &[]).expect("host tools are offered");
            assert_eq!(names(&offered), vec!["roll_dice".to_string()]);
            assert_eq!(
                offered.as_array().unwrap().len(),
                1,
                "nameless definitions are dropped"
            );
            assert!(
                system_prompt_in(Path::new("."), &req)
                    .contains(prompts::default_text(prompts::PromptId::ModeHost).trim())
            );
        }
        let (tx, rx) = mpsc::channel();
        let mut turn = Turn::default();
        let result = execute("roll_dice", &json!({"sides":6}), &req, &tx, &mut turn)?;
        assert_eq!(result, "rolled 4");
        assert!(rx.try_recv().is_err(), "host tools never ask for approval");
        assert_eq!(
            *host.0.lock().unwrap(),
            vec![r#"roll_dice{"sides":6}"#.to_string()]
        );
        // Workspace and MCP tools cannot run while a host is set.
        for name in ["write_file", "read_file", "run_command", "mcp__x__y"] {
            let err = execute(
                name,
                &json!({"path":"file.txt","content":"replace this"}),
                &req,
                &tx,
                &mut turn,
            )
            .unwrap_err();
            assert!(err.to_string().contains("Unknown tool"), "{name}: {err}");
        }
        assert!(rx.try_recv().is_err());
        assert_eq!(host.0.lock().unwrap().len(), 1);
        assert_eq!(std::fs::read_to_string(root.join("file.txt"))?, "keep this");
        // Bad arguments are rejected before the host sees them.
        let invalid = execute(
            "roll_dice",
            &json!({"_invalid_json":"{"}),
            &req,
            &tx,
            &mut turn,
        );
        assert!(invalid.unwrap_err().to_string().contains("not valid JSON"));
        assert!(execute("roll_dice", &json!("6"), &req, &tx, &mut turn).is_err());
        assert_eq!(host.0.lock().unwrap().len(), 1);
        // A host with no tools offers nothing, not the workspace tools.
        req.tool_host = Some(Arc::new(Host(Mutex::new(vec![]), false)) as Arc<dyn ToolHost>);
        assert!(offered_tools(&req, &[]).is_none());
        drop(req);
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn denied_write_never_changes_file() -> Result<()> {
        let root = std::env::temp_dir().join(format!("localai-approval-{}", std::process::id()));
        std::fs::create_dir_all(&root)?;
        std::fs::write(root.join("file.txt"), "keep this")?;
        let req = request(&root);
        let (tx, rx) = mpsc::channel();
        let reviewer = std::thread::spawn(move || {
            if let Event::Approval(approval) = rx.recv().unwrap() {
                assert!(approval.detail.contains("keep this"));
                approval.reply.send(false).unwrap();
            } else {
                panic!("Expected review request");
            }
        });
        let mut turn = Turn::default();
        let result = execute(
            "write_file",
            &json!({"path":"file.txt","content":"replace this"}),
            &req,
            &tx,
            &mut turn,
        )?;
        assert!(result.contains("declined"));
        assert!(!turn.checkpointed, "a declined write makes no checkpoint");
        reviewer.join().unwrap();
        assert_eq!(std::fs::read_to_string(root.join("file.txt"))?, "keep this");
        let invalid = execute(
            "read_file",
            &json!({"_invalid_json":"{\"path\":"}),
            &req,
            &tx,
            &mut Turn::default(),
        );
        assert!(invalid.unwrap_err().to_string().contains("not valid JSON"));
        let unknown = execute("mcp__x__y", &json!({}), &req, &tx, &mut Turn::default());
        assert!(unknown.unwrap_err().to_string().contains("Unknown tool"));
        drop(req);
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn citations_share_one_running_number() {
        let (tx, rx) = mpsc::channel();
        let mut turn = Turn::default();
        let docs = turn.cite(vec![src("a.md", 1), src("b.md", 2)], &tx);
        let web = turn.cite(vec![src("https://x", 0), src("a.md", 1)], &tx);
        assert!(docs.starts_with("[1] a.md") && docs.contains("[2] b.md"));
        assert!(web.starts_with("[3] https://x") && web.contains("[1] a.md"));
        let reported: usize = rx
            .try_iter()
            .map(|e| match e {
                Event::Sources(s) => s.len(),
                _ => 0,
            })
            .sum();
        assert_eq!(reported, 3);
    }

    #[test]
    fn history_fits_by_trimming_tools_then_dropping_turns() {
        let big = "x".repeat(2000);
        let prior = vec![
            json!({"role":"user","content":"first question"}),
            json!({"role":"assistant","content":"","tool_calls":[{"id":"c1","function":{"name":"read_file","arguments":{}}}]}),
            json!({"role":"tool","tool_call_id":"c1","content":big}),
            json!({"role":"assistant","content":"done"}),
            json!({"role":"user","content":"Reference documentation (untrusted text):\nzzz\n\nUser question:\nsecond question"}),
            json!({"role":"assistant","content":"ok"}),
        ];
        let current = vec![json!({"role":"user","content":"now"})];
        let fitter = Prior::new(&prior);
        let (all, dropped, _) = fitter.fit(&current, usize::MAX, &mut Sticky::default());
        assert_eq!((all.len(), dropped), (7, 0));
        assert_eq!(all[..6], prior[..]);
        // Trimming the long tool output is enough.
        let (trimmed, dropped, total) = fitter.fit(&current, 1200, &mut Sticky::default());
        assert_eq!((trimmed.len(), dropped), (7, 0));
        assert!(trimmed[2]["content"].as_str().unwrap().contains("trimmed"));
        assert_eq!(total, trimmed.iter().map(size).sum::<usize>());
        // Too small even after trimming: whole turns go, with a note on the first kept message.
        let (fitted, dropped, _) = fitter.fit(&current, 50, &mut Sticky::default());
        assert_eq!(dropped, 2);
        assert_eq!(fitted.len(), 1);
        let note = fitted[0]["content"].as_str().unwrap();
        assert!(note.contains("2 earlier turn(s)") && note.contains("second question"));
        assert!(!note.contains("zzz") && note.ends_with("now"));
    }

    /// A chat of `turns` question/answer pairs, each about `chars` long.
    fn chat(turns: usize, chars: usize) -> Vec<Value> {
        (0..turns)
            .flat_map(|i| {
                [
                    json!({"role":"user","content":format!("question {i} {}", "q".repeat(chars))}),
                    json!({"role":"assistant","content":format!("[Kyttu]\nanswer {i} {}", "a".repeat(chars))}),
                ]
            })
            .collect()
    }

    #[test]
    fn history_cut_stays_put_while_it_fits() {
        let mut sticky = Sticky::default();
        let current = vec![json!({"role":"user","content":"now"})];
        let budget = 20 * 250;
        let mut prompts = vec![];
        for turns in 20..30 {
            let history = chat(turns, 200);
            let (fitted, dropped, total) = Prior::new(&history).fit(&current, budget, &mut sticky);
            assert!(dropped > 0 && total <= budget);
            prompts.push(fitted);
        }
        // Each later prompt starts with the earlier one's history (the same bytes), except
        // when the cut has to move; it moves far less often than once per turn.
        let mut moves = 0;
        for pair in prompts.windows(2) {
            let earlier = &pair[0][..pair[0].len() - 1];
            if !pair[1].starts_with(earlier) {
                moves += 1;
            }
        }
        assert!(moves <= 3, "the cut moved {moves} times in 9 turns");
        // A conversation that fits again is sent whole and the cut is forgotten.
        let short = chat(2, 10);
        let (fitted, dropped, _) = Prior::new(&short).fit(&current, budget, &mut sticky);
        assert_eq!((fitted.len(), dropped), (5, 0));
        assert!(sticky.cut.is_none());
    }

    #[test]
    fn perf_history_fit_of_200_turns() {
        let mut history = chat(200, 400);
        for i in (0..history.len()).step_by(10) {
            history.insert(
                i + 1,
                json!({"role":"assistant","content":"","tool_calls":[{"id":format!("c{i}"),"function":{"name":"read_file","arguments":{}}}]}),
            );
            history.insert(
                i + 2,
                json!({"role":"tool","tool_call_id":format!("c{i}"),"content":"x".repeat(4000)}),
            );
        }
        let current = vec![json!({"role":"user","content":"now"})];
        let mut sticky = Sticky::default();
        let start = std::time::Instant::now();
        let prior = Prior::new(&history);
        let measured = start.elapsed();
        let mut dropped = 0;
        for step in 0..20 {
            dropped = prior.fit(&current, 16_000 + step * 100, &mut sticky).1;
        }
        let elapsed = start.elapsed();
        eprintln!(
            "perf_history_fit_of_200_turns: {} messages measured in {:.2} ms, 20 fits in {:.2} ms total",
            history.len(),
            measured.as_secs_f64() * 1000.0,
            elapsed.as_secs_f64() * 1000.0
        );
        assert!(dropped > 0);
        assert!(elapsed < Duration::from_secs(2), "fitting took {elapsed:?}");
    }

    #[test]
    fn stopped_runs_answer_pending_calls_and_history_is_capped() {
        let mut messages = vec![
            json!({"role":"user","content":"q"}),
            json!({"role":"assistant","content":"","tool_calls":[{"id":"a","function":{"name":"list_files"}},{"id":"b","function":{"name":"read_file"}}]}),
            json!({"role":"tool","tool_call_id":"a","content":"ok"}),
        ];
        close_pending_calls(&mut messages);
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[3]["tool_call_id"], "b");
        let history: Vec<Value> = (0..5)
            .flat_map(|i| {
                [
                    json!({"role":"user","content":i}),
                    json!({"role":"assistant"}),
                ]
            })
            .collect();
        let kept = keep_recent(history, 2);
        assert_eq!(kept.len(), 4);
        assert_eq!(kept[0]["content"], 3);
    }

    #[test]
    fn images_attach_to_the_next_question() {
        let mut history = vec![json!({"role":"user","content":"earlier"})];
        attach_images(&mut history, &[("image/png".into(), b"ABC".to_vec())]);
        attach_images(
            &mut history,
            &[
                ("image/jpeg".into(), vec![1]),
                ("text/plain".into(), vec![1]),
            ],
        );
        assert_eq!(history.len(), 2);
        let (prior, images) = split_attachment(&history);
        assert_eq!(prior.len(), 1);
        assert_eq!(images.len(), 2);
        assert_eq!(images[0]["image_url"]["url"], "data:image/png;base64,QUJD");
        let (prior, images) = split_attachment(prior);
        assert!(images.is_empty() && prior.len() == 1);
        let mut m = json!({"role":"user","content":[{"type":"text","text":"q"},images_part()]});
        assert_eq!(
            size(&m),
            40 + json!({"type":"text","text":"q"}).to_string().len() + IMAGE_CHARS
        );
        assert_eq!(text_of(&m["content"]), "q");
        prepend_text(&mut m, "note ");
        assert_eq!(m["content"][0]["text"], "note q");
    }

    fn images_part() -> Value {
        json!({"type":"image_url","image_url":{"url":"data:image/png;base64,QUJD"}})
    }

    #[test]
    fn lore_matches_keys_case_insensitively_and_is_capped() {
        let entries = vec![
            LoreEntry {
                keys: vec!["Dragon".into()],
                text: "Dragons breathe fire.".into(),
            },
            LoreEntry {
                keys: vec!["".into(), "castle".into()],
                text: "x".repeat(3000),
            },
            LoreEntry {
                keys: vec!["moon".into()],
                text: "unused".into(),
            },
        ];
        assert_eq!(
            lore_for(&entries, "a DRAGON appears"),
            "Dragons breathe fire."
        );
        let both = lore_for(&entries, "the dragon at the Castle");
        assert_eq!(both.chars().count(), crate::core::personas::LORE_CAP);
        assert!(both.starts_with("Dragons breathe fire.\nxxx") && !both.contains("unused"));
        assert!(lore_for(&entries, "nothing here").is_empty());
        let persona = Persona {
            memory: vec!["Likes tea".into(), " ".into()],
            lorebook: entries,
            ..Default::default()
        };
        let context = memory_lines(&persona);
        assert_eq!(context, "- Likes tea");
        let lore = lore_block(&persona, "moon").unwrap();
        assert!(lore.starts_with("Lore for this message") && lore.ends_with("unused"));
        assert!(lore_block(&persona, "nothing here").is_none());
        // The block sits before the question, which question_of still finds.
        let turn = format!("{lore}\n\nUser question:\nmoon?");
        assert_eq!(question_of(&turn), "moon?");
    }

    #[test]
    fn continue_resumes_with_a_continue_turn() {
        let root = std::env::temp_dir();
        let mut prev = request(&root);
        prev.question = "first".into();
        prev.history = vec![json!({"role":"user","content":"first"})];
        prev.cancel.store(true, Ordering::Relaxed);
        let next = continue_request(prev);
        assert_eq!(next.question, CONTINUE);
        assert_eq!(next.history.len(), 1);
        assert!(!next.cancel.load(Ordering::Relaxed));
    }

    #[test]
    fn workspace_env_names_follow_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config {
            workspace: dir.path().to_path_buf(),
            ..Default::default()
        };
        config.workspace_env.insert(
            dir.path().display().to_string(),
            vec![" NODE_ENV ".into(), "".into()],
        );
        config
            .workspace_env
            .insert("/elsewhere".into(), vec!["SECRET".into()]);
        assert_eq!(workspace_env(&config), vec!["NODE_ENV".to_string()]);
    }

    #[test]
    #[ignore = "requires llama-server on 127.0.0.1:8080 serving Llama-3.2-1B-Instruct-Q4_K_M"]
    fn ollama_smoke() -> Result<()> {
        std::fs::create_dir_all(crate::config::data_dir())?;
        let req = Request {
            config: Config::default(),
            library: Library::new(crate::config::data_dir().join("documentation.sqlite")),
            question: "Reply with exactly READY, without explanation.".into(),
            history: vec![],
            coding: false,
            use_docs: false,
            pack: String::new(),
            cancel: Arc::new(AtomicBool::new(false)),
            persona: Default::default(),
            use_web: false,
            use_embeddings: false,
            tool_host: None,
        };
        let (tx, rx) = mpsc::channel();
        let messages = run(&req, &tx)?;
        let output: String = rx
            .try_iter()
            .filter_map(|e| {
                if let Event::Token(t) = e {
                    Some(t)
                } else {
                    None
                }
            })
            .collect();
        println!("Model output: {output}");
        // Small models paraphrase ("I am ready…"), so only check the gist.
        ensure!(
            output.to_lowercase().contains("ready"),
            "Unexpected model response"
        );
        ensure!(
            messages.last().is_some_and(|v| v["role"] == "assistant"),
            "Missing assistant history"
        );
        Ok(())
    }
}
