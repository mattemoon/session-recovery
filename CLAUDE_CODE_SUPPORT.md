# Claude Code Log Support — Design Plan

## Goal

Extend session-recovery to support Claude Code session logs alongside OpenClaw logs, allowing unified recovery from mixed sources.

## Log Format Comparison

### OpenClaw Format (current)
```jsonl
{"type":"session","id":"7bb34e11-84e5-4d3b-8762-5a343145d955",...}
{"type":"message","timestamp":"2026-02-18T09:33:54Z","message":{"role":"assistant","content":[
  {"type":"toolCall","name":"Write","arguments":{"file_path":"/path/to/file","content":"..."}}
]}}
```

Key fields:
- `type: "message"` with `message.role: "assistant"`
- Tool calls in `message.content[]` with `type: "toolCall"`
- `name`: tool name (case-insensitive: `Write`, `Edit`)
- `arguments.file_path` or `arguments.path`: target file
- `arguments.content`: file content (Write)
- `arguments.oldText`/`arguments.old_string`: edit target (Edit)
- `arguments.newText`/`arguments.new_string`: edit replacement (Edit)

### Claude Code Format (new)
```jsonl
{"type":"assistant","timestamp":"2026-02-12T23:30:31Z","sessionId":"f855e1d3-...","message":{
  "role":"assistant","content":[
    {"type":"tool_use","id":"toolu_01...","name":"Write","input":{"file_path":"/path/to/file","content":"..."}}
  ]
}}
```

Key differences:
- `type: "assistant"` (not `"message"`)
- Tool calls have `type: "tool_use"` (not `"toolCall"`)
- Parameters in `input` (not `arguments`)
- Tool names: `Write`, `Edit`, `MultiEdit` (same names, same case)
- Has `sessionId` at message level (not in separate session entry)
- Has `cwd` field (working directory context)
- Has `version` field (Claude Code version)

### Session Directory Locations

**OpenClaw:**
- Default: `~/.openclaw/agents/main/sessions/`
- Flat directory of `{session-id}.jsonl` files

**Claude Code:**
- Default: `~/.claude/projects/{project-slug}/`
- Project slug is path with `-` replacing `/` (e.g., `-Users-matte-jeb`)
- Also `{session-id}.jsonl` files within each project directory

## Implementation Plan

### Phase 1: Format Detection

Add auto-detection of log format based on JSONL content:

```rust
enum LogFormat {
    OpenClaw,
    ClaudeCode,
    Unknown,
}

fn detect_format(first_lines: &[&str]) -> LogFormat {
    // Look for distinguishing markers:
    // - OpenClaw: type:"message" with message.content[].type:"toolCall"
    // - Claude Code: type:"assistant" with message.content[].type:"tool_use"
    // - Claude Code also has "version" field with semver like "2.1.39"
}
```

### Phase 2: Unified Operation Extraction

Abstract the extraction logic:

```rust
trait SessionLogParser {
    fn parse_line(&mut self, line: &str) -> Option<Op>;
    fn session_id(&self) -> &str;
    fn format(&self) -> LogFormat;
}

struct OpenClawParser { ... }
struct ClaudeCodeParser { ... }
```

Both parsers produce the same `Op` type:
```rust
struct Op {
    ts: DateTime<Utc>,
    tz: i32,
    model: String,
    session: String,
    kind: OpKind,
    path: String,
}
```

### Phase 3: Claude Code Session Discovery

Add `--claude-sessions-dir` flag:
```
--claude-sessions-dir <PATH>   Claude Code projects directory
                               [default: ~/.claude/projects/]
```

Scan all subdirectories for `.jsonl` files when `--scan-sessions` is used.

### Phase 4: Mixed Recovery

Allow specifying both session sources:
```bash
session-recovery \
  --scan-sessions \
  --sessions-dir ~/.openclaw/agents/main/sessions/ \
  --claude-sessions-dir ~/.claude/projects/ \
  --include "**/my-project/**"
```

Sessions are merged chronologically by timestamp, maintaining correct authorship attribution per-commit.

