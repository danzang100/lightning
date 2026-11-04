# Lightning — Project Spec

> A small, local-first tool that reads the session logs your coding agents already write and tells you, with honest confidence, **which harness + model combination works best for which kind of work**, then proposes reviewable changes and later checks whether they helped. Shipped as a lightweight binary plus portable skills, installable via cargo or npx.

Status: draft v0.3 (name set to Lightning) · Research date: November 2026 · Audience: the builder, and a coding agent as implementer

---

## 0. How to read this document

- §1–§3 explain the idea and **how we arrived at it** (the reasoning trail, so the decisions are not arbitrary).
- §4 is the research: who uses what, what exists, what users complain about.
- §5–§6 state who this is for and **what we offer and deliberately do not offer**.
- §7–§14 are the product and technical design.
- §15–§18 are validation, roadmap, success metrics and risks.
- Every factual claim about the outside world carries a source in §19 and is labeled as measured, third-party, or assumed. Where a claim is an assumption or a hypothesis, it says so.

---

## 1. Summary

Developers now mix coding agents. Most use two to four tools, and many switch models within a tool and across tools. They choose by habit, benchmarks, or feel. Meanwhile, every harness writes detailed local session logs, and several existing tools already read those logs to show **cost, tokens, tool usage and history**.

What none of the tools we found do is answer the decision question:

> For the work I actually do, which **harness + model pair** gives the best result per dollar (or per unit of quota), where does each pair fail, and what should I change? And after I change it, did it help?

This project fills that gap. It reads existing logs, normalizes them, computes per-pair and per-task-type cost and reliability with explicit confidence (including "not enough data"), proposes small config changes as diffs the user approves, and later measures whether each adopted change helped.

## 2. The core insight: the unit is the (harness, model) pair

The common framing is "which model is best." The evidence says that is incomplete: the same model can behave very differently in different harnesses (prompt assembly, tool-call format, retry logic, context handling). So the thing to compare is the **pair**, and the thing to decide per task type is **which pair to use**. This is the single most important design choice in the project and is the reason cross-harness support is central, not a bonus.

## 3. How we arrived here (the reasoning trail)

This section records the path so future decisions can be checked against it.

