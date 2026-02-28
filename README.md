# session-recovery

Recover file history from OpenClaw session logs as git commits.

Inspired by [claude-file-recovery](https://github.com/hjtenklooster/claude-file-recovery).

## Overview

When you work with AI coding assistants like Claude through OpenClaw, every file operation (write, edit) is logged in session files. This tool extracts those operations and reconstructs them as a git branch with:

- **Proper timestamps** from the session log
- **Author attribution** based on the AI model used
- **Deterministic commits** — same inputs always produce same commit hashes
- **Full history** of how files evolved during a session

## Installation

```bash
cargo install --path crates/session-recovery
```

Or build from the jeb monorepo:

```bash
cargo build --release -p session-recovery
```

## Quick Start

```bash
# Recover all operations from a specific session
session-recovery ~/.openclaw/agents/main/sessions/abc123.jsonl

# Auto-discover sessions that touched specific files
session-recovery --scan-sessions --include "**/my-project/**"

# Point-in-time recovery: get a file as it was at a specific time
session-recovery --at "src/main.rs@2026-02-20T07:30:00Z"

# Recover only files inside the current repo
session-recovery --scan-sessions --include "**/*.rs" --ignore-external
```

## Features

### Path Filtering

```bash
--include <glob>      # Only include files matching pattern (repeatable)
--exclude <glob>      # Exclude files matching pattern (repeatable)
--ignore-external     # Skip files outside the repository
```

### Automatic Session Discovery

```bash
--scan-sessions       # Find sessions automatically
--sessions-dir <path> # Where to look (default: ~/.openclaw/agents/main/sessions/)
--since <time>        # Only sessions after this time
--until <time>        # Only sessions before this time
```

### Point-in-Time Recovery

```bash
--at <path>@<timestamp>  # Recover file to specific point in time
--lookback <duration>    # How far back to search (default: 14d)
```

### Commit Collapsing

By default, consecutive additive operations (no deletions) without user interaction are collapsed into single commits. Disable with `--no-collapse`.

## How It Works

1. **Extract** — Parse session .jsonl files for write/edit operations
2. **Filter** — Apply include/exclude patterns
3. **Order** — Sort operations chronologically (interleaving multiple sessions)
4. **Commit** — Create git commits for each operation
5. **Merge** — Leave repo in uncommitted merge state for review

### Session Markers

Each session creates orphan commits as markers:
- "Beginning recovery from OpenClaw session <id>"
- "Completing recovery from OpenClaw session <id>"

This ensures the same initial commit hash exists across all recoveries of that session.

### Failed Edits

When an edit can't find its target text, the new content is appended to the file (with 3 blank lines separator) and the commit message is prefixed with ⚠️.

### External Files

Files outside the repository are mapped using `_../` encoding:
```
/other/project/file.txt → _../other/project/file.txt
```

## Example: Recovering Your Own Tool's History

```bash
# Create standalone repo with just this tool's history
mkdir session-recovery-standalone && cd session-recovery-standalone
git init && git commit --allow-empty -m "Initial"

# Recover the tool's development history
session-recovery --scan-sessions --include "**/session-recovery/**" --ignore-external

# Complete the merge
git commit
```

## CLI Reference

```
session-recovery [OPTIONS] [SESSIONS...]

Arguments:
  [SESSIONS...]  Session .jsonl files (optional if --scan-sessions or --at)

Options:
      --repo <PATH>         Target repository [default: .]
      --branch <NAME>       Recovery branch name
      --include <GLOB>      Include files matching pattern
      --exclude <GLOB>      Exclude files matching pattern
      --ignore-external     Skip files outside repo
      --scan-sessions       Auto-discover sessions
      --sessions-dir <PATH> Session directory [default: ~/.openclaw/agents/main/sessions/]
      --since <TIME>        Start of time range
      --until <TIME>        End of time range
      --at <PATH@TIME>      Point-in-time recovery
      --lookback <DUR>      Lookback for --at [default: 14d]
      --no-collapse         Don't collapse commits
      --dry-run             Show what would be done
      --list-only           List operations only
  -v, --verbose             Verbose output
  -h, --help                Print help
```

## Design

See [DESIGN.md](DESIGN.md) for the full specification.

## License

MIT
