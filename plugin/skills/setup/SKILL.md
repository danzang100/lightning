---
name: setup
description: Use when the `lightning` binary is missing or the user asks to install or set up Lightning, register its MCP server, or schedule its weekly report.
---

# Set up Lightning

Installing software is the user's decision. Show each command, say what it does, and run it only after the user says yes.

1. Check: `lightning --version`. If it works, skip to step 3.
2. Offer one install path, pinned to a version:
   - With Rust: `cargo install lightning-ledger --version <version> --locked`
   - With Node: `npx lightning-ledger@<version> --version` (the launcher downloads the matching release binary, checks its SHA-256 against the checksums shipped in the package, and prints what it fetched)
   - From source: clone https://github.com/danzang100/lightning and run `cargo install --path .`
3. First run: `lightning ingest`, then `lightning doctor`. Explain the warnings it prints.
4. Prices: costs show as tokens until the user copies `pricing/default.toml` to `<data dir>/lightning/pricing.toml` and fills in prices from each provider's pricing page. Do not fill in prices yourself.
5. MCP (optional): Claude Code gets it with this plugin. For Codex, add to `~/.codex/config.toml`:
   ```toml
   [mcp_servers.lightning]
   command = "lightning"
   args = ["mcp"]
   ```
6. Weekly report (optional): show the user how to schedule `lightning weekly` with their OS scheduler (see the README). Lightning never registers a scheduled task itself.

## Rules

- Never download or run anything without the user's explicit yes, and never an unpinned version.
- Never change harness settings beyond what the user approved.