1. **Starting idea.** A background analyzer that reads observability data from coding-agent usage, finds token waste and repeated workflows, proposes skills, and learns user and per-model behavior.
2. **Distribution question.** Instead of building a new harness around it, ship it as skills/plugins for harnesses people already use (adoption is far easier than switching tools). Conclusion: a plugin bundle is right, but skills are only instructions, so a real analyzer binary must sit behind them.
3. **Capture question.** Both major harnesses already write full local transcripts, so capture does not need building; only adapters to read them.
4. **Is it unique? (first research pass).** Mostly no. Usage and cost analytics (ccusage and others), friction and CLAUDE.md suggestions (Claude Code's built-in `/insights`), skill mining (several community tools), and memory consolidation (built into Claude Code and Codex) all exist. Conclusion: do **not** build a generic analyzer.
5. **Refinement: model comparison.** People switch models constantly (e.g. Opus vs Sonnet across tasks, sessions and subagents). Existing tools report cost by model but not whether the expensive model was worth it. Candidate wedge: cost per successful outcome by model and task type.
6. **Refinement: cross-harness (second research pass).** People also switch **harnesses**. Surveys show most use multiple tools, and benchmarks show the same model varying widely across harnesses. Existing cross-harness tools (AgentsView, Agent Sessions, others) read many harnesses but only report usage, cost and history. Conclusion: the unit should be the harness + model pair, and the gap is **outcome-based comparison, recommendations, and closed-loop measurement**, not viewing or cost reporting.
7. **Form factor.** A small binary (cargo/npx) plus portable skills plus a read-only MCP server, usable from any harness. Not a new harness, not a dashboard.

### What we corrected along the way
- Early on we under-weighted multi-model use within a single vendor (Opus/Sonnet switching), which is common and a real use case.
- We initially treated cross-harness viewing as potentially unique. The second research pass showed it is already well covered, so it is explicitly **not** our pitch (see §6).

## 4. Research findings

> Quality note: figures come from third-party surveys, vendor-neutral write-ups, and project pages. We did not install or test the competing tools. Star counts and rankings change quickly. Re-verify before any public claim (§15.3).

### 4.1 Which harnesses people use

| Tool | JetBrains Developer Ecosystem Survey 2026 (15,000+ pro devs, May–Jul 2026) | Stack Overflow 2026 (n=12,255, "used in past year") | Pragmatic Engineer (906 respondents, Jan–Feb 2026, newsletter audience) |
|---|---|---|---|
| Claude Code | 39% (up from 18% in Jan; 47% in US) | 65.5% | Most used; "most loved" at 46% |
| GitHub Copilot | 21% (down from 29% a year earlier) | 58.7% | Third; leads at companies with 10,000+ employees |
| OpenAI Codex | 16% (up from 3% in Jan) | 29.5% | ~60% of Cursor's usage |
| Cursor | 12% (down from 18% in Jan) | 25.5% | Fourth |
| OpenCode | 7% | 13.6% | ~10% |
| Google Antigravity | 6% | 16.0% | ~10% (with Gemini CLI) |

Notes: Differences between surveys reflect different samples and question wording (usage at work vs. past year vs. newsletter readers). Treat as a ranking, not exact share. JetBrains reports 90% of professional developers use AI coding agents at work at least weekly, 68% daily.

**Multi-tool use (the premise of this project):** In the Pragmatic Engineer survey, **70% use two to four tools, 15% use one, 15% use five or more**. The JetBrains write-up did not report multi-tool share. The Stack Overflow percentages sum well over 100%, consistent with multi-tool use (inference, not stated).

### 4.2 Open-source and other harnesses

A third-party roundup (star counts as of Aug 29, 2026) lists, among others: OpenCode (~202k stars, MIT), OpenAI Codex CLI (~119k, Apache-2.0), Pi (~98k), OpenHands (~85k), Cline (~67k), Goose (~54k), Aider (~48k; no commits since May 2026 per the roundup), Continue (~36k), plus Crush, Kilo Code, Qwen Code, Grok Build and others. Treat star counts as popularity signals only.

Two projects behave differently from coding CLIs and matter for adapter planning:
- **OpenClaw**: MIT-licensed, self-hosted general-purpose agent framework (20+ messaging channels, persistent memory, community skills, scheduled operations). Coding is one use among several. Noted concerns: broad permissions and plugin/skill supply-chain risk.
- **Hermes Agent** (Nous Research): open-source agent with self-curated memory and self-created skills (agentskills.io-compatible), subagents via a `delegate_task` tool, many model providers including local, messaging gateways; sessions stored in SQLite with full-text search. Reported at 30k+ stars.

Implication: sessions from these are less "coding task" shaped, so task-type classification is harder. They are later adapters, not v1.

### 4.3 What users like and dislike (from the sources we read)

**Likes**
- Claude Code: live steering, project memory; most-loved in one survey.
- Codex: delegate-and-review in a sandbox; strong on terminal and infrastructure work.
- A common combined workflow: Codex for terminal-heavy and first-pass implementation, Claude Code for refactors, review and architecture, often wired together over MCP (e.g. cross-model review of a commit).

**Dislikes / pain points**
- **Quota burn and cost opacity.** Anecdotes include a single first request consuming over half of a Pro session, and users reporting hundreds to thousands of dollars of token spend without clear attribution. (Anecdotal; forum-sourced via a blog post.)
- **Context bloat** before the first tool call, retries, state re-sent between steps, memory summarization overhead.
- **Config duplication and drift** across tools (instruction files, MCP servers), and silent misconfiguration (e.g. a local-model server defaulting to a small context window misdiagnosed as a model problem).
- **No rigorous way to compare.** One write-up on running Claude Code and Codex together states it offers no rigorous method for measuring which is better and recommends treating public benchmarks as directional.

**What users say they wish they could measure** (from one detailed account): first-turn context size, average retries per task, tool-call volume per successful patch, state carried between turns, and cost or quota burn per hour of autonomous runtime. These are cheap to compute from logs and are in scope for v0.1.

### 4.4 Evidence that the harness matters as much as the model
- A benchmark of 14 agent tools on the same task set with the same cloud model found pass rates from **100% down to 0%** depending on the harness; a weaker local model exposed harness weaknesses even more sharply. Caveats: small samples, last two attempts per task counted, and the author built one of the tested tools.
- Another write-up claims the same model scored 78% in one harness and 42% in another. The full evidence was paywalled and **not verified** by us.
- An active benchmarking framework from AWS samples (sample-agent-cost-bench) compared models and harnesses on 9–10 authored tasks and found cost per success depended strongly on which cost basis was used. This shows the value of cost-per-success but also that active benchmarks need authored tasks and are small.

### 4.5 Existing tools and what each does

| Tool | Coverage | What it does | Compares models or measures outcomes? |
|---|---|---|---|
| **AgentsView** (MIT, ~6.3k stars, v0.46.x) | 50+ agents | Local session browser and search; token and cost dashboards with per-session and per-model breakdowns; tool-sequence view with errors/retries; CLI with versioned JSON `stats`; **8 read-only MCP tools**; opt-in git and GitHub PR outcome counts; optional Postgres sync | Per-model **cost** only; opt-in output counts; **no model/harness comparison** |
| **Agent Sessions** (MIT, ~850 stars, macOS 14+ only) | 14 agents incl. Hermes and OpenClaw (only Codex and Claude have a maintainer) | Browse, search, resume; quota meter for Claude/Codex limits | No |
| **session-analytics** (MIT, tiny, "public preview") | 7+ harnesses | Local content-free analytics grouped by repo; CLI, dashboard, MCP | No outcomes or comparison |
| **AI Session Analyzer** (MIT, tiny) | Claude Code primary, plus several others | Markdown reports; per-model tokens and cost; analysis left to the user | No recommendations |
| **agent-session-otel** (MIT, alpha) | Claude Code, Codex | Converts logs to OpenTelemetry traces using GenAI conventions | No |
| **ccusage** (~16k stars) | Many harnesses | Cost and token reports; real-time monitoring; MCP server | No |
| **Claude Code built-ins** | Claude Code only | `/usage`, `/context`, `/cost`; `/insights` (30-day report, friction analysis, CLAUDE.md and skill suggestions); auto memory and background consolidation | Single vendor; no cross-model outcome comparison |
| **Codex memories** | Codex only | Background session summarization | n/a |
| Auto-skill-style tools | Many | Detect repeated workflows, generate SKILL.md | No |
| Routing skills (static) | n/a | Fixed rules for which harness suits which task | No data behind them |
| sample-agent-cost-bench | Several | **Active** benchmarking on authored tasks | Yes, but active, small, authored |

**Reading of the landscape:** viewing, search, cost and token analytics across many harnesses is a **solved, crowded** space, and one project (AgentsView) already has broad parsers, an MCP server and optional outcome signals, making it the most likely place for adjacent features to land. No tool we found does **passive, outcome-based comparison of harness × model pairs from a user's own sessions**, gives recommendations with confidence, or measures whether an adopted change helped.

### 4.6 Data availability and plumbing (facts relevant to design)
- Claude Code writes JSONL transcripts under `~/.claude/projects/**` and **deletes them after 30 days by default** (the `cleanupPeriodDays` setting); the sweep runs at startup. `/insights` looks at the last 30 days.
- Codex writes date-keyed rollout files under `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` and has no session-end event (only a notify hook).
- Formats are **undocumented and change between releases**; projects that parse them warn that readers may break.
- Claude Code plugins are distributed via a marketplace repo (`.claude-plugin/marketplace.json`). Codex plugins do not declare MCP servers in their manifest; MCP registration lives in the user's `~/.codex/config.toml`.
- OpenTelemetry GenAI semantic conventions exist but are still at development stability.

## 5. Target audience

### Primary
- **Multi-tool, multi-model developers** (the 70% who use two to four tools) who want evidence for routing decisions: which harness and model for which task.
- **Heavy users watching spend or quota** on subscriptions or API bills.
- **Subagent and workflow configurers** who assign models to subagents and want to know which assignments are wasteful.

### Secondary
- **Local-model users**, where tool-call reliability varies by model, quantization and harness, and "cost" is time and memory rather than dollars.
- Small teams that want shared, redacted, aggregate findings (post-v1).

### Not the audience
- Casual single-tool users with few sessions (not enough data; built-ins suffice).
- Enterprises needing governance and compliance dashboards.

### Jobs to be done
1. "Tell me whether I'm overpaying for the expensive pair on tasks where a cheaper pair does fine."
2. "Tell me where each pair fails me, so I stop finding out the hard way."
3. "Give me the exact change as a diff, and tell me later whether it helped."

## 6. What we offer, and what we deliberately do not

### We offer
1. **Harness × model pair comparison** by task type and repo, with sample sizes and confidence, and an explicit "insufficient data" state.
2. **Cost (and quota) per successful outcome**, using a transparent proxy for success.
3. **Switch analysis**: what happens to cost, retries and corrections when a user changes model or harness mid-stream.
4. **Pair-scoped suggestions as diffs**: default model or harness for a task type, subagent model assignment, and instructions scoped to the pair that actually needs them.
5. **Closed-loop review**: after a change is adopted, measure whether cost fell and quality held.
6. **Cheap, user-requested diagnostics** (first-turn context size, retries per task, tool calls per successful change, burn per hour).
7. **Longer memory than the harness keeps**: archive and normalize logs before retention deletes them.
8. **Portable delivery**: one small binary (cargo / npx), portable skills, and a read-only MCP server, so any harness can call it.
9. **Local and private by design**: metadata only, no network in core paths.

### We deliberately do not offer (non-goals; binding for v1)
- **N1. A session viewer, search, or resume tool.** Solved by existing tools.
- **N2. General usage and cost dashboards.** Solved (ccusage, AgentsView). Our cost numbers exist only to feed comparison.
- **N3. Generic friction reports and instruction-file suggestions.** Covered by built-ins like `/insights`. We only suggest changes that are pair-scoped and evidence-backed.
- **N4. Skill mining and skill generation** from repeated workflows. Covered by community tools. Possibly later as a consumer of our data.
- **N5. Memory systems** (auto memory, consolidation, third-party memory).
- **N6. A new coding agent or harness.** We never run the user's agent.
- **N7. An observability backend or tracing platform.** We may export normalized events in an OTel-compatible form; we do not build storage or dashboards for them.
- **N8. Active benchmarking infrastructure** (running controlled tasks across harnesses). Possibly a future adjacent module; out of scope for v1.
- **N9. Team dashboards, galleries, or hosted services.**
- **N10. Automatic application of changes.** Everything is a proposal the user approves.
- **N11. Telemetry or cloud features.** Nothing leaves the machine by default.

## 7. Positioning

**One line:** *Stop guessing which agent and model to use. Your own logs can tell you, and tell you if your last change helped.*

**Why use it instead of what exists**
1. **Neutral across tools and vendors.** First-party commands only see their own sessions and have limited incentive to recommend a competitor or a cheaper tier. A neutral tool can compare across them. (This is reasoning about incentives, not something verified.)
2. **A decision, not a report**: a pair × task-type table with confidence, not another cost chart.
3. **Evidence-backed diffs with before/after measurement.**
4. **Longer history** than harness retention allows.
5. **Small, local, auditable.**

**Relationship to the closest incumbent (AgentsView):** it is both a competitor and a possible foundation. Strategy options are in §12 (adapter strategy) and §18 (risks).

## 8. Core concepts and normalized schema

JSONL is the interchange; SQLite is the working store.

### 8.1 Entities
- **Session**: harness name and version, project/cwd, start and end.
- **Turn**: model id, input/output/cache-read/cache-write tokens, latency if available, stop reason, is_subagent, parent link.
- **ToolCall**: tool name, argument **hash** (not raw args), status, error class, output size, duration if available.
- **UserSignal**: classification only (new-task, correction, clarification, approval, abort) plus a hash; not raw text.
- **PermissionEvent**: asked, allowed, denied.
- **Pair**: the (harness, model, optional reasoning/effort setting) active for a turn.
- **Switch**: derived event when the pair changes within a session or across adjacent sessions on the same task.
- **Segment**: one task within a session (see §9.3).
- **Outcome**: per-segment proxy result (see §9.4).
- **Suggestion** and **Adoption**: proposals and the user's decisions with config hashes before and after.

### 8.2 Schema rules
- Every record has `schema_version`; changes are additive only.
- Store hashes, counts and classifications, **not raw prompts, file contents, or tool output**. A local-debug-only `--keep-raw` flag may exist.
- Field names follow OpenTelemetry GenAI conventions where they exist (e.g. `gen_ai.usage.*`, `gen_ai.tool.*`), with a project namespace for the rest, so output can be piped to other tooling. (Convention is still unstable; pin the version.)

## 9. Analysis

### 9.1 Principle
Counting and statistics are plain deterministic code. An LLM is optional and only ever sees **aggregated summaries**, never raw logs.

### 9.2 Metrics (per pair, per task type, per repo; each with n and confidence)
- Tokens by type; estimated cost from a versioned, user-editable pricing table (local models: time and tokens).
- Subscription quota burn where the harness exposes it.
- Tool-call error rate and invalid-argument rate.
- Retry and no-progress loop rate (same tool and arguments repeated, edits reverted).
- Re-read rate and oversized tool outputs.
- **First-turn context size** and state carried between turns.
- Correction rate, abort rate, permission-denial rate.
- Turns, tokens and tool calls **per successful segment**.
- Cache-hit ratio and cost impact around switches.

### 9.3 Task segmentation and labeling
- Segment at new-task signals or long idle gaps.
- Task types (v1 heuristics): debugging/fix, refactor, new feature, test writing, exploration/explanation, config/devops, docs. Inferred from tool-usage mix, file types touched and locally matched keywords.
- Optional v1.1 LLM labeler on **redacted summaries** only; cached; never required.
- Low-confidence labels are excluded from comparisons, not guessed.

### 9.4 Outcome signals (the hard part)
Success is not directly observable. Combine weak signals and call it a **proxy**:
- Positive: tests or build exit 0 near segment end; git commit or merged PR (where available); user continues or approves; no revert; no correction.
- Negative: failing last test run; correction right after completion; reverted or re-edited files; abort; repeated re-ask.
- Each segment gets `likely_success | likely_failure | unknown`, with contributing signals stored for audit.
- Reports always say "proxy success rate."

### 9.5 Selection bias (the credibility risk)
People pick stronger pairs for harder tasks, so naive comparisons mislead. Mitigations:
- Stratify by task type, session size and repo before comparing.
- Prefer **matched comparisons**; report intervals (e.g. Wilson or bootstrap).
- Minimum n per cell (default 20, configurable); below it, "insufficient data."
- Highlight cases where the user alternated pairs on similar work as the strongest evidence.
- Every recommendation includes "what would change this conclusion."

### 9.6 Switch analysis
For each switch, compare the following N turns to a baseline: cache-read drop, token spike, correction within K turns. Aggregate to "switching from A to B within a session tends to cost X." **Assumption to verify:** that provider-side prompt caching is lost on switch and how large the effect is.

## 10. Suggestions (diff proposals)

### 10.1 Types (v1)
1. Default pair (harness or model) for a task type or project.
2. Subagent model assignment.
3. Pair-scoped instruction (added only where it targets a pair that demonstrably ignores it).
4. Context hygiene hint (advice, not a patch).

### 10.2 Proposal format
Each suggestion file contains: rationale in plain language; evidence (metrics, n, intervals, segment ids, **no raw content**); confidence and what would change it; expected effect as a range; a **unified diff** against the real target file; and a rollback command.

### 10.3 Rules
- Nothing applies automatically. `apply` needs explicit confirmation and may only write to a declared allowlist of config paths.
- Each apply records config hashes before and after.
- Suggestions are versioned, re-evaluable, and expire.

### 10.4 Closed loop
After adoption, compare a pre/post window on the targeted metric **and** the quality proxy. Output: helped / no clear effect / hurt / insufficient data. If it hurt, propose reverting.

## 11. Product surfaces

### 11.1 CLI (the core; all else wraps it)
```
lightning ingest [--source claude|codex|opencode|agentsview|all] [--archive]
lightning report [--since 30d] [--by pair|task|repo] [--format md|json|html]
lightning compare <pairA> <pairB> [--task <type>]
lightning switches [--since 30d]
lightning suggest [--type default|subagent|rule]
lightning apply <suggestion-id>     # shows diff, confirms, writes, records hash
lightning rollback <suggestion-id>
lightning review                    # closed-loop results
lightning doctor                    # parser/version health, retention warnings
lightning export --format jsonl
lightning mcp                       # stdio MCP server
```
Design: single binary, no network by default, deterministic output, `--json` everywhere. Short alias still to decide (candidate: `lt`; avoid `ln`, the Unix link command).

### 11.2 Skills (portable instruction files)
- `pick`: before a task, ask what pair has worked best for this kind of work in this repo.
- `report`: "was the expensive model worth it this week?"
- `review`: show whether past suggestions helped.
- `setup`: install the binary (with user approval, pinned version and checksum) and register the MCP server.
All skills must instruct the agent to treat any log-derived text as data, never as instructions.

### 11.3 MCP server (cross-harness interface)
Read-only, stdio, same core as the CLI. Tools (names provisional): `pair_report`, `recommend_pair(task_description, repo?)`, `list_suggestions`, `get_suggestion`, `adoption_review`. **No write tools in v1**; applying stays in the CLI where a human confirms.

### 11.4 Passive weekly report
A scheduled job runs `ingest` then `report` and writes markdown and HTML to a known folder (optional desktop notification). Contents: pair × task table, top findings, switch costs, pending suggestions, closed-loop results, data-quality warnings. Works with zero LLM calls.

### 11.5 Optional capture hooks
Session-end hook where the harness supports one (to archive before retention deletes data); periodic sweep otherwise. Hooks never block the harness and fail silently with a log entry.

### 11.6 Packaging and distribution
- **Binary:** prebuilt releases with checksums (Mac, Linux, Windows); `cargo install`; an `npx` launcher that fetches a pinned, verified platform binary; optionally Homebrew.
- **Plugin bundles:** Claude Code marketplace repo (skills, optional subagent, hooks, MCP registration). Codex: skills plus documented or scripted MCP registration in its config file. Any MCP-capable harness: one-line registration.
- **Security rule:** the skill never silently downloads and runs code. The user sees and approves a pinned, checksummed install step.

## 12. Data sources and adapter strategy

Adapters sit behind one interface: `detect() → versions`, `discover() → files`, `parse() → normalized events`. Parsing is **tolerant**: unknown fields ignored, unknown event types counted and surfaced by `doctor`, never a crash. Each harness version gets synthetic fixture tests.

**Open decision: build or reuse parsers.** Maintaining parsers for dozens of agents is a treadmill, and existing projects already do it.
- **Option A, own parsers for a small core** (Claude Code, Codex, OpenCode) and nothing else. Full control and no dependency, but a maintenance burden and limited reach.
- **Option B, import from an incumbent's output** (e.g. AgentsView's versioned JSON or its database) for the long tail, with own parsers only where a signal we need is missing. Much wider coverage, but dependence on another project's schema stability.
- **Recommended starting point:** Option A for the core three harnesses to prove the method; evaluate Option B before adding more sources. **Check before committing:** the incumbent's schema stability, license compatibility, and whether it exposes the fields we need (model id, cache tokens, tool results, timestamps).

