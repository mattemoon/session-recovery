# Session Recovery - Output Format Design

## Guiding Principles

1. **Clarity over brevity**: A user or agent should understand what's happening without reading documentation
2. **Progressive detail**: Summary first, details on demand (verbose mode)
3. **Accurate state reporting**: Always show what *is*, not what *might be*
4. **Commit IDs matter**: Since commits are created in both preview and apply mode, truncated commit IDs (12 chars) should be shown where appropriate

## Mode Distinction

The `--confirm` flag doesn't control whether commits are created — it controls whether refs are updated. In both modes:
- Commits and trees are written to the object database
- Blobs are created for file contents

The difference:
- **Preview mode** (default): Objects exist but no refs point to them
- **Apply mode** (`--confirm`): Branch is created/updated, merge state set up

This should be reflected in the output with a clear indication like:
```
Mode: preview (commits created but refs unchanged)
```
or
```
Mode: apply (creating branch and preparing merge)
```

## Header Section

Always shown first:

```
session-recovery v0.1.0
━━━━━━━━━━━━━━━━━━━━━━━

Configuration
  Repository:      /path/to/repo
  Target branch:   recovered-abc123def456
  Mode:            preview (commits created but refs unchanged)

Sessions (3 found)
  • 7bb34e11-84e5-4d3b-8762-5a343145d955 (2026-02-18 to 2026-02-20)
  • a1b2c3d4-e5f6-7890-abcd-ef1234567890 (2026-02-15)
  • deadbeef-cafe-babe-1234-567890abcdef (2026-02-12 to 2026-02-14)

Filters
  Include:         crates/gravity/**
  Exclude:         (none)
  Ignore external: yes
  Time range:      2026-02-01 to 2026-02-28
  Path remap:      --strip-prefix /tmp/work/ --add-prefix legacy/
```

Notes:
- Session date ranges show activity span (first op to last op)
- Single-day sessions show just one date
- Path remap only shown if either flag is set

## File Summary

After scanning sessions, show what will be recovered:

```
Files to Recover
  crates/gravity/src/main.rs          (42 versions from 3 sessions)
  crates/gravity/src/lib.rs           (18 versions from 2 sessions)  
  crates/gravity/src/audio.rs         (7 versions from 1 session)
  crates/gravity/Cargo.toml           (3 versions from 2 sessions)
```

Format: `path (N versions from M sessions)`

In verbose mode, expand to show session breakdown:

```
Files to Recover
  crates/gravity/src/main.rs
    • 7bb34e11: 25 ops (2026-02-18 14:30 to 2026-02-20 03:15)
    • a1b2c3d4: 12 ops (2026-02-15 09:00 to 2026-02-15 22:45)
    • deadbeef: 5 ops  (2026-02-12 16:20 to 2026-02-14 11:30)
```

## Processing Output

### Preview Mode

```
Processing...
  ✓ Session 7bb34e11 → 47 commits (a1b2c3d4e5f6..f6e5d4c3b2a1)
  ✓ Session a1b2c3d4 → 23 commits (1234567890ab..ba0987654321)
  ✓ Session deadbeef → 12 commits (abcdef123456..654321fedcba)

Summary
  Total commits:   82 (across 3 sessions)
  Files affected:  4
  Warnings:        2 (use --verbose for details)
```

The commit range shows first..last commit ID for that session's contribution.

### Apply Mode

```
Processing...
  ✓ Session 7bb34e11 → 47 commits (a1b2c3d4e5f6..f6e5d4c3b2a1)
  ✓ Session a1b2c3d4 → 23 commits (1234567890ab..ba0987654321)
  ✓ Session deadbeef → 12 commits (abcdef123456..654321fedcba)

Summary
  Total commits:   82 (across 3 sessions)
  Files affected:  4
  Warnings:        2 (use --verbose for details)

Branch created: recovered-7bb34e11 @ 654321fedcba

Merge State
  Repository is now in an uncommitted merge state.
  Current tree:    unchanged (--strategy ours)
  Recovery branch: recovered-7bb34e11 (merged for history only)
  
  To complete:     git commit
  To abort:        git merge --abort
  To inspect:      git log --all --graph --oneline
```

