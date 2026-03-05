# Grepple

Grepple is a local session-centric terminal log observer for AI agents.

It provides:
- a CLI (`grepple`) for starting/attaching sessions and reading logs
- an MCP stdio server (`grepple mcp` or `grepple-mcp`) exposing structured tools
- first-party installer helpers for `codex`, `claude`, and `opencode`

## Build

```bash
cargo build
```

## Install

Default install (recommended):

```bash
cargo install --path .
```

This installs `grepple`, which already includes MCP mode via:

```bash
grepple mcp
```

Optional: also install dedicated `grepple-mcp` binary:

```bash
cargo install --path . --features with-mcp-bin
```

## Quick Start

Run a command under Grepple:

```bash
grepple run --name api -- pnpm dev
```

List sessions:

```bash
grepple sessions
```

Read logs incrementally:

```bash
grepple logs <session_id> --stream combined --offset 0 --max-bytes 32768
```

Search logs:

```bash
grepple logs <session_id> --search "error|panic" --regex
```

Stop a managed session:

```bash
grepple stop <session_id>
```

## Install MCP Into Clients

Codex:

```bash
grepple install codex
# equivalent dry-run
# grepple install codex --dry-run
```

Claude Code:

```bash
grepple install claude --scope user
```

OpenCode:

```bash
grepple install opencode --scope user
```

## MCP Entry Point

Run the server over stdio (default):

```bash
grepple mcp
```

Or, if installed with `with-mcp-bin`:

```bash
grepple-mcp
```

Grepple MCP exposes these core tools:
- `session_list`
- `session_status`
- `session_start_command`
- `session_attach`
- `session_stop`
- `log_read`
- `log_search`
- `log_tail`
- `log_stats`
- `install_client`

## Environment Variables

- `GREPPLE_STATE_DIR`: override state directory
- `GREPPLE_REDACT=0`: disable output redaction in `log_read/log_search/log_tail`
