# KYTTU - AI backlog

Each item is marked **Done** or **Partly done** (with what is still missing), checked against the code. Items without a mark are not started. User documentation is in the [wiki](https://github.com/KyttuXD/Kyttu-AI/wiki).

## Highest-value next work

1. Git-aware change review: structured multi-file patches, staging, undo/restore, and checkpoints. **Partly done:** staging, discard, checkpoints and restore exist; no structured multi-file patch review.
2. Multiple projects and named chat sessions, search/history, branch or fork a conversation. **Done.**
3. A richer code editor with syntax highlighting, diagnostics, file search, and inline diffs. **Partly done:** highlighting, find, Ctrl+P quick-open and inline diff exist; no diagnostics.
4. Long-running terminal sessions with streamed output, process-tree cancellation, and reusable task profiles. **Done.**
5. A full semantic index with incremental updates and automatic document watching; PDF/Word extraction. **Partly done:** incremental FTS, watcher, PDF/DOCX exist; embeddings only rerank, no full vector index.
6. Hardware detection and guided model download, with CPU/RAM/context recommendations per machine. **Done.**
7. Ship `llama-server` inside each package (Windows CUDA/Vulkan/CPU, Linux, macOS x64 and arm64) so llama.cpp needs no separate install; detect missing AVX2 on older Intel CPUs and pick a matching build. (Start/stop from the GUI is done.) **Partly done:** vulkan/CPU Windows, Linux, macOS bundled with AVX2 pick; no CUDA build.
8. Encrypted OS-keychain credentials and explicit per-integration permissions. **Partly done:** keychain storage for Discord secrets; no general per-integration permission model.
9. MCP/tool adapters, extension manifests, and a sandboxed plugin boundary. **Partly done:** mCP stdio tool adapter exists; no extension manifests or sandboxed plugins.
10. Character portraits, richer character cards, lorebooks, per-character memory, import compatibility, and configurable group turn order. **Partly done:** lorebooks, memory notes, turn order exist; no portrait display, no external card import.
11. Discord slash commands, per-channel profiles, streaming edits, queue/cooldown controls, and administrator allowlists. **Partly done:** slash commands, streaming edits, cooldown, channel/guild allowlists; no admin-user allowlist.
12. Optional trusted LAN inference so older Macs can use a stronger computer while keeping local model execution. **Done.**
13. Automated native UI tests, signed installers, updater, crash reporting that is opt-in, and accessibility checks. **Partly done:** kittest UI tests, updater, optional signing, dmg; no Windows installer or crash upload.
14. In-app GGUF model browser: search Hugging Face, show size/quant/RAM fit for this machine, download with resume and checksum, and delete models. **Done.**
15. llama.cpp embeddings (`llama-server --embedding`) so semantic rerank works without Ollama; one runtime for chat and retrieval. **Done.**
16. Model hot-swap and presets: switch GGUF files from the chat model picker, per-model context/threads/GPU layers, and remembered sampling settings (temperature, top-p, repeat penalty). **Partly done:** per-model presets and hot-swap on Models page; chat picker cannot switch GGUF.
17. Speculative decoding and KV-cache quantization options for faster replies on older CPUs, with a one-click "low-end machine" profile. **Done.**
18. Live performance readout in the status bar: tokens per second, prompt processing time, RAM and context usage. **Partly done:** status bar shows tok/s, ctx, RAM; prompt time only in Activity.
19. Proper chat UX: Enter to send with Shift+Enter for a newline, edit and resend a message, regenerate a reply, and stop with Esc. **Done.**
20. Markdown rendering with syntax-highlighted code blocks, tables, lists and inline code, plus an "apply to file" button on code blocks in Code mode. **Done.**
21. Multiple saved conversations in the sidebar with rename, pin, search and delete; export to Markdown. **Done.**
22. Light theme, system-theme following, accent colour choice, and a compact density setting. **Done.**
23. Image input for vision models (llama.cpp multimodal projector), with drag-and-drop and paste. **Partly done:** image attach, drag-drop, paste exist; no mmproj projector passed to llama-server.
24. Voice input and read-aloud with local Whisper and TTS models. **Partly done:** audio-file transcription via server, OS-voice read-aloud; no local Whisper/TTS models.
25. Workspace file tree with folders, ignore rules (.gitignore), and quick-open (Ctrl+P). **Done.**
26. Remember window size and position per monitor, and restore the last open view. **Done.**

## Next up, by area

### Coding agent

27. **Add a read-only search_code tool.** Literal or regex search over the workspace that skips .git, target and node_modules and returns at most N `path:line` matches, so small models can find a symbol in one step instead of reading files one by one. (high/S) **Done.**
28. **Let read_file take a line range and report truncation.** Add optional start_line/end_line, number the output lines, and end with a "truncated: lines X-Y of Z" note, because the 16,000-character cut is currently silent. (high/S) **Done.**
29. **Add an edit_file find-and-replace tool.** Take old_text/new_text where old_text must match exactly once, and reuse the existing diff review and stale-file check, so small edits do not need the model to resend a whole file through write_file. (high/M) **Done.**
30. **Recover from malformed tool calls.** Turn invalid JSON in tool arguments into a tool-error message the model can retry instead of failing the turn with `?` in provider.rs, and add a fallback parser for tool calls written as plain text or `<tool_call>` tags. (high/M) **Done.**
31. **Budget history by tokens, not a 14,000-character all-or-nothing check.** Read n_ctx from the running server (or use config.context_tokens), count with /tokenize or the last reply's usage numbers, trim old tool outputs first, then drop the oldest whole turns and replace them with a short summary. (high/M) **Done.**
32. **Keep progress when a run stops early, and add Continue.** When the step limit, an error or Stop ends a run, return the partial messages so approved writes and command output stay in history, make the step limit a setting, and offer a Continue button. (high/S) **Done.**
33. **Offer only enabled tools and short-circuit repeated calls.** Leave web_search and search_docs out of the tool list when web or docs are off, and answer an identical repeat read_file or list_files with "already done, see the earlier result" without using a step. (medium/S) **Done.**
34. **Keep the head and tail of command output.** Start the result with a one-line exit code and pass/fail summary, label stdout and stderr, and keep both the start and the end of each stream with an "N bytes omitted" marker so test summaries are not cut off. (medium/S) **Done.**

### Runtime and performance

35. **Check the llama.cpp port before launch.** Bind-test llama_port, fall back to the next free port and update direct_url, and confirm /v1/models lists the expected model before reporting success, because any other server answering /health on 8080 is currently taken as ours. (high/S) **Done.**
36. **Clean up orphaned llama-server processes.** Put the child in a Windows Job Object with KILL_ON_JOB_CLOSE and write a PID file that the next launch checks, so a crash or force-quit does not leave a server holding RAM and the port that Settings then cannot stop. (high/M) **Done.**
37. **Detect a llama-server crash during a session and offer Restart.** Check running() every few seconds and, when the process has exited unexpectedly, show the log tail with a Restart button instead of a plain "Model server is not responding". (high/S) **Done.**
38. **Pin one slot and keep the prompt prefix stable for KV-cache reuse.** Pass `--parallel 1` and `--cache-reuse`, send cache_prompt and id_slot 0, and trim history from the oldest end so the system prompt and earlier turns stay byte-identical and old CPUs do not reprocess the whole prompt each turn. (high/M) **Done.**
39. **Fall back to CPU when GPU offload fails on Windows.** Recognise GPU init or out-of-memory failures in the log, retry once with `-ngl 0 --device none`, and show the backend and device actually used next to "Running" in Settings. (medium/M) **Done.**
40. **Probe `llama-server --version` and pass only supported flags.** Run it once per binary (cached by mtime), show the build in Settings, and leave out `--jinja` or `--device` on old builds or name the minimum build needed, with tests next to the server_args tests. (medium/S) **Done.**

### Interface

41. **Write settings and the conversation atomically and never reset on a parse error.** Save through a temp file and rename with a .bak copy, and when settings.json or conversation.json fails to parse, keep the bad file, load the backup and show a banner instead of overwriting every character with defaults. (high/S) **Done.**
42. **Fix the layout on small laptop screens.** Let the left sidebar collapse to an icon rail (Ctrl+B), and below 900pt show the inspector as an overlay or disable its toggle with a tooltip instead of hiding it while the toggle still looks on. (medium/S) **Done.**
43. **Show a first-run setup checklist in the empty Chat view.** When the model is offline, list GGUF chosen, llama-server found, model running, workspace and docs folder, each with a fix button, and give the disabled Send button hover text explaining why. (high/M) **Done.**
44. **Lay out only visible messages and cache parsed markdown.** Use show_viewport with cached row heights and re-parse only the message being streamed, so long chats do not take CPU from a CPU-only llama-server on Intel Macs. (high/M) **Done.**
45. **Turn on AccessKit and label custom-painted widgets.** Enable eframe's accesskit feature, give the status dot and logo accessible names, and make sidebar file entries real buttons so Narrator and VoiceOver can read and reach them. (high/M) **Done.**
46. **Stop wiping the chat on workspace change without warning.** Choosing a folder or pressing Ctrl+N currently replaces and saves over the conversation; keep the previous one in conversation.prev.json and show a "Chat cleared - Undo" notice until item 21 lands. (medium/S) **Done.**
47. **Remember chat options across launches.** Store Code/Chat mode, Use documentation, Web search, Embedding rerank, Group chat, the selected doc pack and the active character in Config, and save settings automatically (debounced) when they change. (medium/S) **Done.**
48. **Load system fallback fonts.** Add OS fonts at startup (Segoe UI, Segoe UI Emoji and Microsoft YaHei on Windows; PingFang, Hiragino and Apple Color Emoji on macOS) so CJK, Arabic and emoji text no longer shows as boxes. (medium/S) **Partly done:** emoji and YaHei/Hiragino/PingFang load; no Segoe UI (Arabic) or Apple Color Emoji.

### Knowledge and search

49. **Number citations once per turn.** Docs, web and tool results each start again at [1] while the Sources panel numbers the merged list, so give each source one running number per turn and store it on the Source. (high/S) **Done.**
50. **Fall back to lexical results when embedding rerank fails.** If Ollama is stopped or the embedding model is missing or changed size, return the BM25 hits already found and post an Activity note instead of failing the whole turn. (high/S) **Done.**
51. **Fuse BM25 and embedding scores and allow several passages per file.** Use reciprocal-rank fusion instead of cosine only, allow up to two passages per file, and try an AND query before the OR fallback. (high/M) **Done.**
52. **Save sources with each assistant message.** Store the sources on Message and in conversation.json, and show them for the selected message, so older [n] citations still point somewhere after the next question or a restart. (medium/S) **Done.**
53. **Index text files that are not UTF-8.** Detect BOMs and UTF-16, fall back to lossy decoding for files such as Windows-1252, and list skipped files with reasons in Library instead of only a count. (medium/S) **Done.**
54. **Split documents at headings and store section titles.** Chunk Markdown, RST and HTML at headings, keep a "File > H1 > H2" breadcrumb in a weighted FTS column, and show it in citations instead of only a line number. (medium/M) **Done.**

### Characters and Discord

55. **Record each group round as one user turn.** Store the question once per round so the third and later characters see all earlier replies and the previous round, which the current two-user-turn history window drops. (high/M) **Done.**
56. **Merge character imports and confirm removals.** Import currently replaces every character and Remove has no confirmation; merge imports with renaming on name clashes, allow exporting one character, and run validate() inside persist. (high/S) **Done.**
57. **Add a Discord stop command and a time limit per reply.** Give each request its own cancel flag, add `<prefix> stop` and a Stop button on Connections, and set a configurable time limit so one long reply cannot hold the only inference slot for minutes. (high/S) **Done.**
58. **Keep one saved history per Discord channel.** Key history by channel with author names in the turns, save it under data_dir, evict the least recently used channels instead of clearing all at 50 entries, and add `<prefix> reset`. (high/M) **Done.**
59. **Fix @Name routing for multi-word and non-ASCII names.** Match the longest character name at the start of the text with Unicode case folding, since splitting at the first space and ASCII-only comparison silently drops messages like `@Code Reviewer hi`. (medium/S) **Done.**
60. **Add a per-character "Available on Discord" flag.** Default it off, filter select_personas by it, and show it on the character card, so private characters cannot be called in a public channel. (medium/S) **Done.**

### Security and privacy

61. **Run approved commands with a scrubbed environment.** Call env_clear() and pass through only PATH, HOME/USERPROFILE, SYSTEMROOT, TEMP/TMP, LANG and the Rust toolchain variables, plus an optional per-workspace list shown in the approval dialog, so tokens in the user's environment are not exposed. (high/S) **Done.**
62. **Confirm the automatic web query and use a generic User-Agent.** With Web search on, the first 240 characters of each question go to DuckDuckGo without review; show an editable query preview (or let the model write a short query) and stop sending the "KYTTU-AI/0.2" User-Agent. (high/S) **Partly done:** query approval and generic UA exist; preview is not editable.
63. **Show the resolved program path and flag interpreters in command review.** Resolve the program on PATH and show the absolute path, and show a stronger warning for shells and interpreters (cmd, powershell, bash, python, node with -c/-e/-Command) that bypass the no-shell-expansion rule. (medium/S) **Done.**

### Packaging and releases

64. **Add cargo-deny and pin GitHub Actions.** Add a deny.toml with a `cargo deny check` job in build.yml, release.yml and a weekly run, pin actions to commit SHAs, and add .github/dependabot.yml for cargo and github-actions, since the release job has contents: write. (high/S) **Done.**
65. **Set the macOS deployment target and take the bundle version from Cargo.toml.** Set MACOSX_DEPLOYMENT_TARGET and LSMinimumSystemVersion, check minos in CI, fill CFBundleVersion from Cargo.toml instead of a hard-coded 0.2.0, and fail the release when the tag and crate version differ. (high/S) **Done.**
66. **Write a local panic log, keep the previous llama-server log, and add Copy diagnostics.** Install a panic hook that writes crash-<time>.txt to data_dir (the Windows build has no console), rename llama-server.log to .prev.log before each start instead of truncating it, and add Open log and a "Copy diagnostics" button with home paths shortened; nothing is sent anywhere. (high/M) **Done.**
67. **Pin the toolchain and add a release profile.** Add rust-toolchain.toml with an exact version, a `[profile.release]` with thin LTO, codegen-units = 1 and strip, and run fmt and clippy in release.yml, which it skips today. (medium/S) **Done.**

## Audit / checks in this build

- Regression checks for rejected/stale edits, path escapes, hard-linked files, fast command output, Unicode document chunks, retrieval pack filtering, web-result parsing, Discord character routing and UTF-16 message limits.
- Preset characters removed; fresh-install defaults are tested.
- Character identity retained in shared group history.
- Individual approval for model-generated web queries.
- Local model work serialized across app and Discord to limit CPU contention.
- Explicit loopback-only direct model endpoints and no implicit API-key handling.

## Remaining verification

- Live Discord gateway/webhook delivery awaits user-owned credentials and a test channel.
- Linux/ARM/macOS build jobs are prepared, not executed from this Windows workspace.
- Older Intel Macs require device/OS-specific compatibility testing. The built-in llama.cpp runtime's CPU-only defaults for Intel Macs are unit-tested but have not run on a real Intel Mac.
- Search-engine rate limits and small-model tool reliability vary independently of the app.
