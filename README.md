# KYTTU - AI

A native Rust desktop coding assistant, chatbot, character studio, and optional Discord bot. The interface uses red accents, dark purple panels, and separated workspace/context areas.

## Start on this computer

Open **dist/KYTTU - AI.exe**. This first packaged Windows preview uses the development build to make it available quickly. Build an optimized release with:

```powershell
.\scripts\package.ps1
```

Everyday use does not require a terminal. **llama.cpp is the primary runtime on every platform**: install `llama-server` (Homebrew `brew install llama.cpp`, or a [release zip](https://github.com/ggml-org/llama.cpp/releases) unpacked next to the app), then in **Settings → llama.cpp** choose a `.gguf` model and click **Start model**. The saved model starts automatically on the next launch. Ollama remains available as an optional provider and is still used for embedding rerank.

1. Open a coding project from the sidebar.
2. Choose your documentation folder in Library. On this PC it defaults to `C:\Users\Kyttu\Documents\Projects\Docs`.
3. Wait for the first full-text index to finish. Rebuild it after updating documentation.
4. Choose a chat/coding model, and optionally select a documentation pack.
5. Use Code mode for reviewed file/command tools, or Chat for conversation and character group chats.

## What works

- Native desktop GUI, streamed replies, workspace file previews, copy code/message actions.
- Coding tools: file listing/reading, documentation search, file creation/replacement, and program execution.
- Every file write and command requires a review dialog. Existing files are backed up. Writes use atomic replacement and reject content changed since review.
- Persistent documentation search: Markdown, MDX, text, RST, AsciiDoc, and HTML. HTML source line references use extracted-text lines.
- Optional **Ollama embedding models** for semantic reranking of a bounded full-text shortlist. Select an embedding model in Settings, then enable Embeddings in Conversation. Vectors are cached locally; this is hybrid retrieval, not an exhaustive vector index.
- Optional DuckDuckGo web search with reference snippets/URLs. Enable Web search to send the current question as a search query. Additional model-generated searches require individual query approval. The provider can rate-limit/block automated requests; errors are shown and the model continues without claiming live references.
- User-created characters with names, personalities, optional model overrides and Discord avatar URLs. **No preset characters are installed.** Character JSON import/export is supported; importing replaces the current character list.
- In-app group chat: selected participants reply in sequence, up to six characters. Group chat is available in Chat mode. Create characters and mark their group participation first.
- Optional Discord bot with a channel allowlist, prefix commands, per-user in-memory history, character routing, group replies, and optional character-name/avatar webhook posting.
- Local conversation persistence and JSON export.
- Keyboard shortcuts: Ctrl+Enter send, Ctrl+N new conversation, Ctrl+O open workspace, Ctrl +/−/0 interface scale (also in Settings → Interface).
- The window fits itself to the screen on first launch, so high-DPI laptop displays (175–200% scaling) don't push the message box off-screen.

## Own model hook / older Intel Macs

Settings provides two model providers:

- **llama.cpp** (default): the app launches and stops `llama-server` itself with the chosen GGUF file, on `127.0.0.1` only, with `--jinja` for tool calls. It looks for `llama-server` in the path you choose, next to the app (including `KYTTU - AI.app/Contents/MacOS` or `Resources`), Homebrew (`/usr/local/bin` on Intel, `/opt/homebrew/bin` on Apple Silicon), then `PATH`. Server output goes to `llama-server.log` in the data folder. Under **External server** you can instead point at any OpenAI-compatible server you run yourself on localhost.
- **Ollama** (optional): its local chat and embedding APIs.

**Intel Macs:** with GPU offload on Auto, the app runs llama.cpp CPU-only (`-ngl 0 --device none`), because Metal on Intel/AMD GPUs is slow or unstable. Use small quantized models (1–4B, Q4_K_M), 2048 context tokens and a 256-token reply cap. Set CPU threads to the number of physical cores if the default is slow. The context slider applies when the model starts. Embedding rerank still needs Ollama; leave it off on llama.cpp-only machines.

Homebrew's `llama.cpp` works on recent Intel macOS. To build for an older Intel Mac or CPU, with Xcode command-line tools, CMake and Git:

```bash
git clone https://github.com/ggml-org/llama.cpp
cd llama.cpp
cmake -B build -DGGML_METAL=OFF -DCMAKE_BUILD_TYPE=Release
cmake --build build --config Release --target llama-server -j 2
```

Then in Settings → llama.cpp, choose `build/bin/llama-server` as the server and start your model.

A source build on the target Mac selects instructions appropriate to that CPU. An OS older than what the current Rust GUI/dependencies support may need pinned older dependencies, an older toolchain, or a Linux installation. No blanket compatibility with every historical Intel macOS version is claimed. macOS Intel/ARM and Linux builds are prepared in CI but have not been run from this Windows machine.

Upstream references: [llama.cpp server](https://github.com/ggml-org/llama.cpp/tree/master/tools/server), [build instructions](https://github.com/ggml-org/llama.cpp/blob/master/docs/build.md).

## Discord configuration

The app does not create an account, bot, token, or webhook for you, and never connects automatically.

1. Create your own bot in the [Discord developer portal](https://discord.com/developers/applications).
2. Enable **Message Content Intent**. Invite it with View Channel and Send Messages permissions.
3. In Connections, enter the bot token and a single channel ID. Tokens stay in memory and are not saved to settings or character exports.
4. Optionally enter an existing webhook URL from that same channel. It is validated against the channel before posting.
5. Create your own characters, save, then click Connect bot yourself.

Examples (replace CharacterName with your character's name):

```
!ai hello
!ai @CharacterName help with this idea
!ai group discuss this idea
```

Characters post through the supplied webhook using their configured names/avatars, or as labelled messages from the bot without a webhook. This is an independent proxy-style implementation; it does not integrate with a Tupperbox or PluralKit account.

Only prefixed human messages in the configured channel are handled. Bot/webhook messages are ignored to prevent reply loops. Mentions are disabled in outgoing messages. Remote file/command tools and web search are disabled. Local docs are excluded unless you explicitly enable Discord documentation sharing. Relevant passages may then appear in replies visible to that channel.

Reconnect after changing characters, model, channel, or bot settings. Busy bot requests are dropped rather than queued. Disconnect stops generation between stream events; an already in-flight request/post can finish. Live Discord delivery requires your credentials and has not been tested with your account.

## Cross-platform builds

`scripts/package.ps1` packages Windows. `bash scripts/package.sh` packages Linux or a macOS .app bundle.

GitHub Actions:

- **Build** (`.github/workflows/build.yml`) runs on every push to `main` and every pull request: `cargo fmt --check` and clippy, then tests and packaging on Windows x64, Linux x64/ARM64 and macOS Intel/Apple Silicon. Each package is uploaded as a workflow artifact.
- **Release** (`.github/workflows/release.yml`) runs when you push a tag such as `v0.3.0` (`git tag v0.3.0 && git push origin v0.3.0`). It builds the same five targets and publishes a GitHub release with one zip per platform and `SHA256SUMS.txt`.
- Dependabot opens weekly Cargo and monthly Actions update PRs.

macOS bundles are unsigned and not notarized.

Linux build dependencies include a C/C++ compiler, pkg-config, GTK3, X11/Wayland development libraries, OpenGL development libraries, and OpenSSL development libraries (see workflow). A recent Rust toolchain is required to build.

## Data and limits

Per-computer storage is `%LOCALAPPDATA%\KyttuAI` on Windows, or the standard local app-data folder elsewhere:

- `settings.json`: paths, provider preferences, characters, and nonsecret Discord settings.
- `documentation.sqlite`: document text/full-text index.
- `embeddings.sqlite`: cached local embedding vectors.
- `conversation.json`: current conversation and recent model/tool history, potentially including code and document passages.
- `backups/*.bak`: original contents of replaced files, named by timestamp.

The current conversation is replaced by New conversation. Export anything you want to keep first. There is no encrypted credential vault yet; Discord credentials are memory-only.

File tools are constrained to the workspace, with symlink/path checks, .git exclusion and stale-write checks. This is not an OS sandbox: approved commands have your account's normal file/network permissions. Commands run up to 60 seconds; output is capped; descendants may need manual cleanup after a timeout. Stop takes effect between operations/stream events and cannot instantly interrupt every blocking network call.

The agent allows eight tool rounds. File reads are limited to 256 KB / 16,000 characters; writes to 128 KB; listing to 400 files and five levels. New file parents must already exist. Documentation files over 2 MB/non-UTF-8 files and PDFs/Word/binaries are not indexed. Small models may make incorrect tool calls. Web search may be unavailable due to provider blocking. Citation text is model-generated; inspect the source passages.

## Code organization

- `src/ui/`: application state, layout/screens, character and connection editors.
- `src/core/`: per-computer configuration, agent loop, provider transports, character types, reviewed tools.
- `src/knowledge/`: extraction/chunking/full-text search and embedding reranking.
- `src/integrations/`: optional web search and Discord gateway/webhook adapter.
- `scripts/`: native packaging.
- `.github/workflows/`: cross-platform build matrix.

## Checks

```powershell
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
cargo test ollama_smoke -- --ignored --nocapture
```

Ignored smoke tests require a running local model server. No cloud credentials are used by the tests.