### Phase 5: Project Filtering for Claude Code

Claude Code organizes sessions by project. Add project-aware filtering:
```
--claude-project <SLUG>   Only scan specific Claude Code project
                          (e.g., "-Users-matte-jeb")
```

## Field Mapping

| Concept | OpenClaw | Claude Code |
|---------|----------|-------------|
| Session ID | `type:"session"` entry `id` field | `sessionId` on each message |
| Timestamp | `timestamp` | `timestamp` |
| Model | `message.model` or `modelId` | `message.model` |
| Write path | `arguments.file_path` or `.path` | `input.file_path` |
| Write content | `arguments.content` | `input.content` |
| Edit old | `arguments.oldText` or `.old_string` | `input.old_string` |
| Edit new | `arguments.newText` or `.new_string` | `input.new_string` |
| Tool type marker | `type:"toolCall"` | `type:"tool_use"` |
| Working dir | (inferred from paths) | `cwd` field |

## Claude Code Specific Tools

Claude Code has additional tools we should consider:

- **MultiEdit**: Multiple edits in one call — extract as sequence of Edit ops
- **Bash/Shell**: Could track file operations from shell commands (future enhancement)
- **TodoWrite/TodoRead**: Not file operations, ignore

## Edge Cases

1. **Truncated/streaming tool calls** — Claude Code has `content_block_start`/`content_block_delta` streaming entries. Skip these; only process complete `tool_use` blocks.

2. **Empty/malformed JSONL lines** — Skip gracefully, log warning count at end.

3. **Session ID extraction timing** — Claude Code has `sessionId` per-message (no upfront session entry like OpenClaw). Extract from first parseable message.

4. **Symlinked session directories** — Follow symlinks (standard fs behavior).

5. **Path normalization** — Canonicalize paths after `cwd` resolution before `--ignore-external` comparison.

6. **MultiEdit atomicity** — Extract as sequence of Edit ops with same timestamp.

## Open Questions (Resolved)

1. **Should we merge sessions from both sources into a single recovery branch?**
   - Yes, chronologically interleaved, each commit attributed to its source session

2. **How to handle overlapping edits from both tools on the same file?**
   - Process chronologically by timestamp; later edits win (same as current behavior)

3. **Should we support Claude Code's `cwd` for relative path resolution?**
   - Yes, when path is relative, resolve against `cwd` from the message

4. **Project slug → path mapping?**
   - Not needed initially; `cwd` in messages provides real paths
   - Revisit if `--ignore-external` has issues

## CLI Changes

### New flags:
```
--claude-sessions-dir <PATH>   Claude Code projects directory [default: ~/.claude/projects/]
```

**Deferred (add if needed):**
- `--claude-project <SLUG>` — use `--include` glob instead
- `--log-format <auto|openclaw|claude-code>` — auto-detection should be reliable

### Updated help text:
```
session-recovery — Recover file history from OpenClaw and Claude Code session logs

Supports both OpenClaw (.openclaw/agents/*/sessions/) and Claude Code 
(.claude/projects/) session formats. Format is auto-detected from log content.
```

## Testing Strategy

1. **Unit tests**: Parse sample logs from both formats
2. **Integration tests**: Recovery from mixed sessions
3. **Real-world test**: Recover this crate's own history from both log sources (we have Claude Code sessions in `~/.claude/projects/-Users-matte-jeb/`)

## Implementation Order

1. Add format detection
2. Refactor OpenClaw parser into trait impl
3. Implement Claude Code parser
4. Add `--claude-sessions-dir` flag and scanning
5. Update output to show log format per session
6. Add `--claude-project` filter
7. Update documentation
8. Test with real mixed recovery

## Commit Plan

1. `session-recovery: add log format detection`
2. `session-recovery: refactor parser into trait`
3. `session-recovery: implement Claude Code parser`
4. `session-recovery: add --claude-sessions-dir scanning`
5. `session-recovery: unified mixed-source recovery`
6. `session-recovery: update docs for Claude Code support`
