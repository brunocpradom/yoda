# Yoda

A local-first, model-agnostic agentic harness in Rust — "Claude Code, driven by
open-source models running on your own machine." See [DESIGN.md](DESIGN.md) for
the architecture and rationale.

## Requirements

- Rust (edition 2024)
- [Ollama](https://ollama.com) running locally (`ollama serve`)
- A pulled model, e.g. `ollama pull qwen2.5-coder:7b`

## Run

```sh
cargo run
```

Or, to launch against **any** directory (builds a release binary on first run):

```sh
~/code/yoda_harness/run.sh
```

For a global `yoda` command, symlink it onto your PATH (e.g. `ln -s
~/code/yoda_harness/run.sh ~/.local/bin/yoda`), then run `yoda` from anywhere.

Configuration via environment variables:

| Var | Default | Meaning |
|-----|---------|---------|
| `YODA_MODEL` | `qwen2.5-coder:7b` | Ollama model to use |
| `YODA_BASE_URL` | `http://localhost:11434` | OpenAI-compatible endpoint |

The working directory is the "project" — file access inside it is auto-allowed;
access outside it, and non-allowlisted shell commands, prompt for permission.

## Commands

```
/model [name]    show or switch the active model
/skills          list available skills
/skill <name>    activate a skill (inject its instructions)
/save [name]     save the conversation to ~/.yoda/sessions
/load [name]     load a saved conversation
/sessions        list saved sessions
/reset           clear history (keep system prompt)
/help            show help
/quit            leave
```

## Tools

`read_file`, `write_file`, `edit_file`, `run_bash`, `glob_files`, `grep_files`,
plus any tools provided by configured MCP servers.

## Skills

Drop `*.md` files in `~/.yoda/skills/`. Optional frontmatter sets a description;
the body is injected when you run `/skill <name>`. See `skills.example/`.

## MCP

Copy `mcp.json.example` to `mcp.json` (project dir or `~/.yoda/`) to connect MCP
servers. Their tools appear as `server__tool` and always prompt before running.

## Develop

```sh
cargo test          # unit tests
cargo clippy        # lints
cargo fmt           # format
```
