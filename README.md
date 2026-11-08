# KYTTU - AI

A native desktop app for a local AI assistant: a coding helper that reads and edits your project with your approval, a searchable documentation library with cited answers, and optional Discord bots. Models run on your own computer with [llama.cpp](https://github.com/ggml-org/llama.cpp) — no cloud AI service, no account, no API key.

Runs on Windows 10/11, Linux (x64, ARM64) and macOS, including older Intel Macs (macOS 10.13+ for the app, 10.15+ for local models).

## Download

Get the zip for your computer from the [latest release](https://github.com/KyttuXD/Kyttu-AI/releases/latest), unzip it and open the app. On first launch it sets up llama.cpp and downloads a model sized for your computer automatically, and it keeps itself up to date from GitHub releases.

## Documentation

Everything else is in the **[wiki](https://github.com/KyttuXD/Kyttu-AI/wiki)**:

- [Getting Started](https://github.com/KyttuXD/Kyttu-AI/wiki/Getting-Started)
- [Models and llama.cpp](https://github.com/KyttuXD/Kyttu-AI/wiki/Models-and-llama.cpp)
- [Intel Macs](https://github.com/KyttuXD/Kyttu-AI/wiki/Intel-Macs)
- [Chat and Coding](https://github.com/KyttuXD/Kyttu-AI/wiki/Chat-and-Coding)
- [Discord Bots](https://github.com/KyttuXD/Kyttu-AI/wiki/Discord-Bots)
- [Privacy and Data](https://github.com/KyttuXD/Kyttu-AI/wiki/Privacy-and-Data)
- [Building from Source](https://github.com/KyttuXD/Kyttu-AI/wiki/Building-from-Source)

Plans are in [ROADMAP.md](ROADMAP.md).

## Build

```bash
cargo run
```

Packaging, CI and releases: see [Building from Source](https://github.com/KyttuXD/Kyttu-AI/wiki/Building-from-Source).