## Warnings and Errors

### Edit Failures (append fallback)

When an edit can't find its target text:

```
Warnings
  ⚠️  crates/gravity/src/main.rs @ 2026-02-19T14:32:15Z
      Edit target not found, content appended
      Commit: a1b2c3d4e5f6
```

### Malformed Lines

```
Warnings
  ⚠️  Session 7bb34e11: 3 malformed lines skipped
      (lines 1042, 1043, 1047)
```

### Partial Recovery

If errors occurred during any session:

```
⚠️  PARTIAL RECOVERY: Some operations failed or were skipped.
    Merge message will note: "(partial recovery with errors)"
    Review warnings above before completing the merge.
```

## Special Cases

### Empty Recovery

```
session-recovery v0.1.0
━━━━━━━━━━━━━━━━━━━━━━━

Configuration
  Repository:      /path/to/repo
  ...

Sessions (0 found matching filters)

Error: No sessions contain matching file operations.

Suggestions:
  • Check --include patterns match your target files
  • Try --scan-sessions to auto-discover sessions
  • Adjust --since/--until time range
  • Use --verbose to see what's being filtered out
```

### Point-in-Time Recovery (`--at`)

```
session-recovery v0.1.0
━━━━━━━━━━━━━━━━━━━━━━━

Point-in-Time Recovery
  Target:          crates/gravity/src/main.rs
  As of:           2026-02-20T07:30:00Z
  Lookback:        14 days (2026-02-06 to 2026-02-20)

Sessions (2 found with matching operations)
  • 7bb34e11-84e5-4d3b-8762-5a343145d955 (38 ops before cutoff)
  • a1b2c3d4-e5f6-7890-abcd-ef1234567890 (12 ops before cutoff)
```

### `--list-only` Mode

Detailed listing of all operations:

```
session-recovery v0.1.0
━━━━━━━━━━━━━━━━━━━━━━━

Operations (127 total)

Session 7bb34e11 (2026-02-18 to 2026-02-20)
  [2026-02-18T14:30:22Z] write  crates/gravity/src/main.rs
  [2026-02-18T14:31:05Z] edit   crates/gravity/src/main.rs
  [2026-02-18T14:32:18Z] edit   crates/gravity/src/main.rs
  [2026-02-18T14:35:00Z] write  crates/gravity/Cargo.toml
  ...

Session a1b2c3d4 (2026-02-15)
  [2026-02-15T09:00:12Z] write  crates/gravity/src/lib.rs
  ...
```

## Implementation Notes

### Commit ID Format

Always 12 hex characters (first 12 of full SHA):
```
a1b2c3d4e5f6
```

Ranges use `..`:
```
a1b2c3d4e5f6..f6e5d4c3b2a1
```

### Timestamps

ISO 8601 format in verbose/list modes:
```
2026-02-20T07:30:00Z
```

Human-friendly for summaries:
```
2026-02-20 (single day)
2026-02-18 to 2026-02-20 (range)
```

### Color (when TTY)

- Session IDs: cyan
- Commit IDs: yellow  
- File paths: white/default
- Warnings: yellow
- Errors: red
- Success indicators (✓): green

### Structured Output (Future)

Consider `--json` flag for machine-readable output:
```json
{
  "version": "0.1.0",
  "mode": "preview",
  "sessions": [...],
  "commits": [...],
  "warnings": [...],
  "result": "success"
}
```

## Summary

The output should tell a complete story:
1. **What was requested** (configuration, filters, mode)
2. **What was found** (sessions, files, operation counts)
3. **What was done** (commits created, ranges)
4. **What needs attention** (warnings, next steps)
5. **Current state** (merge status, how to proceed)

A user seeing this output for the first time should understand exactly what happened and what to do next.
