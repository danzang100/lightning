# Model Ledger M0: running on your real logs (L8, not yet done)

Status: not run. Everything so far was tested on synthetic fixtures only.

`ledger` only ever opens transcripts read-only and refuses a database path inside the transcript root. To be extra safe, work on a copy:

```powershell
robocopy "$env:USERPROFILE\.claude\projects" "$env:USERPROFILE\ledger-snapshot" /E /COPY:D /R:1 /W:1
attrib +R "$env:USERPROFILE\ledger-snapshot\*" /S
cargo run -p ledger --release -- ingest --root "$env:USERPROFILE\ledger-snapshot"
cargo run -p ledger --release -- doctor
cargo run -p ledger --release -- report --by model
```

Before the report is useful, copy `crates/ledger/pricing/default.toml` to `%LOCALAPPDATA%\ledger\pricing.toml` and fill the prices from the provider's pricing page. Unpriced models show `unpriced`.

After the run, record counts, timing and any parse problems here (no private content).

## Run of 2026-10-13 (read-only snapshot, 91 transcript files)

- Ingest: 91 files, 44 sessions, 9,319 turns, 6,305 tool calls, 0 malformed lines, 0 partial tails, 7.4 s (release build). Second run: 0 parsed, 91 unchanged, 33 ms.
- Doctor: all 14 Claude Code versions (2.1.246 to 2.1.295) have 100% usage/cache fields; stop_reason is 80% on 2.1.281. Report tool-call total (6,305) matches the database.
- Models seen: claude-opus-5, claude-opus-5-5, claude-sonnet-5, claude-sonnet-5-5, claude-haiku-4-5-20251001 (not in the shipped price table). All unpriced until prices are filled in.
- Source logs were copied with robocopy and the copy marked read-only; the database was written to the temp directory.

## Lightning 0.2 run of 2026-11-04 (live logs, read-only; database in a temp directory)

- Ingest, all harnesses: 112 files (Claude Code and Codex; no OpenCode database present), 65 sessions, 7,130 turns (claude_code 6,712, codex 418), 8,153 tool calls, 0 malformed lines, 1 partial tail, about 3 s (release build).
- Codex 0.123 and 0.130 rollouts: usage and cache-read fields 100%; cache writes and stop reasons are not logged by Codex.
- Segments: 542. Task types: 25% unknown, then exploration, feature, debugging, test writing, docs, config/devops, refactor. Proxy outcome: 61% likely success, 12% likely failure, 27% unknown. One mixed-pair segment.
- Comparisons: only Claude Code Opus 5 and Opus 5.5 reach n >= 20 in any task type (feature); they are indistinguishable within intervals. Codex pairs are all below n = 20.
- Suggestions at min n 10: none clear the evidence bar. (A first version flagged context hygiene everywhere because it measured mid-session segments; fixed to measure only the segment that opens a session, relative to the user's other sessions in the same harness.)
- No prices are configured yet, so all costs are in tokens.
- The real-data gate (spec section 15.2) has not been run: no segments are hand-labeled yet.
