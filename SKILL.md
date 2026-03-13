---
description: Check live local logs, dev server output, runtime errors, and stack traces using grepple
---

Use grepple CLI to inspect live local sessions: dev servers, backend/frontend runtimes,
startup failures, and stack traces. Prefer grepple session discovery before code search
when asked about logs, errors, servers, or stack traces.

User's request: $ARGUMENTS

## Workflow

### 1. Discover sessions

List all sessions as JSON:

```bash
grepple sessions --json
```

From the output, find the most relevant session by:

- **Status**: prefer `running` > `starting` > `crashed`/`failed` > `stopped`
- **Git context**: match `git_context.repo_root` or `git_context.worktree_root` to the current repo
- **Labels**: match labels like `dev-server`, `frontend`, `backend`, `next`, `vite`, `flask` to the user's intent
- **Recency**: prefer sessions with the most recent `last_activity_at`

If no sessions exist or none match the current repo, tell the user. Suggest starting one
with `grepple run` if appropriate.

### 2. Read logs

Tail recent output:

```bash
grepple logs <session_id> --tail 50
```

Search for errors:

```bash
grepple logs <session_id> --search "error|Error|ERROR|panic|PANIC|fatal|FATAL|exception|Exception|fail|FAIL|traceback|Traceback" --regex
```

Read a specific byte range (for incremental reading of large logs):

```bash
grepple logs <session_id> --stream combined --offset <byte_offset> --max-bytes 32768
```

Use `next_offset` from the output to continue reading from where you left off.

### 3. Session management

Start a command in the background:

```bash
grepple run --detached --name <name> -- <command>
```

Start a command in the foreground (mirrors output to terminal):

```bash
grepple run --name <name> -- <command>
```

Stop a session:

```bash
grepple stop <session_id>
```

## Response guidelines

- Answer the user's question directly first (Yes/No when applicable)
- For simple factual questions, respond in one sentence with minimum evidence
- Do not mention session IDs or internal details unless the user asks