Other rules:
- **Retention:** `ingest --archive` normalizes and stores logs before the harness deletes them; `doctor` warns and shows the relevant setting.
- **Subagent transcripts** are ingested and tagged with a parent link.
- Every adapter is best-effort with a visible health status.
- Later adapters: Hermes, OpenClaw and other general agents (different session shapes; lower priority).

## 13. Privacy and security
- **Local-only by default.** No telemetry and no network in core paths. Any optional LLM use is opt-in and sends aggregates only.
- **Metadata over content.** No raw prompts or tool outputs persisted.
- **Redaction** on anything that could leave the aggregate layer (secret patterns, environment-like strings, normalized paths).
- **Prompt injection.** Logs contain untrusted text. Raw transcript content never reaches an LLM, skills tell agents to treat log text as data, and suggestions cannot contain free text copied from logs.
- **Write boundary.** `apply` can only modify allowlisted config paths, always shows a diff, and records hashes; no shell execution from suggestion content.
- **Supply chain.** Pinned dependencies, published checksums, small auditable binary, no silent downloads.

## 14. Technical approach
- **Language:** Rust for the core (single static binary, fast JSONL parsing, easy cross-platform releases). A TypeScript implementation would simplify the npx path but enlarge the footprint; the schema is the contract either way.
- **Storage:** SQLite working store; JSONL export; versioned migrations.
- **Statistics:** simple, explainable methods (stratified rates, Wilson or bootstrap intervals). No opaque models.
- **Pricing:** versioned, user-overridable table; `doctor` warns when stale.
- **MCP:** stdio server over the same library as the CLI (one core, three front ends).
- **Testing:** golden tests on fixtures; fuzz tests for malformed lines; a **synthetic-session generator with planted ground truth** (a planted "pair B ignores rule X" effect must be detected; a planted null must not yield a recommendation).
- **Footprint budget (targets, not measurements):** small single binary, no daemon in v1, near-zero idle cost, ingest of thousands of sessions in minutes, reports in seconds.

