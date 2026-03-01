# session-recovery

Recover file history from OpenClaw session logs as git commits.

Inspired by [claude-file-recovery](https://github.com/hjtenklooster/claude-file-recovery).

## Why Use This?

When you work with AI coding assistants through OpenClaw, every file edit is logged — but those changes happen outside of git. If you want to see how a file evolved during a session, or recover a version from before you overwrote it, that history exists only in the session logs.

This tool extracts that hidden history and reconstructs it as git commits, so you can:
- Browse how files changed during AI-assisted work
- Recover specific versions of files
- Merge session history into your git repository

## Prerequisites

- Rust (for building)
- Git
- OpenClaw session logs (typically in `~/.openclaw/agents/main/sessions/`)

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
# Find all sessions touching certain files
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

Recover a file as it was at a specific time:

```bash
session-recovery --at "src/main.rs@2026-02-20T07:30:00Z" --confirm
```

### Path Remapping

When work was done in a different location (e.g., a worktree), remap paths:

```bash
# Remove prefix from paths
session-recovery --strip-prefix "/tmp/agent-workspace/" --confirm

# Remap to a subdirectory
session-recovery --strip-prefix "/old/path/" --add-prefix "imported/" --confirm
```

### Time Range

```bash
# Only sessions from the last week
session-recovery --scan-sessions --since "2026-02-21" --confirm
```

## How It Works

1. **Preview** — By default, shows what would be recovered without making changes
2. **Confirm** — With `--confirm`, creates a recovery branch with commits for each operation
3. **Merge** — Leaves repo in uncommitted merge state so you can review before committing

### After Running with `--confirm`

Your repo will be in an uncommitted merge state:
```
$ git status
On branch main
All conflicts fixed but you are still merging.
  (use "git commit" to conclude merge)
```

**To complete the recovery:**
```bash
git commit
```

**To abort and discard the recovery:**
```bash
git merge --abort
```

The recovery history is preserved on a branch (e.g., `recovered-abc123`) even if you abort.

### External Files

Files outside your repository are encoded with `_../` path components:
```
/other/project/file.txt → _../_../other/project/file.txt
```

This preserves the relative path structure. Use `--ignore-external` to skip external files entirely, or `--strip-prefix` to remap them.

### Failed Edits

When an edit operation can't find its target text (because the file state differs from what the AI saw), the new content is appended to the file with blank line separators. The commit message is prefixed with ⚠️ so you can find these cases:

```bash
git log --grep="⚠️"
```

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
      --since <TIME>         Start of time range [default: ~3.3 years ago]
      --until <TIME>         End of time range [default: now]
      --at <PATH@TIME>       Point-in-time recovery
      --lookback <DUR>       Lookback for --at [default: 14d]
      --strip-prefix <PATH>  Remove this prefix from file paths
      --add-prefix <PATH>    Add this prefix to file paths
      --no-collapse          Don't collapse consecutive commits
      --confirm, --yes       Actually apply the recovery (required)
      --list-only            Show detailed operation list
  -v, --verbose              Verbose output
  -h, --help                 Print help
```

## Troubleshooting

### "no sessions found"
- Check that `--sessions-dir` points to the right location
- Verify your `--include` pattern matches the file paths in the session
- Try `--verbose` to see which sessions are being scanned

### "uncommitted changes"
The tool requires a clean git state. Commit or stash your changes first:
```bash
git stash
session-recovery ... --confirm
git stash pop
```

### Recovery produced unexpected results
- Use `git merge --abort` to discard the merge
- The recovery branch still exists — you can inspect it: `git log recovered-xxx`
- Try with `--list-only` first to see exactly what operations would be applied

### Files appear in weird `_../` directories
These are files that were outside your repository. Use `--ignore-external` to skip them, or `--strip-prefix` to remap them to the correct location.

## Design

See [DESIGN.md](DESIGN.md) for the full specification.

## License

MIT
