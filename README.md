# session-recovery

Recover file history from OpenClaw session logs as git commits.

Inspired by [claude-file-recovery](https://github.com/hjtenklooster/claude-file-recovery).

## What It Does

When you work with AI coding assistants like Claude through OpenClaw, every file operation (write, edit) is logged. This tool extracts those operations and reconstructs them as git commits with:

- **Proper timestamps** from the session log
- **Author attribution** based on the AI model
- **Full history** of how files evolved

## Installation

```bash
cargo build --release -p session-recovery
```

## Quick Start

**Preview what would be recovered** (safe, no changes made):
```bash
session-recovery --scan-sessions --include "**/my-project/**"
```

**Actually apply the recovery** (requires `--confirm`):
```bash
session-recovery --scan-sessions --include "**/my-project/**" --confirm
```

## Usage

### Basic Recovery

```bash
# Preview recovery from a specific session
session-recovery path/to/session.jsonl

# Apply it
session-recovery path/to/session.jsonl --confirm
```

### Auto-Discover Sessions

```bash
# Find and preview all sessions touching certain files
session-recovery --scan-sessions --include "**/src/**"

# Apply
session-recovery --scan-sessions --include "**/src/**" --confirm
```

### Filter Files

```bash
# Only recover Rust files inside the repo
session-recovery --scan-sessions --include "**/*.rs" --ignore-external --confirm

# Exclude test files
session-recovery --scan-sessions --exclude "**/test_*" --confirm
```

### Point-in-Time Recovery

Recover a specific file as it was at a specific time:

```bash
# Preview
session-recovery --at "src/main.rs@2026-02-20T07:30:00Z"

# Apply
session-recovery --at "src/main.rs@2026-02-20T07:30:00Z" --confirm
```

### Time Range

```bash
# Only sessions from the last week
session-recovery --scan-sessions --since "2026-02-21" --confirm

# Sessions between specific dates
session-recovery --scan-sessions --since "2026-02-01" --until "2026-02-15" --confirm
```

## How It Works

1. **Preview** — By default, shows what would be recovered without making changes
2. **Confirm** — With `--confirm`, creates a recovery branch with commits for each operation
3. **Merge** — Leaves repo in uncommitted merge state so you can review before committing

### What Gets Created

- **Recovery branch** — Contains one commit per file operation
- **Session markers** — "Beginning recovery" and "Completing recovery" commits
- **Merge commit** — Combines recovery history with your current branch

### Handling External Files

Files outside the repo are mapped using `_../` encoding:
```
/other/path/file.txt → _../_../other/path/file.txt
```

Use `--ignore-external` to skip external files entirely.

### Failed Edits

When an edit can't find its target text, the content is appended to the file (with blank line separator) and the commit message is prefixed with ⚠️.

## CLI Reference

```
session-recovery [OPTIONS] [SESSIONS...]

Arguments:
  [SESSIONS...]              Session .jsonl files (optional if --scan-sessions)

Options:
      --repo <PATH>          Target repository [default: .]
      --branch <NAME>        Recovery branch name
      --include <GLOB>       Include files matching pattern (repeatable)
      --exclude <GLOB>       Exclude files matching pattern (repeatable)
      --ignore-external      Skip files outside repo
      --scan-sessions        Auto-discover sessions
      --sessions-dir <PATH>  Session directory [default: ~/.openclaw/agents/main/sessions/]
      --since <TIME>         Start of time range
      --until <TIME>         End of time range
      --at <PATH@TIME>       Point-in-time recovery
      --lookback <DUR>       Lookback for --at [default: 14d]
      --no-collapse          Don't collapse consecutive commits
      --confirm, --yes       Actually apply the recovery
      --list-only            Show detailed operation list
  -v, --verbose              Verbose output
  -h, --help                 Print help
```

## Examples

### Recover a Project's Full History

```bash
# Preview
session-recovery --scan-sessions --include "**/my-project/**" --ignore-external

# Apply
session-recovery --scan-sessions --include "**/my-project/**" --ignore-external --confirm
git commit  # Complete the merge
```

### Create Standalone Repo

```bash
mkdir my-project-history && cd my-project-history
git init && git commit --allow-empty -m "Initial"

session-recovery --scan-sessions --include "**/my-project/**" --ignore-external --confirm
git commit
```

### Recover File at Specific Time

```bash
session-recovery --at "src/lib.rs@2026-02-20T14:00:00Z" --ignore-external --confirm
git commit
```

## Design

See [DESIGN.md](DESIGN.md) for the full specification.

## License

MIT