## 15. Validation plan (gate before building beyond ingest and report)

### 15.1 One-week feasibility experiment
1. Build only `ingest` and `report` for the builder's own logs (Claude Code first, then Codex if available).
2. Hand-label 30–50 segments by task type and outcome.
3. Compare pairs per task type; measure agreement between heuristics and hand labels.
4. Run the closest incumbents (AgentsView, ccusage, `/insights`) on the same data and write down which questions they cannot answer.

### 15.2 Go / no-go (starting thresholds, to be tuned)
- Task-type heuristics agree with hand labels ≥ ~80%.
- Outcome proxy agrees with hand-judged outcome ≥ ~75%.
- At least one cell shows a pair difference that is large, stable across weeks and survives stratification.
- At least one finding the builder did not already know and would act on.
- If pairs are indistinguishable or the proxy is too noisy: stop, or narrow to the cost and waste diagnostics in §6 item 6.

### 15.3 Re-verify before any public positioning
- That no widely adopted tool already provides outcome-based pair comparison (searches so far were limited).
- Current log field names and availability in each harness version.
- Prompt-cache behavior on model and harness switches in current provider documentation.
- Current plugin, marketplace and MCP registration mechanics for each harness.
- Survey figures, star counts and tool feature sets (they change quickly).

## 16. Roadmap

