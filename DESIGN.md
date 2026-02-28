# Session Recovery - Design Document

## Overview

Reconstruct file edit history from OpenClaw session logs as git commits, with deterministic commit hashes for reproducibility.

## Guiding Principle: Capture Everything

**Every character from every write/edit operation must be captured, no matter what.**

Even if:
- The edit can't find its target text → append to end with ⚠️ warning
- The path collides with an existing directory → replace directory with file (old content preserved in history)
- The path uses weird encoding (`_../`) → still write it
- The operation seems nonsensical → capture it anyway

The goal is a complete record. Nonsense can be fixed in subsequent commits; lost data cannot be recovered.

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

**Why `_../` instead of `../`:**
- `../` is not a valid path component (would escape repo)
- `_../` is a legal directory name that symbolically represents "up"
- Produces consistent, deterministic paths across sessions
- The relative structure is preserved and visible

**`--ignore-external` flag:**
- When set, completely ignore all files outside the current repository
- Useful when recovering only in-repo changes from sessions that also touched external files

## Operations

### Write
- Full file content, straightforward
- If path is currently a directory: delete directory contents, create file (old contents preserved in git history)

### Edit
- Use OpenClaw's existing edit resolution logic where it works
- If exact match fails: apply best-effort matching (fuzzy, whitespace normalization, etc.)
- If all matching fails: append `new_text` to end of file to ensure full preservation
- Failed matches: commit message prefixed with ⚠️ and explanation

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
⚠️ edit (fuzzy): path/to/file.rs

Warning: Exact match failed, applied best-effort replacement.
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
- Merge strategy: prefer recovery branch versions on conflicts
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

## CLI Flags

- `--ignore-external`: Skip all files outside the current repository
- `--branch <name>`: Explicit branch name (otherwise derived from first session id)
- `--dry-run`: Show what would be done without making changes
- `--verbose`: Detailed output
- `--list-only`: Just list operations, don't create commits
- `--filter <prefix>`: Only include files matching path prefix

## Future Considerations

- Non-Anthropic model identification
- Integrate with `save` crate author conventions
- Better fuzzy matching strategies (explicit priority order for determinism)
- Handle file deletions if detectable from exec calls
