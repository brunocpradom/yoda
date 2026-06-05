# Yoda — Design Document

> A local-first, model-agnostic agentic harness written in Rust. Think "Claude Code,
> but driven by open-source models running on your own machine." This document is the
> harness component (`yoda_harness`); more components may follow.

Status: **Design draft** — under discussion, not yet implemented.
Last updated: 2026-06-05.

---

## 1. Vision

Build a terminal agent that can use tools (read/write files, run shell, search code),
respect a permission/safety boundary, speak MCP, load "skills," and route work across
one or more local open-source models — all running locally via a swappable inference
backend.

Primary day-to-day use cases (from the user):
- Writing small scripts.
- Correcting / rewriting English text.
- Other small, well-scoped assistant tasks.

These are squarely in the "works well with local 7–8B models" zone. Large, Claude-Code-scale
multi-file refactors are explicitly **out of scope for reliability** on this hardware (see §3).

---

## 2. Hardware constraints (the machine this targets)

- **Apple M4**, 10 CPU cores, **10-core GPU**
- **16 GB unified memory** ← the dominant constraint
- macOS 26.5, Metal 4

Practical model budget: ~10–11 GB usable for a model while normal apps are open.
Everything in this doc is sized to that limit.

---

## 3. Model strategy

### 3.1 The MoE memory trap (decision rationale)

Mixture-of-Experts models ("A3B" = ~3B active params) are **fast** but require memory
proportional to **total** parameters, because all experts must be resident (the router
picks different experts every token). Consequences for 16 GB:

| Model | Total params | ~Memory (Q4) | Fits 16 GB? |
|---|---|---|---|
| Qwen3-Coder-Next 80B-A3B | 80B | ~45 GB | ❌ |
| Qwen3.6 35B-A3B | 35B | ~20 GB | ❌ |
| gpt-oss:20b (MXFP4) | ~21B | ~15 GB | ⚠️ at the floor, tight |
| dense 7–8B coder | 7–8B | ~4.5–5 GB | ✅ comfortable |
| dense 14B coder | 14B | ~9 GB | ✅ tight, close apps |

Sources: InsiderLLM "MoE Models Explained", dypsis.ai "RAM vs VRAM in MoE",
Hugging Face `openai/gpt-oss-20b`. (All to be re-verified — see Open Questions.)

### 3.2 Recommended models

| Role | Candidate | ~Size | Notes |
|---|---|---|---|
| **Daily driver** | dense 7–8B coder (`qwen2.5-coder:7b` or current Qwen3-class dense 7–8B) | ~4.5–5 GB | Fast, headroom; best fit for stated use cases |
| **Stretch quality** | `qwen2.5-coder:14b` | ~9 GB | Better, slower, close other apps |
| **Occasional heavy** | `gpt-oss:20b` | ~15 GB | Only with everything else closed |
| **Avoid** | 35B-A3B / 80B-A3B MoEs | 20–45 GB | Do not fit on 16 GB |

> ⚠️ Exact best *current* dense small coder tag (June 2026) to be confirmed at pull time.
> `qwen2.5-coder` 7b/14b are confirmed to exist and fit.

### 3.3 Multi-model vs multi-role (honest take for 16 GB)

On 16 GB only **one** capable model is comfortably resident at a time; swapping models
costs a cold reload (seconds). Therefore:

- **Default: one strong dense model for everything.**
- The valuable form of "multi-agent" here is **multi-role with the same weights** —
  a planner pass, an executor pass, a reviewer pass (different prompts/contexts, one model).
- **True multi-model routing** is designed for but disabled by default; it becomes
  attractive on a 32–64 GB machine. The `Provider` abstraction (§5) makes it a config switch.

### 3.4 Capability expectations (scope honesty)

- ✅ Works well: small scripts, text rewriting, single-file edits, short tool chains.
- ⚠️ Manage carefully: tool-calling reliability (small models mangle JSON / loop) — mitigated
  by strict schema validation, constrained output, retries, and **few tools at once**.
- ❌ Not reliable locally: long multi-step, many-file agentic tasks.

---

## 4. Backend / provider decision

**Decision: implement a `Provider` trait, first backed by the OpenAI-compatible HTTP API
(`http://localhost:11434/v1`) served by Ollama. Keep an in-process llama.cpp provider as a
future option behind the same trait. Skip pure-Rust Candle for now.**

Options considered:

| | A. HTTP → Ollama (CHOSEN) | B. llama.cpp bindings | C. Candle (pure Rust) |
|---|---|---|---|
| Transport | `reqwest` to localhost | link C++ in-process | all-Rust |
| Model mgmt | Ollama handles it | manual GGUF | manual GGUF |
| Tool-call reliability | API + `format` JSON-schema | GBNF grammar (strongest) | build it yourself |
| Single binary | No (Ollama must run) | Yes | Yes |
| Effort to first agent | **Lowest** | Medium-high | Highest |

Why A first:
1. Fastest path to a working agent (Phase 1).
2. Free model hot-swap → serves the multi-model/role idea.
3. Portable: same code works with Ollama, `llama-server`, vLLM, LM Studio.
4. Structured output reachable via Ollama `format: <json schema>` (to verify).
5. Door stays open for an in-process llama.cpp provider with zero agent-loop rewrite.

> ⚠️ To verify before relying on them: current Ollama support for (a) `tools` in `/api/chat`
> and (b) `format` accepting a JSON schema for structured output.

---

## 5. Architecture

Model-agnostic core. Crate layout (workspace or single crate with modules — TBD):