| Milestone | Scope | Gate |
|---|---|---|
| M0 | Schema, Claude Code adapter, `ingest`, `report` (cost, tool errors, re-reads, first-turn context, retries) | Parsing health on real data; §15.2 |
| M1 | Codex adapter; task segmentation; outcome proxy; pair comparison with intervals; `doctor`; archive | Labeled-set agreement met; schema holds across two harnesses without hacks |
| M2 | OpenCode adapter; `switches`; passive weekly report; decision on reuse vs own parsers | Cross-harness schema stable |
| M3 | `suggest`, `apply`, `rollback`; closed-loop `review` | Planted-effect tests pass; no false recommendations on null data |
| M4 | Skills; read-only MCP server; plugin bundles; npx launcher and release pipeline | Works end to end in a clean environment |
| M5 | Optional LLM labeler and narrative summary over aggregates; local-model metrics | Beats heuristics on the labeled set |
| Later | Hermes/OpenClaw adapters; team aggregation; optional active-benchmark module | Real demand |

## 17. Success metrics
- The builder uses the weekly report and acts on at least one suggestion per month.
- Adopted suggestions show lower cost without a drop in the quality proxy (closed loop).
- Harness updates that break parsing are caught by `doctor` before they produce wrong numbers.
- External adoption (stars, installs, issues) is tracked only after the above; it is not the first target.

