# Grepple

Grepple is a local session-centric terminal log observer for AI agents.
It is designed to feel like the default tool for live local logs/processes: dev servers,
backend/frontend runtimes, startup failures, and stack traces.

It provides:
- a CLI (`grepple`) for starting/attaching sessions and reading logs
- a skill file (`SKILL.md`) for Claude Code — CLI-only, no MCP server needed
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

Start a live interactive shell session under Grepple:

```bash
grepple
```

Leave that shell the normal way with `exit` or `Ctrl-D`. If you have the shell helpers installed,
`grepple exit` and `grepple quit` also tear down shell jobs before exiting while you are inside a
Grepple shell.

Run an explicit command under Grepple:

```bash
grepple run --name api -- pnpm dev
```

By default, `grepple run` mirrors stdout/stderr to your terminal (like running the command directly)
while still capturing logs for Grepple tools. To start in background mode instead:

```bash
grepple run --detached --name api -- pnpm dev
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

Clear all locally stored session snapshots/logs:

```bash
grepple sessions clear --yes
```

## How Grepple Works

Grepple is built around sessions. A session is a local record of a shell, command, or attached
runtime, with metadata plus log files that Grepple and MCP tools can inspect later.

- `grepple` starts a new interactive shell session on a PTY. Grepple sits between your terminal
  and the child shell, forwards keyboard input to it, mirrors the rendered output back to your
  screen, and writes that transcript to the session logs.
- `grepple run -- <command>` starts a managed non-interactive process. Grepple captures
  `stdout`/`stderr`, mirrors them in the foreground by default, and can also keep the process in
  the background with `--detached`.
- `grepple attach` is the attach/import path for tmux panes. It captures pane output into a
  Grepple session so the same log-reading tools can inspect it.
- Each session is stored on disk under Grepple's state directory with `meta.json`, `stdout.log`,
  `stderr.log`, `combined.log`, and an event history. This gives MCP tools a stable local source
  of truth instead of depending on a still-open terminal window.
- Session ranking uses local context like cwd, git worktree, branch, running status, and command
  labels so agents can usually find the most relevant runtime for the repo they are working in.

In short: Grepple does not just tail random files. It creates or attaches to session sources,
stores structured local session state, and then lets the CLI and MCP layer read, rank, search,
and summarize those sessions.

## Shell Helpers

Generate shell helpers for a short alias (`g`) and run wrapper (`gr`):

```bash
grepple shell init zsh
grepple shell init fish
```

`g` maps to bare `grepple`, so it starts a live shell session. `gr` remains the explicit
`grepple run -- ...` wrapper for one-off commands. Inside a Grepple shell, the helper also makes
`grepple exit`, `grepple quit`, `g exit`, and `g quit` tear down shell jobs and resolve to exit.

Install for current shell session:

```bash
eval "$(grepple shell init zsh)"
```

Fish:

```fish
grepple shell init fish | source
```

Persist in shell config:

```bash
grepple shell init zsh >> ~/.zshrc
grepple shell init fish >> ~/.config/fish/config.fish
```

## CLI + Skill (Claude Code)

Instead of running grepple as an MCP server, you can use the CLI directly through a
Claude Code skill file. Claude runs `grepple` commands via Bash, guided by the skill
instructions. This is simpler to set up — no MCP server process, no stdio protocol.

Install the skill:

```bash
grepple install claude-skill --scope user
```

This writes the skill file to `~/.claude/commands/grepple.md`. Use `--scope project` to
install into the current project's `.claude/commands/` instead.

Use it in Claude Code:

```
/grepple check for errors on the dev server
/grepple show me the last 50 lines of the api logs
/grepple what's running in this repo
```

The skill teaches Claude how to discover sessions, read/search logs, and manage
processes using the same `grepple` CLI commands documented above.

## Install MCP Into Clients

Codex:

```bash
grepple install codex
# equivalent dry-run
# grepple install codex --dry-run
```

`grepple install codex` also sets `startup_timeout_sec = 30` for the installed MCP entry in `~/.codex/config.toml`.

By default, `install/add` uses a terminal UI with progress and success/failure state.
For machine-readable output, pass `--json`.

Claude Code:

```bash
grepple install claude --scope user
```

OpenCode:

```bash
grepple install opencode --scope user
```

`grepple install opencode` now also adds a small instruction file so OpenCode prefers
Grepple first for logs/errors/server/dev-server questions before broad code search.

## MCP Entry Point

Run the server over stdio (default):

```bash
grepple mcp
```

Or, if installed with `with-mcp-bin`:

```bash
grepple-mcp
```

## Troubleshooting Codex Startup Timeout

If Codex reports:

```text
MCP client for `grepple` timed out after 10 seconds
```

use a prebuilt binary command in your MCP config (`grepple mcp` or `grepple-mcp`), not `cargo run ...`, and ensure timeout is set (the installer now sets this automatically for Codex):

```toml
[mcp_servers.grepple]
startup_timeout_sec = 30
```

Grepple MCP exposes these core tools:
- `session_list`
- `session_status`
- `current_repo_sessions`
- `pick_best_session`
- `session_start_command`
- `session_attach`
- `session_stop`
- `log_read`
- `log_search`
- `log_tail`
- `log_stats`
- `log_error_counts`
- `session_preset`
- `install_client`

The high-level debugging helpers are:
- `pick_best_session`: resolve the most relevant running session for the current repo/worktree
- `current_repo_sessions`: ranked current-repo candidates with match reasons
- `log_error_counts`: first-class error counting with best-effort time-window support
- `session_preset`: one-shot presets for `recent_errors`, `startup_failures`, `watch_errors`, and `session_summary`

## Environment Variables

- `GREPPLE_STATE_DIR`: override state directory
- `GREPPLE_MAX_STATE_BYTES`: hard cap for total grepple state size in bytes (default `2147483648`; set `0` to disable cap)
- `GREPPLE_REDACT=0`: disable output redaction in `log_read/log_search/log_tail`
