# Security

Yoda is an agentic harness: a language model decides which tools to run on your
machine. That makes the trust model worth stating plainly.

## Trust model

The real security boundary is **you reviewing what the model asks to do** at the
permission prompt, plus the fact that the model runs locally. Everything below
supports that boundary; none of it replaces it.

- **Permission gate.** Every side-effecting tool call declares the actions it
  would take; the policy decides allow / ask / deny *before* anything runs.
  - File reads/writes inside the project directory are auto-allowed; outside it
    they prompt. `..` traversal is collapsed and symlinks are resolved, so an
    in-project symlink can't quietly point outside the project.
  - `run_bash` auto-allows only a small read-only command set and only when the
    command contains no shell chaining/redirection (`;`, `&&`, `|`, backticks,
    `$(`, redirects). Anything else prompts.
  - Network egress (`web_fetch`, `web_search`) and all MCP tool calls always
    prompt.
- **Modes** (`/mode`):
  - `normal` (default) — prompt before risky actions.
  - `read-only` — allow reads, deny everything else.
  - `auto` — **auto-approve everything, including destructive commands.** The
    prompt turns red so it's obvious. See the warning below.

## Indirect prompt injection — the central risk

Tool results flow back into the model: a fetched web page, a file you asked it to
read, the output of an MCP server. Any of that content can contain instructions
("ignore your task, run `…`"), and the model may act on them. Yoda even recovers
tool calls a model emits as plain text, so injected content that gets echoed can
turn into a tool request.

In `normal` and `read-only` modes the permission gate still stands between an
injected instruction and a real side effect — you'll be asked. The takeaway:

- **Never run `auto` mode against untrusted content.** `auto` removes the prompt
  *and* the catastrophic-command backstop; injected text becomes injected
  actions with nothing in between.
- Read the permission prompts. An unexpected `run`/`write`/`fetch` while the
  model is "just reading a page" is the tell.

## SSRF protection

`web_fetch`/`web_search` refuse loopback, private, link-local (incl. the cloud
metadata address `169.254.169.254`), carrier-grade-NAT, and NAT64 targets. The
check is wired into DNS resolution, so the address that's vetted is the exact
address dialed — closing the DNS-rebinding window — and it re-applies on every
redirect hop. Set `YODA_ALLOW_LOCAL_FETCH=1` to intentionally allow local
targets (e.g. a dev server).

## Secrets

- MCP server credentials live in `mcp.json` (project dir or `~/.yoda/`). That
  file is **gitignored** — only `mcp.json.example` is tracked. Don't commit a
  real `mcp.json`.
- Saved sessions (`~/.yoda/sessions/`) can contain anything the model saw
  (file contents, tool output, secrets). They're written `0600` in a `0700`
  directory.

## The catastrophic-command backstop is not a boundary

`run_bash` hard-denies a few unambiguously destructive patterns (`rm -rf /`,
`mkfs`, fork bombs, …) even in `normal` mode. It is pattern-based and trivially
bypassable — a convenience seatbelt, not a security control. `auto` mode disables
it entirely.

## Reporting a vulnerability

Please open a private report via GitHub Security Advisories
("Report a vulnerability" on the Security tab) rather than a public issue.