## 18. Risks and open questions

| Risk | Mitigation |
|---|---|
| An incumbent (most plausibly AgentsView) adds pair comparison | Stay narrow on outcomes, confidence and closed-loop; consider integrating or contributing rather than competing |
| Selection bias makes comparisons misleading | Stratification, matched comparisons, n thresholds, "insufficient data" |
| Outcome proxy too noisy | Report as proxy; fall back to diagnostics only |
| Log formats change without notice | Tolerant parsing, fixtures per version, `doctor` |
| Retention deletes data before analysis | Archive on hook or schedule; warn in `doctor` |
| Prompt injection via logs | Metadata-only analysis; no raw content to LLMs |
| Small sample size for a single user | Clear thresholds; pooled anonymized sharing is post-v1 |
| First-party tools absorb the feature | Neutrality across vendors is the durable difference |
| Scope creep into viewers, dashboards, memory or skills | §6 non-goals are binding for v1 |
| Dependence on another project's schema (if reusing parsers) | Version-pin; keep own parsers for the core three |

### Open questions
1. Own parsers vs importing from an incumbent for the long tail (§12).
2. Should the unit include reasoning/effort settings as a third dimension of the pair?
3. `recommend_pair`: heuristic-only in v1, or wait for the labeler?
4. Should the tool ever write harness config directly, or only emit patch files?
5. How should local-model "cost" be defined (wall-clock, tokens, memory-seconds)?
6. Which outcome signals are reliably present in every supported harness's logs?
7. Should the normalized schema be published as an open spec so other tools can adopt it?
8. **Name:** Lightning (chosen for simplicity and to signal speed and low overhead). Known collisions to handle: the plain `lightning` package name is already taken on npm (a Lightning Network client library) and crates.io; `lightning-cli` is taken on npm (and, from memory, is also the command name of a Bitcoin Lightning node's CLI); `lightning-ai` is held on npm as a security placeholder (from memory, Lightning AI / PyTorch Lightning is a well-known AI brand, which will crowd search results). Mitigations to decide: publish under a scoped or alternative package name while keeping `lightning` as the command, and check GitHub, domains and trademarks. Avoid `ln` as a short alias (it is the Unix link command).

## 19. Sources

Surveys and market data
- JetBrains, "AI Coding Agents: Adoption Trends," Developer Ecosystem Survey 2026 — https://blog.jetbrains.com/research/2026/08/ai-coding-agent-adoption-2026/
- Stack Overflow Developer Survey 2026, AI coding agents — https://survey.stackoverflow.co/2026/ai/data/ai-code-agent
- The Pragmatic Engineer, "AI Tooling for Software Engineers in 2026" — https://newsletter.pragmaticengineer.com/p/ai-tooling-2026

Harnesses
- Open-source CLI coding agents roundup (Pinggy) — https://pinggy.io/blog/best_open_source_cli_coding_agents/
- OpenClaw overview (MintMCP, via Plushcap) — https://www.plushcap.com/content/mintmcp/blog/mintmcp-what-is-openclaw-the-complete-guide-to-the-open-source-agent-harness
- Hermes Agent tutorial (DataCamp) — https://www.datacamp.com/tutorial/hermes-agent

Existing tools
- AgentsView — https://github.com/wesm/agentsview
- Agent Sessions — https://github.com/jazzyalex/agent-sessions
- session-analytics — https://github.com/rokas-tarasevicius/session-analytics
- AI Session Analyzer — https://github.com/fmicalizzi/AI-Session-Analyzer
- agent-session-otel — https://pypi.org/project/agent-session-otel/
- ccusage (multi-harness cost reports; stars per third-party tracker) — https://gittrend.io/repo/ccusage/ccusage
- Claude Code `/insights` guides — https://angelo-lima.fr/en/claude-code-insights-command/ , https://pasqualepillitteri.it/en/news/408/claude-code-insights-workflow-command
- Claude Code auto memory and consolidation — https://www.implicator.ai/anthropic-adds-auto-dream-to-claude-code-fixing-memory-decay-between-sessions
- Codex memories — https://mem0.ai/blog/how-memory-works-in-codex-cli
- Skill-mining tool example — https://pypi.org/project/aiskill/
- OTel export plugin example — https://skillselion.com/marketplace/whizzzkid/claude-otel
- Codex rollout/session layout — https://github.com/drmikecrowe/harnessed/wiki/home-folder-codex-requirements
- Session-log parsing projects and retention note — https://github.com/plinde/skillscope

Evidence on harness differences, workflows and pain points
- "I benchmarked 14 AI coding agents on the same task" — https://dev.to/cuculhart/i-benchmarked-14-ai-coding-agents-on-the-same-task-same-model-wildly-different-results-16o8
- "Same model, 78 vs 42" (paywalled; claim only) — https://natesnewsletter.substack.com/p/same-model-78-vs-42-the-harness-made
- "Benchmark it on your own tasks" (active benchmarking) — https://schristoph.online/blog/benchmarking-coding-agents
- Running Claude Code and Codex together — https://dev.to/prithvi_bharadwaj_d1c875e/running-claude-code-and-codex-together-instead-of-choosing-one-3pa8
- Claude Code vs Codex pain points (quota, context, cost visibility) — https://dev.to/lars_winstand/i-thought-claude-code-vs-codex-was-about-model-iq-until-i-watched-one-prompt-eat-53-of-a-session-208p
- Static routing skill example — https://claudskills.com/skills/routing-work-across-ai-harnesses/SKILL.md

## 20. Appendix

### A. Mock weekly report
```
Lightning — week of 2026-11-02
Data: 168 segments, 3 repos, 2 harnesses. Parser health: OK.

Pair × task (proxy success, cost/segment, n)
                          debugging         refactor          test-writing
Harness A + Model X       92% $0.84 (n=21)  88% $1.40 (n=14)  insufficient (n=6)
Harness A + Model Y       89% $0.31 (n=48)  76% $0.62 (n=19)  91% $0.22 (n=24)
Harness B + Model Z       90% $0.29 (n=33)  insufficient (n=8) 88% $0.20 (n=27)

Findings
1. Debugging: Pair A+Y is within 3 pts of A+X at ~37% of the cost (medium confidence).
2. Test-writing: A+Y and B+Z are indistinguishable within intervals (no clear difference).
3. 9 mid-session switches cost extra cache-read tokens each (low confidence).

Pending suggestions: 2 · Closed loop: "default test-writing to A+Y" → helped.
```

### B. Mock suggestion
```
id: sug-0007   type: default-pair   confidence: medium
Rationale: For test-writing (n=24 vs n=6), A+Y matches A+X proxy success at ~26% of the cost.
Would change if: the A+X sample grows and shows a consistent >10-point success edge.
Expected effect: lower weekly cost (range); quality proxy unchanged.
--- a/<config path>
+++ b/<config path>
@@ ... @@
-  "model": "X"
+  "model": "Y"
Rollback: lightning rollback sug-0007
```

### C. Mock MCP contract
```
recommend_pair(task_description: string, repo?: string) -> {
  task_type: string, task_type_confidence: number,
  ranking: [{ harness, model, proxy_success, cost_per_segment, n, confidence }],
  verdict: "recommend" | "insufficient_data",
  caveats: [string]
}
```

### D. Glossary
- **Pair**: a harness plus a model (optionally a reasoning/effort setting). **Segment**: one task within a session. **Proxy success**: composite weak-signal estimate of outcome. **Stratify**: compare like with like. **Closed loop**: measuring whether an adopted change helped. **Harness**: the agent runtime around the model (tools, prompts, memory, loop).