```
yoda/
├─ provider/    Provider trait + OpenAI-compatible HTTP impl (Ollama).
│               Responsibilities: chat, streaming, tool-call request/parse, structured output.
├─ agent/       The loop: send → receive (text + tool calls) → execute tools → feed back → repeat
│               until the model emits a final answer or a stop condition.
├─ tools/       Typed tools, each with a JSON schema: read, write, edit, bash, grep, glob.
├─ permission/  Gate destructive/outward ops before execution (allow/deny/ask, allowlists).
├─ session/     Conversation state + history + token budget management.
├─ ui/          Streaming CLI first; ratatui TUI later.
├─ config/      Models, endpoints, routing rules, skills dir, permissions. (TOML.)
├─ mcp/         (Phase 4) MCP client — connect to MCP servers as a client.
└─ skills/      (Phase 5) Skill loading = instruction/prompt injection from files.
```

Key traits/abstractions:
- `Provider` — `async fn chat(&self, req: ChatRequest) -> ChatStream`. Hides Ollama/HTTP.
- `Tool` — `name`, `json_schema`, `async fn run(&self, args) -> ToolResult`.
- `PermissionPolicy` — decides allow/ask/deny per tool call.

Tech stack: `tokio` (async), `reqwest` (HTTP), `serde`/`serde_json` (JSON),
`clap` (CLI args), `ratatui` (later TUI). Build/test: `cargo`.

---

## 5b. Hardware upgrade path

The harness is model- and backend-agnostic, so upgrading RAM unlocks bigger models with a
**one-line config change, no code change**. Approximate tiers (verify at purchase time):

| RAM | Unlocks | Notable |
|---|---|---|
| 16 GB (current) | dense 7–8B, tight 14B, gpt-oss:20b (tight) | current sweet spot |
| 32 GB | dense 32B, 35B-A3B agentic MoE, gpt-oss:20b comfortably | big agentic jump |
| 64 GB | 80B-A3B (Qwen3-Coder-Next, ~Claude-Sonnet-class), dense 70B | "real Claude Code feel" |
| 128 GB | larger MoEs, multiple resident models | true multi-model routing practical |

Note: Ollama wraps llama.cpp for GGUF text models, so the HTTP-vs-in-process choice does **not**
change inference memory/CPU cost — model weights dominate either way. Decision (2026-06-05):
**proceed HTTP-first**; in-process llama.cpp remains a future option behind the `Provider` trait.

## 6. Roadmap (phased — something running early)

- **Phase 1 — Talk to the model.** cargo project, config, `Provider` trait + Ollama HTTP impl,
  streaming chat REPL. *Goal: prove the connection and streaming work.*
- **Phase 2 — Make it an agent.** Agent loop + tool registry + file tools (read/write/edit) +
  bash tool + permission gate. *Goal: it can do a real task.*
- **Phase 3 — Navigate & persist.** grep/glob search tools, sessions/history, nicer UX,
  structured-output/tool-call hardening.
- **Phase 4 — MCP.** MCP client to use external tool servers.
- **Phase 5 — Skills & routing.** Skill files (instruction injection); optional multi-role /
  multi-model routing behind config.

---

## 7. Open questions / to verify

1. ✅ RESOLVED (2026-06-05): best dense 7–8B coder = **`qwen2.5-coder:7b`** (beats Llama 3.1 8B
   on code; strong Qwen3 coders are 27B dense → too big for 16 GB).
2. ✅ RESOLVED (2026-06-05): Ollama supports `tools` (OpenAI function-schema), streaming with
   tool calls, and `format` JSON-schema structured output. Phase 2–3 plan confirmed viable.
3. Single crate vs cargo workspace (decision so far: single crate until Phase 4).
4. ✅ DECIDED: CLI first, TUI later.
5. Permission model details (per-tool? per-path allowlist? session-remembered grants?). — Phase 2.
6. How `gpt-oss:20b` actually performs as an occasional model on this machine (empirical).

## 8. Implementation status

- **Phase 1 — DONE (code).** cargo project (`yoda`, edition 2024), `Provider` trait
  (`src/provider/mod.rs`), Ollama OpenAI-compatible streaming impl (`src/provider/ollama.rs`),
  config with env overrides (`src/config.rs`), streaming chat REPL (`src/main.rs`). Compiles
  clean. ✅ Live-tested 2026-06-05: streamed a reply from `qwen2.5-coder:7b` end-to-end.
- **Phase 2 — DONE & tested 2026-06-05.** Non-streaming `Provider::complete` with tools, agent
  loop (`src/agent.rs`), 4 tools (`src/tools.rs`: read/write/edit/bash), allowlist permission
  gate with catastrophic denylist (`src/permission.rs`, 4 unit tests passing). Verified live:
  file create+read via the agent, out-of-project write correctly prompted & denied.
- Phases 3–5: not started.

### Phase 2 finding: local tool-calling reliability (important)

Empirical results pulling each installed model through `/v1/chat/completions` with a tool:

| Model | native `tool_calls` | argument quality |
|---|---|---|
| `qwen2.5-coder:7b` | ❌ emitted as **text** | clean JSON (wrong channel) |
| `llama3.2:3b` | ✅ | ❌ mangled / malformed |
| `mistral:7b` | ✅ | ✅ clean |

Mitigation built into the agent loop: a **text-fallback parser** (`extract_tool_calls` in
`src/agent.rs`) recovers tool calls a model printed as text, validating the name against the
registry to avoid false positives. This makes the strong-but-non-native-tool-calling
`qwen2.5-coder` usable as an agent. Trade-off to revisit: `mistral` tool-calls natively but is a
weaker coder; `qwen2.5-coder` codes better and now works via the fallback. Model is swappable
via `YODA_MODEL`.
```
