# Session Recovery - Design Document

Inspired by [claude-file-recovery](https://github.com/hjtenklooster/claude-file-recovery).

## Overview

Reconstruct file edit history from OpenClaw session logs as git commits, with deterministic commit hashes for reproducibility.

## Guiding Principle: Capture Everything

**Every character from every write/edit operation must be captured, no matter what.**

Even if:
- The edit can't find its target text → append to end (with 3 blank lines separator)
- The path collides with an existing directory → replace directory with file (old content preserved in history)
- The path uses weird encoding (`_../`) → still write it
- The operation seems nonsensical → capture it anyway

The goal is a complete record. Nonsense can be fixed in subsequent commits; lost data cannot be recovered.

**Note:** When appending due to failed edit match, do NOT insert comments — just 3 blank lines before the appended text. Comments aren't safe across all file formats. The commit message explains what happened.

## Safety: Protecting .git

**Never write to any path containing `.git` as a component.**

Any path with `.git` in it gets that component prefixed with `_`:
- `.git/config` → `_.git/config`  
- `foo/.git/hooks/pre-commit` → `foo/_.git/hooks/pre-commit`

This ensures recovery operations cannot corrupt the repository itself.

## Prerequisites

**Clean repository state required.** Before running, the tool verifies:
- No uncommitted changes in working tree
- No staged changes in index
- No in-progress rebase, merge, cherry-pick, etc.

If any of these conditions are not met, the tool exits with an error. There is no override flag — a clean state is strictly required.

## Core Principles

### Determinism
- Same input → same commit hashes
- Author and committer both derived from model (not git config)
- Timestamps from session log only — **missing timestamps are a fatal error** (logs should always have them)
- Timezone from log if present, otherwise UTC

### Idempotency
- Running recovery twice produces the same orphan/recovery commits (identical hashes)
- A new merge commit is created each time (this is expected and desired)
- The recovery branch can be safely re-created without affecting existing merges

### Symlink Resolution
- All paths are resolved to their real (non-symlink) paths before processing
- This affects: inside/outside repo determination, `.git` protection checks, `_../` path calculation

## Path Filtering

### Include/Exclude Lists (Glob Patterns)

**`--include <glob>`** (can be specified multiple times):
- Only include files matching these patterns
- Default: include all files
- Examples: `--include "crates/gravity/**"` `--include "*.rs"`

**`--exclude <glob>`** (can be specified multiple times):
- Exclude files matching these patterns (applied after include)
- Default: no exclusions
- Examples: `--exclude "*.log"` `--exclude "**/test_*"`

**`--ignore-external`**:
- Shorthand for excluding all files outside the repository
- Equivalent to only including files that resolve inside the repo

### Path Handling

Split all file operations into two categories:

**Inside repository:**
- Resolve directly to repo-relative path
- Always takes priority, never affected by external paths

**Outside repository:**
- Construct a symbolic path using `_../` components to represent the relative path
- This encodes the relationship to the repo root in a valid, deterministic path

**Example:**
```
Repo at:     /a/b/c/d
External:    /a/b/x/file.txt

Relative path from repo to file: ../../x/file.txt
Mapped path in repo:             /a/b/c/d/_../_../x/file.txt
```

## Automatic Session Discovery

**`--scan-sessions`**:
- Instead of specifying session files explicitly, scan the OpenClaw sessions directory
- Find sessions that contain operations matching the include patterns
- Append matching sessions to the recovery list

**`--sessions-dir <path>`**:
- Directory to scan for sessions
- Default: `~/.openclaw/agents/main/sessions/`

**`--since <timestamp>`** and **`--until <timestamp>`**:
- Only include sessions with activity within this time range
- Default `--since`: now minus 64×64×16×16 seconds (≈ 1,193 days / ~3.3 years)
- Default `--until`: now
- Format: ISO 8601 or relative (e.g., "7d", "24h", "2026-02-01")

## Point-in-Time Recovery

**`--at <path>@<timestamp>`**:
- Recover a specific file to a specific point in time
- Finds all sessions that touched `<path>` within the lookback window (default: 14 days before the timestamp)
- Replays operations up to and including the specified timestamp
- Stops there — does not include any operations after that time
- Timestamp format: ISO 8601, or git-style relative dates (e.g., "2 days ago", "yesterday 3pm")

**`--lookback <duration>`**:
- How far back to search for sessions when using `--at`
- Default: 14 days
- Format: "14d", "2w", "24h", etc.

**Example:**
```bash
# Recover gravity/src/main.rs as it was on Feb 20 at 2:30 AM
session-recovery --at "crates/gravity/src/main.rs@2026-02-20T07:30:00Z" --ignore-external
```

This is the common case: "I want the version of this file that existed at timestamp X, along with the history leading up to it."

## Commit Collapsing

**`--collapse` (default: enabled)**:
Collapse consecutive operations into a single commit when ALL of the following are true:

1. Operations are **additive only** — no deletions (edits that only add lines, writes that don't replace)
2. No interleaved **user messages** between them
3. No interleaved **tool calls** other than safe read-only operations:
   - `read`, `Read`
   - `web_search`, `web_fetch`
   - `grep`, `find`, `ls`, `cat`, `head`, `tail`
   - (any tool that can't execute external commands or write data)

This reduces commit count without losing any states that were actually observed/used by the agent.

**`--no-collapse`**: Disable collapsing, create one commit per operation.

Collapsed commits have message format:
```
[N ops] write/edit: path/to/file.rs
```

## Operations

### Write
- Full file content, straightforward
- If path is currently a directory: delete directory contents, create file (old contents preserved in git history)

### Edit
- Use OpenClaw's existing edit resolution logic where it works
- If exact match fails: apply best-effort matching (fuzzy, whitespace normalization, etc.)
- If all matching fails: append `new_text` to end of file (preceded by 3 blank lines)
- Failed matches: commit message prefixed with ⚠️

### Read
- Creates a "context commit" if the file is written/edited anywhere in the transcript (even later)
- Only if it would actually change the current file state in the recovery branch
- Provides baseline content that may help resolve later edits more accurately

## Error Handling

### Malformed Log Lines
- Skip invalid/unparseable lines
- Batch consecutive skipped lines into a single warning commit
- Warning commit message: "⚠️ Skipped N malformed lines"
- Timestamp: average of the timestamps immediately before and after the skipped section
- Final merge commit message notes "partial recovery with errors" if any lines were skipped

### Empty Recovery
- If recovery would produce only empty/warning commits with no actual file operations: 
- Do NOT create a merge commit
- Exit with error: "No data to recover from session(s)"

### Missing Timestamps
- Fatal error — logs should always have timestamps
- Do not fall back to current time or other heuristics

## Commit Messages

**Initial commit (per session, always orphan):**
```
Beginning recovery from OpenClaw session <session-id>
```

**File operations:**
```
write: path/to/file.rs
```
```
edit: path/to/file.rs
```
```
⚠️ edit (appended): path/to/file.rs
```

**Collapsed operations:**
```
[5 ops] write/edit: path/to/file.rs
```

No model or timestamp in message body — already in author/date.

**Skipped lines:**
```
⚠️ Skipped 3 malformed lines
```

**Final commit (per session):**
```
Completing recovery from OpenClaw session <session-id>
```

## Multiple Transcripts

Accept multiple transcript paths as arguments. Process as one logical stream:

1. Sort all events chronologically (conceptually interleaved)
2. Preserve tool call/response pairs (don't actually interleave mid-call)
3. Earlier transcripts provide context for later edit resolution

### Per-Session Markers

- "Beginning recovery" at timestamp of first event in that session
- "Completing recovery" at timestamp of last event in that session
- These may interleave if sessions overlap temporally

### Orphan Commits for Traceability

**Each session's "Beginning recovery" commit is always an orphan.**

If combining multiple transcripts:
1. First session: orphan becomes base of recovery branch
2. Subsequent sessions: orphan is immediately merged into ongoing branch
   - Merge message: "Including OpenClaw session <id> in recovery"
3. This ensures the same initial commit hash exists in all branches recovered from that session
4. You can find all branches derived from a session by searching for its initial orphan commit hash

### Determinism Across Runs

- Non-overlapping transcripts: first transcript's commits identical to running it alone (assuming no path remapping changes)
- Later transcripts build on earlier state

## Author/Committer Format

**Anthropic models:**
```
Claude Opus 4.5 <noreply@anthropic.com>
```

**Other providers (future):**
- TODO: Check `save` crate for prefix character conventions
- TODO: Copy styles/email formats where appropriate
- Still include full model name, may add prefix character

**Unknown models (fallback):**
- Use raw model identifier as username
- Email: `<model-id>@unknown.local` or similar

## Final State

Leave repository in uncommitted merge state:
- Merge strategy: keep current tree (like `git merge -s ours`), just incorporate history
- The recovery branch may have corrupted final state from failed edits, but the history is valuable
- Draft merge commit message:

**Single session:**
```
Merge recovered OpenClaw session <id>
```

**Multiple sessions (grammatically correct list):**
```
Merge recovered OpenClaw sessions <id1>, <id2>, and <id3>
```

**If errors occurred:**
```
Merge recovered OpenClaw session <id> (partial recovery with errors)
```

## CLI Flags Summary

The tool always outputs a summary of all parameters:

```
session-recovery v0.1.0
━━━━━━━━━━━━━━━━━━━━━━━
Repository:      /path/to/repo (default: .)
Branch:          recovered-abc123 (default: derived from first session)
Sessions:        3 files (or: scanning ~/.openclaw/agents/main/sessions/)
Include:         ["crates/gravity/**"] (default: all)
Exclude:         [] (default: none)
Ignore external: yes (default: no)
Time range:      2023-01-01 to 2026-02-28 (default: last ~3.3 years)
Collapse:        yes (default: yes)
Dry run:         no

Found 1023 operations in 5 sessions...
```

### Full Flag List

| Flag | Description | Default |
|------|-------------|---------|
| `<sessions...>` | Session .jsonl files to recover | (optional if --scan-sessions or --at) |
| `--repo <path>` | Target repository | `.` |
| `--branch <name>` | Recovery branch name | `recovered-<first-session-id>` |
| `--include <glob>` | Include files matching pattern | all |
| `--exclude <glob>` | Exclude files matching pattern | none |
| `--ignore-external` | Skip files outside repo | no |
| `--scan-sessions` | Auto-discover sessions | no |
| `--sessions-dir <path>` | Directory to scan | `~/.openclaw/agents/main/sessions/` |
| `--since <time>` | Start of time range | ~3.3 years ago |
| `--until <time>` | End of time range | now |
| `--at <path>@<time>` | Point-in-time recovery for specific file | (none) |
| `--lookback <duration>` | Session search window for --at | 14d |
| `--collapse` / `--no-collapse` | Collapse additive operations | yes |
| `--dry-run` | Show what would be done | no |
| `--list-only` | List operations without committing | no |
| `--verbose` | Detailed output | no |

## Standalone Repository Creation

To create a standalone repository with just the recovery history:

1. Create new empty repo with single empty initial commit
2. Run recovery with appropriate filters
3. Commit the merge
4. Result: clean repo with just the recovered file history

Example for recovering session-recovery's own history:
```bash
mkdir session-recovery-standalone && cd session-recovery-standalone
git init && git commit --allow-empty -m "Initial commit"
session-recovery --scan-sessions --include "crates/session-recovery/**" --ignore-external
git commit  # Accept the merge
```

## Future Scope

### Claude Code and Other Agents
This tool is designed for OpenClaw session logs, but the architecture should eventually support:
- Claude Code (`~/.claude/` session history)
- Other AI coding agents with similar log formats
- Generic JSONL transcript format

This is out of scope for the initial implementation. We will continue refining the OpenClaw support first, then consider extending to other formats.

### Additional Future Considerations
- Non-Anthropic model identification
- Integrate with `save` crate author conventions
- Better fuzzy matching strategies (explicit priority order for determinism)
- Handle file deletions if detectable from exec calls
