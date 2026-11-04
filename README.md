# Lightning

**Stop guessing which agent and model to use. Your own logs can tell you, and tell you whether your last change helped.**

Lightning reads the session logs your coding agents already write (Claude Code, Codex, OpenCode), normalizes them locally, and compares **harness + model pairs** per kind of work: proxy success with intervals, cost per task, sample sizes, and an explicit "insufficient data" when the evidence is thin. It proposes small config changes as diffs you approve, and later checks whether they helped.

It is one small binary. It makes no network calls and sends no telemetry, and it stores no prompts, file contents or tool output. Only hashes, counts and classes are kept.

## Install

```bash
cargo install lightning-ledger --locked
```

Or, once a release is published: `npx lightning-ledger@<version> <command>`. The npm launcher downloads that release's binary and verifies its SHA-256 against the checksums shipped in the package before running it. To build from source, clone this repository and run `cargo install --path .`.

The package is called `lightning-ledger` because `lightning` is taken on crates.io and npm. The command is `lightning`.

## Quick start

```bash
lightning ingest      # read every harness's logs it can find (read-only)
lightning doctor      # parser health per harness, retention and pricing warnings
lightning report      # pair x task table with findings
```

Costs show in tokens until you add prices. Copy [pricing/default.toml](pricing/default.toml) to `<data dir>/lightning/pricing.toml` (`%LOCALAPPDATA%\lightning` on Windows, `~/.local/share/lightning` elsewhere) and fill in prices from each provider's pricing page. Lightning never guesses a price.

## What it answers

| Question | Command |
|---|---|
| Which pair should I use for this task? | `lightning pick "fix the flaky login test"` (`--repo <name>`, `--json`) |
| Which pair works best for each kind of task? | `lightning report` (`--by pair`, `--by pair+effort`, `--format md\|json\|html`) |
| Is the expensive model worth it for debugging? | `lightning compare claude-opus-5 claude-sonnet-5 --task debugging` |
| What do mid-session model switches cost me? | `lightning switches` |
| What should I change? | `lightning suggest`, then `lightning suggest --show <id>` |
| Make the change (shows the diff, asks first) | `lightning apply <id> [--repo <path>]`, undo with `lightning rollback <id>` |
| Did the change help? | `lightning review` |
| Spend by model, repo or session | `lightning report --by model\|repo\|session` |
| A weekly summary (md + html) | `lightning weekly` |
| Everything as JSONL | `lightning export` |

Every command takes `--db`, `--pricing` and `--min-n` (decided segments per cell before comparing; default 20).

## How it decides

- **Segments.** A segment is one task inside a session. It starts at a new-task prompt or after a 30-minute gap. Each segment gets a task type: debugging, refactor, feature, test writing, exploration, config/devops or docs. The type comes from prompt keywords and the mix of tools used, and is classified at ingest time; then the text is dropped. Low-confidence labels are `unknown` and are left out of comparisons.
- **Proxy success.** `likely_success`, `likely_failure` or `unknown`, from weak signals:
  - the last test or build result
  - commits and reverts
  - aborts
  - corrections during the task or right after it
  - whether you moved on

  Reports always say *proxy*.
- **Comparisons.** Pairs are compared within a task type. Samples are stratified by repo and segment size, so that one pair's harder work doesn't make it look worse. A cheaper pair B is recommended over A only when:
  - both have at least 20 decided segments
  - B is no more than 10 points worse on proxy success (one-sided 5%)
  - B credibly costs under 90% of A

  Anything else is "no clear difference" or "insufficient data". Every verdict also says what would change it.
- **Closed loop.** `review` compares the 30 days before an adoption with the period since. The verdict is helped, no clear effect, hurt or insufficient data. When a change hurt, `review` points to `rollback`.

These heuristics are checked by synthetic sessions with planted ground truth: a planted effect must be found, and a planted null must never produce a recommendation. To check them against your own data, which is the go/no-go gate in [the spec](lightning-spec.md) §15:

```bash
lightning label next --sample 40   # shows each segment's opening prompt (read live, never stored)
lightning label agreement          # task agreement >= 80%, outcome agreement >= 75%?
```

## Suggestions and the write boundary

There are four kinds of suggestion:
- **default:** a cheaper model for a repo (`.claude/settings.json` or `.codex/config.toml`), plus per-task routing advice
- **subagent:** a cheaper model for a Claude Code subagent type (`.claude/agents/<type>.md`)
- **rule:** a fixed-template instruction scoped to a model that re-reads, retries, errors or hits denials far more than your other pairs (`~/.claude/CLAUDE.md`, `~/.codex/AGENTS.md`)
- **hygiene:** advice when a repo's sessions start with far more context than your others

`apply` writes only to an allowlist of config paths. It always shows a unified diff and asks before writing (unless you pass `--yes`), and it records the file's hash and content before and after. `rollback` restores that content, and refuses if you have edited the file since unless you pass `--force`. Suggestions never contain text copied from logs.

## Agents, MCP and skills

- **Claude Code:** add this repository as a plugin marketplace and install the `lightning` plugin. It brings the `pick`, `report`, `review`, `setup` and `doctor` skills, registers the read-only MCP server, and adds a session-end hook. The hook runs `lightning ingest --quiet` to archive metadata before retention deletes the transcript.
- **Codex:** add to `~/.codex/config.toml`:
  ```toml
  [mcp_servers.lightning]
  command = "lightning"
  args = ["mcp"]
  ```
  To use the skills, copy `plugin/skills/*` into your Codex skills folder.
- **Any MCP client:** `lightning mcp` speaks JSON-RPC over stdio. It exposes `pair_report`, `recommend_pair`, `list_suggestions`, `get_suggestion` and `adoption_review`, and has no write tools.

## Weekly report

`lightning weekly` ingests everything, then writes `weekly-<monday>.md` and `.html` to `<data dir>/lightning/reports` (or `--out`). Lightning never registers a scheduled task itself. To run it every Monday:

- **Windows (Task Scheduler):** `schtasks /Create /SC WEEKLY /D MON /ST 09:00 /TN Lightning /TR "lightning weekly"`
- **Linux (cron):** `0 9 * * 1 lightning weekly`
- **macOS:** a launchd agent with `StartCalendarInterval` (Weekday 1, Hour 9) running `lightning weekly`

## Data and privacy

| Harness | Read from | Notes |
|---|---|---|
| Claude Code | `~/.claude/projects/**` (`LIGHTNING_CLAUDE_ROOT`) | Subagent transcripts are linked to their parent. Deleted after `cleanupPeriodDays` (30 by default): `doctor` warns. |
| Codex | `~/.codex/sessions/**/rollout-*.jsonl` (`CODEX_HOME`, `LIGHTNING_CODEX_ROOT`) | Codex doesn't log cache writes or stop reasons. |
| OpenCode | `~/.local/share/opencode/opencode.db` (`LIGHTNING_OPENCODE_ROOT`) | Opened read-only. The newer `session_v2` layout is detected and reported, not yet parsed. |

Transcripts are only ever opened for reading, and the database is refused if it would sit inside a log directory. Rows survive after the harness deletes its transcripts. The normalized schema and the export format are documented in [docs/schema.md](docs/schema.md).

## Status

M0 to M4 of the [spec](lightning-spec.md) are implemented. M5 (an optional LLM labeler) is deliberately not built, and the core makes no network calls. The heuristics haven't yet passed the real-data gate (§15.2): run `lightning label` on your own sessions before trusting a recommendation.

## License

MIT
