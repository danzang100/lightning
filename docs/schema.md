# Lightning normalized schema (schema_version 2)

JSONL is the interchange; SQLite is the working store. Changes are additive only: a new field never changes the meaning of an old one. Records carry `schema_version`.

**Nothing raw is stored.** Prompts, commands, file paths and tool output are classified at ingest and then dropped. What remains are salted hashes (16 hex chars of SHA-256 with a per-database salt), counts and classes. Repo names are kept only as the last path component.

## Records

### session
| field (export name) | meaning |
|---|---|
| `gen_ai.conversation.id` | harness session id |
| `lightning.agent_id` | subagent id, empty for the main agent |
| `lightning.harness` | `claude_code`, `codex`, `opencode` |
| `lightning.harness_version` | harness version from the log |
| `lightning.repo` | last component of the working directory |
| `lightning.repo_hash` | salted hash of the normalized working directory |
| `lightning.start`, `lightning.end` | first and last timestamp (RFC 3339, UTC) |
| `gen_ai.agent.name` | subagent type, when known |

### turn (one API call)
| field | meaning |
|---|---|
| `lightning.turn_id` | salted hash, stable across resumed-session copies |
| `gen_ai.request.model` | model id as logged |
| `lightning.effort` | reasoning effort setting, when logged |
| `gen_ai.usage.input_tokens` | uncached input tokens (Codex's cached share is moved out) |
| `gen_ai.usage.output_tokens` | output tokens |
| `lightning.usage.cache_read_tokens`, `lightning.usage.cache_write_tokens`, `lightning.usage.reasoning_tokens` | as logged; `null` when the harness doesn't report them |
| `gen_ai.response.finish_reasons` | stop reason |
| `lightning.is_subagent`, `lightning.is_api_error`, `lightning.is_aborted` | flags |

### tool_call
| field | meaning |
|---|---|
| `gen_ai.tool.call.id` | tool call id (hashed for Codex and OpenCode) |
| `gen_ai.tool.name` | tool name as logged |
| `lightning.tool_kind` | `read`, `edit`, `shell`, `search`, `web`, `task`, `other` |
| `lightning.args_hash` | salted hash of canonical JSON arguments |
| `lightning.target_hash` | salted hash of the normalized target path (read/edit only) |
| `lightning.target_kind` | `test`, `doc`, `config`, `code`, `other` |
| `lightning.cmd_class` | shell only: `test`, `build`, `lint`, `git_commit`, `git_revert`, `git_push`, `install`, `other` |
| `lightning.status` | `ok`, `error`, `unknown` |
| `lightning.permission` | `allowed`, `denied`, `none` |

### user_signal (human prompts only)
| field | meaning |
|---|---|
| `lightning.class` | `new_task`, `correction`, `clarification`, `approval`, `abort`, `continue` |
| `lightning.task_hint` | task type suggested by keywords, if any |
| `lightning.text_hash` | salted hash of the trimmed text |

### segment (derived, recomputed on every run)
`segment_id`, `session_id`, `source`, `repo`, `repo_key`, `start`, `end`, `model`, `effort`, `mixed`, `task_type`, `task_confidence`, `outcome` (`likely_success | likely_failure | unknown`), `outcome_signals`, token totals, `cost` (null when any turn is unpriced), `tool_calls`, `tool_errors`, `denials`, `reads`, `rereads`, `retries`, `edits`, `corrections`, `first_turn_context` (opening segment of a session only), `wall_secs`.

## Conventions

`gen_ai.*` names follow the OpenTelemetry GenAI semantic conventions as of v1.37.0. Those conventions are still at development stability, so the version is pinned in the export header (`otel_genai_semconv`). Names without a settled convention live under `lightning.*`.

## Working store

SQLite, `PRAGMA user_version` 3. Tables: `files` (source fingerprints), `sessions`, `turns`, `tool_calls`, `signals`, `unknown_types`, `labels`, `suggestions`, `adoptions`, `meta` (salt, schema version). Migrations run on open.
