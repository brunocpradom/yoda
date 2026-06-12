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

Or, to launch against **any** directory (builds a release binary on first run),
run the location-aware launcher by its path:

```sh
./run.sh                        # from the repo directory
/path/to/yoda_harness/run.sh    # from anywhere
```

For a global `yoda` command, symlink it onto your PATH, then run `yoda` from
anywhere:

```sh
ln -s /path/to/yoda_harness/run.sh ~/.local/bin/yoda
```

Configuration via environment variables:

| Var | Default | Meaning |
|-----|---------|---------|
| `YODA_MODEL` | `qwen2.5-coder:7b` | Ollama model to use |
| `YODA_BASE_URL` | `http://localhost:11434` | OpenAI-compatible endpoint |
| `YODA_ALLOWED_COMMANDS` | safe read-only set | comma-separated programs `run_bash` may run without asking (the default excludes code-executors like `cargo`/`git`/`find` and file-dumpers like `cat`/`grep` — those prompt) |
| `YODA_ALLOW_LOCAL_FETCH` | unset | set to `1` to let `web_fetch` reach `localhost`/private/loopback addresses (off by default to block SSRF) |

The working directory is the "project" — file access inside it is auto-allowed;
access outside it, and non-allowlisted shell commands, prompt for permission.

## Commands

```
/model [name]    show or switch the active model
/mode [name]     permission mode: normal | auto | read-only
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
`web_fetch` (fetch an http/https URL and return its text with HTML stripped),
`web_search` (search the web via DuckDuckGo), `ask_user` (the model asks you a
question when it's unsure or missing info), plus any tools provided by configured
MCP servers.

## Modes

`/mode` sets how the permission gate behaves:

- **`normal`** (default) — prompt before risky actions (writes/commands outside
  the safe set, network, MCP calls).
- **`auto`** — auto-approve **everything**, including destructive commands. The
  prompt turns red (`you (auto) ▸`) so you always know it's on.
- **`read-only`** — allow reads; block all writes, commands, and network. Safe
  exploration.

## Security

The model decides which tools to run on your machine, so the trust boundary is
**you reviewing the permission prompts** (the model running locally backs that
up). Tool results — fetched pages, file contents, MCP output — feed back into the
model, so untrusted content can carry instructions it may act on (indirect prompt
injection). In `normal`/`read-only` mode the prompt still gates real side
effects; **never run `auto` mode against untrusted content** — it approves
everything with nothing in between. `web_fetch`/`web_search` block
private/loopback/metadata targets (rebinding-proof, re-checked on redirects).
Don't commit a real `mcp.json` (it's gitignored; it can hold OAuth secrets). Full
details and how to report a vulnerability: [SECURITY.md](SECURITY.md).

## Skills

Drop `*.md` files in `~/.yoda/skills/`. Optional frontmatter sets a description;
the body is injected when you run `/skill <name>`. See `skills.example/`.

## MCP & threepio

[MCP](https://modelcontextprotocol.io) (Model Context Protocol) lets Yoda use
tools provided by external servers — GitHub, Gmail, a filesystem sandbox, and so
on. Copy `mcp.json.example` to `mcp.json` (in the project dir or `~/.yoda/`) to
configure them. Each connected server's tools appear to the model as
`server__tool` and **always prompt before running**.

Each server entry takes:

| Field | Required | Meaning |
|-------|----------|---------|
| `command` | yes | Executable to spawn |
| `args` | no | Arguments passed to it |
| `env` | no | Extra environment variables for the process (e.g. per-server credentials) |

### Local (stdio) servers

Yoda speaks the MCP **stdio** transport: it spawns the server as a subprocess and
exchanges JSON-RPC over its stdin/stdout. Any stdio MCP server works directly:

```json
{ "servers": { "filesystem": {
    "command": "npx",
    "args": ["-y", "@modelcontextprotocol/server-filesystem", "/path/to/allow"]
} } }
```

### Remote (HTTP + OAuth) servers via threepio

Yoda itself only speaks stdio — it does **not** do HTTP or OAuth. To reach remote,
OAuth-protected MCP servers (GitHub, Google Gmail, etc.), it uses
[**threepio**](../threepio), a companion pure-Rust bridge. Yoda spawns threepio
like any stdio server; threepio handles the Streamable HTTP transport and the
full OAuth 2.1 + PKCE browser flow, caching tokens so you rarely re-authorize.

```json
{ "servers": {
    "github": {
      "command": "/abs/path/to/threepio",
      "args": ["https://api.githubcopilot.com/mcp/"],
      "env": { "THREEPIO_CLIENT_ID": "your-github-oauth-app-client-id" }
    },
    "gmail": {
      "command": "/abs/path/to/threepio",
      "args": ["https://gmailmcp.googleapis.com/mcp/v1"],
      "env": {
        "THREEPIO_CLIENT_ID": "your-google-oauth-client-id",
        "THREEPIO_CLIENT_SECRET": "your-google-oauth-client-secret"
      }
    }
} }
```

Setup (per server): create an OAuth client with callback
`http://localhost:33418/callback`, fill the `env` credentials, run
`threepio <url> --login` once to cache a token, then start Yoda. See
[threepio's README](../threepio/README.md) for the details and the reasoning
behind the pre-login step.

## Develop

```sh
cargo test          # unit tests
cargo clippy        # lints
cargo fmt           # format
```
