# Testing Performed

## Test Environments

### 1. Isolated Test Repository (`/tmp/sr-test`, `/tmp/sr-confirm-test`, etc.)
- Fresh git repos with single empty initial commit
- Used to test recovery without affecting real repos
- Verified merge workflow completes correctly

### 2. Real Repository (jeb monorepo)
- Recovered session-recovery's own development history
- Verified files resolve correctly when inside the repo
- Merged recovery history successfully

## Features Tested

### Preview Mode (Default)
- ✅ Running without `--confirm` shows preview only
- ✅ No changes made to repo in preview mode
- ✅ Output shows operation count and instructions

### Confirmation Mode
- ✅ `--confirm` flag applies recovery
- ✅ `--yes` alias works identically
- ✅ Creates recovery branch with commits
- ✅ Leaves repo in uncommitted merge state

### Session Discovery
- ✅ `--scan-sessions` finds sessions by path pattern
- ✅ Time range filtering with `--since`/`--until`
- ✅ Custom sessions directory with `--sessions-dir`

### Path Filtering
- ✅ `--include` glob patterns match file paths
- ✅ Multiple `--include` patterns work
- ✅ `--ignore-external` skips files outside repo

### Point-in-Time Recovery
- ✅ `--at path@timestamp` parses correctly
- ✅ Operations after cutoff time are excluded
- ✅ Lookback window finds relevant sessions

### External File Handling
- ✅ External paths encoded with `_../` prefix
- ✅ Path structure preserved in encoding
- ✅ Files written to correct locations

### Multi-Session Recovery
- ✅ Multiple sessions combined chronologically
- ✅ Session start/end markers created
- ✅ Orphan commits per session for traceability

### Merge Strategy
- ✅ Uses "ours" strategy (keeps current tree)
- ✅ Recovery history incorporated without changing files
- ✅ Merge message includes session IDs

### Edit Fallback
- ✅ Failed edits append content (3 blank lines)
- ✅ Commit message prefixed with ⚠️
- ✅ No comment markers inserted

### Determinism
- ✅ Same session produces same initial orphan commit hash
- ✅ Author/committer from model, not git config
- ✅ Timestamps from session log

## Test Scenarios Run

1. **session-recovery's own history** — Recovered 33 operations from current session into jeb repo
2. **Gravity project history** — Scanned 3 sessions with 1010+ operations (dry run)
3. **External file recovery** — Tested `_../` encoding in isolated test repo
4. **Point-in-time cutoff** — Verified operations stop at specified timestamp
5. **Preview vs Apply** — Confirmed no changes without `--confirm`

### Additional Tests Performed (2026-02-28)

#### `.git` Sanitization
- ✅ Paths containing `.git` are rewritten to `_.git`
- ✅ Works for `.git/config` → `_.git/config`
- ✅ Works for nested paths: `src/.git/hooks/pre-commit` → `src/_.git/hooks/pre-commit`
- ✅ Real `.git` directory remains untouched

#### Path Remapping
- ✅ `--strip-prefix "/other/worktree/"` correctly removes prefix
- ✅ `/other/worktree/src/main.rs` becomes `src/main.rs`

#### Exclude Patterns
- ✅ `--exclude "**/test_*"` correctly filters out matching files

#### Empty Recovery
- ✅ Session with no file operations returns error "no file operations"

#### Add Prefix
- ✅ `--add-prefix "legacy/"` correctly adds prefix to paths
- ✅ `src/main.rs` becomes `legacy/src/main.rs`

## Not Yet Tested
- [ ] `--no-collapse` flag
- [ ] Commit collapsing behavior
- [ ] Directory-to-file replacement
- [ ] Malformed session log handling
- [ ] Empty recovery (no operations) error
- [ ] Multiple sessions with overlapping timestamps
- [ ] Very large sessions (performance)
- [ ] Non-Anthropic model author formatting
