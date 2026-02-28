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

## Core Principles

### Determinism
- Same input → same commit hashes
- Author and committer both derived from model (not git config)
- Timestamps from session log only (no fallbacks to current time)
- Timezone from log if present, otherwise UTC

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
- If path is currently a directory: delete directory, create file (directory contents preserved in git history)

### Edit
- Try exact match first
- If no match: fuzzy matching, whitespace normalization, etc.
- Worst case: append to end of file
- Failed matches: commit message prefixed with ⚠️ and explanation

### Read
- If the file is written/edited anywhere in the transcript (even later), create a commit from read contents
- Only if it would actually change the file state
- Provides context that may help resolve later edits

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
- Better fuzzy matching for edits
- Handle file deletions if detectable from exec calls
