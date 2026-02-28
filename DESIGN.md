# Session Recovery - Design Document

## Overview

Reconstruct file edit history from OpenClaw session logs as git commits, with deterministic commit hashes for reproducibility.

## Core Principles

### Determinism
- Same input → same commit hashes
- Author and committer both derived from model (not git config)
- Timestamps from session log only (no fallbacks to current time)
- Timezone from log if present, otherwise UTC

### Path Handling

**Inside repository:**
- Resolve directly, paths are unambiguous

**Outside repository:**
- Find common root of: session `cwd` + all write/edit paths in transcript
- Remap to `<session-id>/<common-prefix-stripped-path>` inside repo
- Consistent naming, no leaked absolute paths

## Operations

### Write
- Full file content, straightforward

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

Leave repository in uncommitted merge state. Draft merge commit message:

**Single session:**
```
Merge recovered OpenClaw session <id>
```

**Multiple sessions (grammatically correct list):**
```
Merge recovered OpenClaw sessions <id1>, <id2>, and <id3>
```

## Future Considerations

- Non-Anthropic model identification
- Integrate with `save` crate author conventions
- Better fuzzy matching for edits
- Handle file deletions if detectable from exec calls
