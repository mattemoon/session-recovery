//! session-recovery — Recover file history from OpenClaw session logs
//!
//! Extracts write/edit/read operations from .jsonl session files and reconstructs
//! them as a git branch with proper timestamps and authorship.
//!
//! See DESIGN.md for full specification.

use anyhow::{bail, Context, Result};
use chrono::{DateTime, FixedOffset, Utc};
use clap::Parser;
use git2::{Repository, RepositoryState, Signature, Time, Oid, FileMode};
use serde::Deserialize;
use std::collections::{HashMap, HashSet, BTreeMap};
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf, Component};

#[derive(Parser)]
#[command(name = "session-recovery")]
#[command(about = "Recover file history from OpenClaw session logs")]
struct Args {
    /// Paths to session .jsonl files (can specify multiple)
    #[arg(required = true)]
    sessions: Vec<PathBuf>,

    /// Repository path (default: current directory)
    #[arg(long, default_value = ".")]
    repo: PathBuf,

    /// Branch name for reconstructed history
    #[arg(long)]
    branch: Option<String>,

    /// Ignore files outside the current repository
    #[arg(long)]
    ignore_external: bool,

    /// Filter to only include files matching this path prefix
    #[arg(long)]
    filter: Option<String>,

    /// Dry run - show what would be done without making changes
    #[arg(long)]
    dry_run: bool,

    /// Verbose output
    #[arg(short, long)]
    verbose: bool,

    /// Just extract and list operations, don't create git history
    #[arg(long)]
    list_only: bool,
}

// ============================================================================
// Data structures
// ============================================================================

#[derive(Debug, Clone)]
struct SessionInfo {
    id: String,
    first_timestamp: DateTime<Utc>,
    last_timestamp: DateTime<Utc>,
    cwd: Option<String>,
}

#[derive(Debug, Clone)]
struct FileOperation {
    timestamp: DateTime<Utc>,
    tz_offset_minutes: i32,
    model: String,
    session_id: String,
    op_type: OpType,
    path: String,
    line_number: usize,
}

#[derive(Debug, Clone)]
enum OpType {
    Write { content: String },
    Edit { old_text: String, new_text: String },
    Read { content: String },
    SessionStart,
    SessionEnd,
    SkippedLines { count: usize },
}

#[derive(Debug, Clone)]
enum EditResult {
    ExactMatch,
    FuzzyMatch { description: String },
    Appended,
}

// ============================================================================
// Parsing session logs
// ============================================================================

#[derive(Debug, Deserialize)]
struct SessionEntry {
    #[serde(rename = "type")]
    entry_type: String,
    timestamp: Option<String>,
    #[serde(rename = "modelId")]
    model_id: Option<String>,
    message: Option<Message>,
    id: Option<String>,
    cwd: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Message {
    role: Option<String>,
    model: Option<String>,
    content: Option<serde_json::Value>,
    #[serde(rename = "toolName")]
    tool_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ToolCall {
    #[serde(rename = "type")]
    call_type: Option<String>,
    name: Option<String>,
    arguments: Option<serde_json::Value>,
}

fn parse_timestamp(s: &str) -> Option<(DateTime<Utc>, i32)> {
    // Try to parse with timezone info
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        let offset_minutes = dt.offset().local_minus_utc() / 60;
        return Some((dt.with_timezone(&Utc), offset_minutes));
    }
    // Fallback: assume UTC
    if let Ok(dt) = s.parse::<DateTime<Utc>>() {
        return Some((dt, 0));
    }
    None
}

/// Extract session info and all operations from a single session file
fn extract_session(
    session_path: &Path,
    verbose: bool,
) -> Result<(SessionInfo, Vec<FileOperation>)> {
    let file = File::open(session_path)
        .with_context(|| format!("Failed to open session file: {}", session_path.display()))?;
    let reader = BufReader::new(file);

    let mut operations = Vec::new();
    let mut current_model = String::from("unknown");
    let mut session_id = session_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();
    let mut session_cwd: Option<String> = None;
    let mut first_timestamp: Option<DateTime<Utc>> = None;
    let mut last_timestamp: Option<DateTime<Utc>> = None;
    let mut skipped_start: Option<(usize, DateTime<Utc>)> = None;
    let mut last_good_timestamp: Option<DateTime<Utc>> = None;

    // Track which files are written/edited (for read filtering)
    let mut written_files: HashSet<String> = HashSet::new();
    // Store reads to process after we know all written files
    let mut pending_reads: Vec<(usize, DateTime<Utc>, i32, String, String, String)> = Vec::new();

    for (line_num, line_result) in reader.lines().enumerate() {
        let line_num = line_num + 1; // 1-indexed
        
        let line = match line_result {
            Ok(l) => l,
            Err(_) => {
                // Malformed line - track for batch warning
                if skipped_start.is_none() {
                    skipped_start = Some((line_num, last_good_timestamp.unwrap_or_else(Utc::now)));
                }
                continue;
            }
        };

        if line.trim().is_empty() {
            continue;
        }

        let entry: SessionEntry = match serde_json::from_str(&line) {
            Ok(e) => e,
            Err(_) => {
                // Malformed JSON - track for batch warning
                if skipped_start.is_none() {
                    skipped_start = Some((line_num, last_good_timestamp.unwrap_or_else(Utc::now)));
                }
                continue;
            }
        };

        // If we were skipping and now have a good line, emit skip warning
        if let Some((start_line, start_ts)) = skipped_start.take() {
            let count = line_num - start_line;
            let (ts, tz) = entry.timestamp.as_deref()
                .and_then(parse_timestamp)
                .unwrap_or((start_ts, 0));
            // Average timestamp
            let avg_ts = DateTime::from_timestamp(
                (start_ts.timestamp() + ts.timestamp()) / 2,
                0
            ).unwrap_or(start_ts);
            
            operations.push(FileOperation {
                timestamp: avg_ts,
                tz_offset_minutes: tz,
                model: current_model.clone(),
                session_id: session_id.clone(),
                op_type: OpType::SkippedLines { count },
                path: String::new(),
                line_number: start_line,
            });
        }

        // Parse timestamp
        let (timestamp, tz_offset) = match entry.timestamp.as_deref().and_then(parse_timestamp) {
            Some((ts, tz)) => {
                last_good_timestamp = Some(ts);
                if first_timestamp.is_none() {
                    first_timestamp = Some(ts);
                }
                last_timestamp = Some(ts);
                (ts, tz)
            }
            None => continue, // Skip entries without valid timestamps
        };

        // Handle session metadata
        if entry.entry_type == "session" {
            if let Some(id) = entry.id {
                session_id = id;
            }
            if let Some(cwd) = entry.cwd.clone() {
                session_cwd = Some(cwd);
            }
            continue;
        }

        // Track model changes
        if entry.entry_type == "model_change" {
            if let Some(model_id) = entry.model_id {
                current_model = model_id;
            }
            continue;
        }

        if entry.entry_type != "message" {
            continue;
        }

        let msg = match entry.message {
            Some(m) => m,
            None => continue,
        };

        // Update model if present in message
        if let Some(ref model) = msg.model {
            current_model = model.clone();
        }

        // Handle tool results (reads)
        if msg.role.as_deref() == Some("toolResult") {
            if let Some(tool_name) = &msg.tool_name {
                if tool_name.to_lowercase() == "read" {
                    if let Some(content) = msg.content {
                        if let Some(text) = content.as_str().or_else(|| {
                            content.get("text").and_then(|t| t.as_str())
                        }) {
                            // We don't know the path from a toolResult directly
                            // This is a limitation - reads don't include path in result
                            // ASSUMPTION: We skip read operations as we can't reliably match them to paths
                        }
                    }
                }
            }
            continue;
        }

        if msg.role.as_deref() != Some("assistant") {
            continue;
        }

        let content = match msg.content {
            Some(c) => c,
            None => continue,
        };

        let content_arr = match content.as_array() {
            Some(arr) => arr,
            None => continue,
        };

        for block in content_arr {
            let call: ToolCall = match serde_json::from_value(block.clone()) {
                Ok(c) => c,
                Err(_) => continue,
            };

            if call.call_type.as_deref() != Some("toolCall") {
                continue;
            }

            let name = match call.name {
                Some(n) => n.to_lowercase(),
                None => continue,
            };

            let args = match call.arguments {
                Some(a) => a,
                None => continue,
            };

            match name.as_str() {
                "write" => {
                    let file_path = args
                        .get("file_path")
                        .or_else(|| args.get("path"))
                        .and_then(|v| v.as_str())
                        .map(String::from);

                    let file_content = args.get("content").and_then(|v| v.as_str()).map(String::from);

                    if let (Some(path), Some(content)) = (file_path, file_content) {
                        written_files.insert(path.clone());
                        
                        if verbose {
                            eprintln!("[{}] write: {}", timestamp, path);
                        }

                        operations.push(FileOperation {
                            timestamp,
                            tz_offset_minutes: tz_offset,
                            model: current_model.clone(),
                            session_id: session_id.clone(),
                            op_type: OpType::Write { content },
                            path,
                            line_number: line_num,
                        });
                    }
                }
                "edit" => {
                    let file_path = args
                        .get("file_path")
                        .or_else(|| args.get("path"))
                        .and_then(|v| v.as_str())
                        .map(String::from);

                    let old_text = args
                        .get("oldText")
                        .or_else(|| args.get("old_string"))
                        .and_then(|v| v.as_str())
                        .map(String::from);

                    let new_text = args
                        .get("newText")
                        .or_else(|| args.get("new_string"))
                        .and_then(|v| v.as_str())
                        .map(String::from)
                        .unwrap_or_default();

                    if let (Some(path), Some(old_text)) = (file_path, old_text) {
                        written_files.insert(path.clone());
                        
                        if verbose {
                            eprintln!("[{}] edit: {}", timestamp, path);
                        }

                        operations.push(FileOperation {
                            timestamp,
                            tz_offset_minutes: tz_offset,
                            model: current_model.clone(),
                            session_id: session_id.clone(),
                            op_type: OpType::Edit { old_text, new_text },
                            path,
                            line_number: line_num,
                        });
                    }
                }
                "read" => {
                    // We'll handle reads via tool results, but we need the path from here
                    let file_path = args
                        .get("file_path")
                        .or_else(|| args.get("path"))
                        .and_then(|v| v.as_str())
                        .map(String::from);
                    
                    if let Some(path) = file_path {
                        // Store for later - we need to match with toolResult
                        // ASSUMPTION: Skipping read-based context commits for now
                        // as matching toolCall to toolResult is complex
                        if verbose {
                            eprintln!("[{}] read: {} (skipped - context commits not yet implemented)", timestamp, path);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    // Handle any trailing skipped lines
    if let Some((start_line, start_ts)) = skipped_start {
        let count = 1; // At least one
        operations.push(FileOperation {
            timestamp: start_ts,
            tz_offset_minutes: 0,
            model: current_model.clone(),
            session_id: session_id.clone(),
            op_type: OpType::SkippedLines { count },
            path: String::new(),
            line_number: start_line,
        });
    }

    let first_ts = first_timestamp.ok_or_else(|| {
        anyhow::anyhow!("No valid timestamp found in session log: {}", session_path.display())
    })?;

    let last_ts = last_timestamp.unwrap_or(first_ts);

    Ok((
        SessionInfo {
            id: session_id,
            first_timestamp: first_ts,
            last_timestamp: last_ts,
            cwd: session_cwd,
        },
        operations,
    ))
}

// ============================================================================
// Path handling
// ============================================================================

/// Sanitize path: replace .git components with _.git
fn sanitize_git_path(path: &Path) -> PathBuf {
    let components: Vec<_> = path.components().map(|c| {
        match c {
            Component::Normal(s) => {
                let s_str = s.to_string_lossy();
                if s_str == ".git" {
                    Component::Normal(std::ffi::OsStr::new("_.git"))
                } else {
                    c
                }
            }
            _ => c
        }
    }).collect();
    
    components.iter().collect()
}

/// Convert absolute path to repo-relative path
/// Returns None if path is external and ignore_external is true
fn resolve_path(
    path: &str,
    repo_path: &Path,
    ignore_external: bool,
) -> Option<PathBuf> {
    // Resolve symlinks
    let abs_path = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        repo_path.join(path)
    };
    
    let resolved = abs_path.canonicalize().unwrap_or(abs_path.clone());
    let resolved_repo = repo_path.canonicalize().unwrap_or(repo_path.to_path_buf());

    // Check if inside repo
    if let Ok(rel) = resolved.strip_prefix(&resolved_repo) {
        return Some(sanitize_git_path(rel));
    }

    // External file
    if ignore_external {
        return None;
    }

    // Compute relative path using _../ encoding
    // Find common ancestor
    let mut repo_parts: Vec<_> = resolved_repo.components().collect();
    let mut file_parts: Vec<_> = resolved.components().collect();
    
    let mut common_len = 0;
    for (a, b) in repo_parts.iter().zip(file_parts.iter()) {
        if a == b {
            common_len += 1;
        } else {
            break;
        }
    }

    // Number of _../ needed = remaining repo parts after common
    let up_count = repo_parts.len() - common_len;
    
    // Build path: _../^up_count / remaining file parts
    let mut result = PathBuf::new();
    for _ in 0..up_count {
        result.push("_..");
    }
    for part in &file_parts[common_len..] {
        result.push(part);
    }

    Some(sanitize_git_path(&result))
}

// ============================================================================
// Git operations
// ============================================================================

/// Check that repo is in clean state
fn verify_clean_state(repo: &Repository) -> Result<()> {
    // Check repo state
    match repo.state() {
        RepositoryState::Clean => {}
        state => bail!("Repository is not in clean state: {:?}. Please resolve before running recovery.", state),
    }

    // Check for uncommitted changes
    let statuses = repo.statuses(None)?;
    for entry in statuses.iter() {
        let status = entry.status();
        if status.intersects(
            git2::Status::INDEX_NEW
            | git2::Status::INDEX_MODIFIED
            | git2::Status::INDEX_DELETED
            | git2::Status::INDEX_RENAMED
            | git2::Status::INDEX_TYPECHANGE
            | git2::Status::WT_NEW
            | git2::Status::WT_MODIFIED
            | git2::Status::WT_DELETED
            | git2::Status::WT_RENAMED
            | git2::Status::WT_TYPECHANGE
        ) {
            bail!("Repository has uncommitted changes. Please commit or stash before running recovery.");
        }
    }

    Ok(())
}

/// Convert model ID to git author
fn model_to_author(model: &str) -> (String, &'static str) {
    const EMAIL: &str = "noreply@anthropic.com";
    
    let model_lower = model.to_lowercase();
    
    let name = if model_lower.contains("opus") {
        format_model_name(model, "Opus")
    } else if model_lower.contains("sonnet") {
        format_model_name(model, "Sonnet")
    } else if model_lower.contains("haiku") {
        format_model_name(model, "Haiku")
    } else if model_lower.contains("claude") {
        "Claude".to_string()
    } else if model_lower.contains("gpt-4") {
        "GPT-4".to_string()
    } else if model_lower.contains("gpt") {
        "GPT".to_string()
    } else {
        // Unknown model - use raw identifier
        model.to_string()
    };
    
    (name, EMAIL)
}

fn format_model_name(model: &str, variant: &str) -> String {
    let model_lower = model.to_lowercase();
    
    let version = if let Some(idx) = model_lower.find(variant.to_lowercase().as_str()) {
        let after_variant = &model_lower[idx + variant.len()..];
        let version_part: String = after_variant
            .chars()
            .skip_while(|c| *c == '-' || *c == '_' || *c == '/')
            .take_while(|c| c.is_ascii_digit() || *c == '-' || *c == '.' || *c == '_')
            .collect();
        
        if !version_part.is_empty() {
            let cleaned = version_part.replace('-', ".").replace('_', ".");
            format!(" {}", cleaned)
        } else {
            String::new()
        }
    } else {
        String::new()
    };
    
    format!("Claude {}{}", variant, version)
}

/// Apply an edit operation, returning result type
fn apply_edit(current: &str, old_text: &str, new_text: &str) -> (String, EditResult) {
    // Try exact match
    if current.contains(old_text) {
        return (current.replacen(old_text, new_text, 1), EditResult::ExactMatch);
    }

    // Try whitespace-normalized match
    let current_normalized: String = current.split_whitespace().collect();
    let old_normalized: String = old_text.split_whitespace().collect();
    
    if current_normalized.contains(&old_normalized) {
        // Find approximate location and replace
        // This is a simplification - just do the replacement
        let result = current.replacen(old_text.trim(), new_text, 1);
        if result != current {
            return (result, EditResult::FuzzyMatch { 
                description: "whitespace-normalized match".to_string() 
            });
        }
    }

    // Fallback: append to end
    let mut result = current.to_string();
    if !result.ends_with('\n') && !result.is_empty() {
        result.push('\n');
    }
    result.push_str("\n// [session-recovery] Failed to match edit target, appending:\n");
    result.push_str(new_text);
    result.push('\n');
    
    (result, EditResult::Appended)
}

/// Build a tree with a file at the given path
fn build_tree_with_file(
    repo: &Repository,
    base_tree: Option<&git2::Tree>,
    file_path: &Path,
    content: &[u8],
) -> Result<Oid> {
    let blob_id = repo.blob(content)?;
    
    // For simple case (single component), just insert directly
    let components: Vec<_> = file_path.components().collect();
    
    if components.len() == 1 {
        let mut builder = repo.treebuilder(base_tree)?;
        builder.insert(
            file_path.to_str().unwrap(),
            blob_id,
            FileMode::Blob.into(),
        )?;
        return Ok(builder.write()?);
    }

    // For nested paths, we need to build the tree hierarchy
    // This is a recursive operation
    fn insert_at_path(
        repo: &Repository,
        base_tree: Option<&git2::Tree>,
        components: &[Component],
        blob_id: Oid,
    ) -> Result<Oid> {
        if components.len() == 1 {
            let mut builder = repo.treebuilder(base_tree)?;
            let name = components[0].as_os_str().to_str().unwrap();
            builder.insert(name, blob_id, FileMode::Blob.into())?;
            return Ok(builder.write()?);
        }

        let dir_name = components[0].as_os_str().to_str().unwrap();
        let rest = &components[1..];

        // Get existing subtree if any
        let existing_subtree = base_tree.and_then(|t| {
            t.get_name(dir_name).and_then(|entry| {
                if entry.kind() == Some(git2::ObjectType::Tree) {
                    entry.to_object(repo).ok().and_then(|o| o.into_tree().ok())
                } else {
                    None
                }
            })
        });

        let subtree_id = insert_at_path(repo, existing_subtree.as_ref(), rest, blob_id)?;

        let mut builder = repo.treebuilder(base_tree)?;
        builder.insert(dir_name, subtree_id, FileMode::Tree.into())?;
        Ok(builder.write()?)
    }

    insert_at_path(repo, base_tree, &components, blob_id)
}

/// Format session IDs for merge message
fn format_session_list(ids: &[String]) -> String {
    match ids.len() {
        0 => String::new(),
        1 => ids[0].clone(),
        2 => format!("{} and {}", ids[0], ids[1]),
        _ => {
            let all_but_last = ids[..ids.len()-1].join(", ");
            format!("{}, and {}", all_but_last, ids.last().unwrap())
        }
    }
}

// ============================================================================
// Main recovery logic
// ============================================================================

fn run_recovery(
    repo: &Repository,
    sessions: Vec<(SessionInfo, Vec<FileOperation>)>,
    repo_path: &Path,
    branch_name: &str,
    ignore_external: bool,
    verbose: bool,
) -> Result<(usize, bool)> {
    let mut all_ops: Vec<FileOperation> = Vec::new();
    let mut session_infos: Vec<SessionInfo> = Vec::new();
    let mut had_errors = false;

    // Collect all operations and add session markers
    for (info, mut ops) in sessions {
        // Add session start marker
        all_ops.push(FileOperation {
            timestamp: info.first_timestamp,
            tz_offset_minutes: 0,
            model: "system".to_string(),
            session_id: info.id.clone(),
            op_type: OpType::SessionStart,
            path: String::new(),
            line_number: 0,
        });

        // Check for skipped lines (errors)
        for op in &ops {
            if matches!(op.op_type, OpType::SkippedLines { .. }) {
                had_errors = true;
            }
        }

        all_ops.append(&mut ops);

        // Add session end marker
        all_ops.push(FileOperation {
            timestamp: info.last_timestamp,
            tz_offset_minutes: 0,
            model: "system".to_string(),
            session_id: info.id.clone(),
            op_type: OpType::SessionEnd,
            path: String::new(),
            line_number: 0,
        });

        session_infos.push(info);
    }

    // Sort by timestamp
    all_ops.sort_by_key(|op| op.timestamp);

    // Count actual file operations
    let file_op_count = all_ops.iter().filter(|op| {
        matches!(op.op_type, OpType::Write { .. } | OpType::Edit { .. } | OpType::Read { .. })
    }).count();

    if file_op_count == 0 {
        bail!("No file operations to recover from session(s)");
    }

    // Track file contents
    let mut file_contents: HashMap<String, String> = HashMap::new();
    let mut commit_count = 0;
    let mut current_tree_id: Option<Oid> = None;
    let mut parent_commit: Option<Oid> = None;
    let mut session_orphans: HashMap<String, Oid> = HashMap::new();
    let mut branch_tip: Option<Oid> = None;

    // Track which session we're in for branch management
    let mut active_sessions: HashSet<String> = HashSet::new();

    for op in &all_ops {
        match &op.op_type {
            OpType::SessionStart => {
                let (author_name, author_email) = ("OpenClaw".to_string(), "noreply@anthropic.com");
                let git_time = Time::new(op.timestamp.timestamp(), op.tz_offset_minutes);
                let sig = Signature::new(&author_name, author_email, &git_time)?;

                // Create orphan commit for this session
                let empty_tree_id = repo.treebuilder(None)?.write()?;
                let empty_tree = repo.find_tree(empty_tree_id)?;
                
                let message = format!("Beginning recovery from OpenClaw session {}", op.session_id);
                let orphan_id = repo.commit(None, &sig, &sig, &message, &empty_tree, &[])?;
                
                session_orphans.insert(op.session_id.clone(), orphan_id);
                commit_count += 1;

                if active_sessions.is_empty() {
                    // First session - this orphan is our branch base
                    parent_commit = Some(orphan_id);
                    current_tree_id = Some(empty_tree_id);
                    
                    // Create branch
                    let branch_ref = format!("refs/heads/{}", branch_name);
                    repo.reference(&branch_ref, orphan_id, true, "session-recovery: initial")?;
                } else {
                    // Merge this orphan into ongoing recovery
                    if let Some(tip) = branch_tip {
                        let tip_commit = repo.find_commit(tip)?;
                        let orphan_commit = repo.find_commit(orphan_id)?;
                        
                        // Create merge commit
                        let tree = repo.find_tree(current_tree_id.unwrap())?;
                        let message = format!("Including OpenClaw session {} in recovery", op.session_id);
                        let merge_id = repo.commit(
                            None, &sig, &sig, &message, &tree,
                            &[&tip_commit, &orphan_commit]
                        )?;
                        
                        parent_commit = Some(merge_id);
                        branch_tip = Some(merge_id);
                        commit_count += 1;

                        // Update branch
                        let branch_ref = format!("refs/heads/{}", branch_name);
                        repo.reference(&branch_ref, merge_id, true, "session-recovery: include session")?;
                    }
                }

                active_sessions.insert(op.session_id.clone());
                
                if verbose {
                    eprintln!("Session start: {}", op.session_id);
                }
            }

            OpType::SessionEnd => {
                let (author_name, author_email) = ("OpenClaw".to_string(), "noreply@anthropic.com");
                let git_time = Time::new(op.timestamp.timestamp(), op.tz_offset_minutes);
                let sig = Signature::new(&author_name, author_email, &git_time)?;

                // Create session end commit
                let tree = repo.find_tree(current_tree_id.unwrap_or_else(|| {
                    repo.treebuilder(None).unwrap().write().unwrap()
                }))?;
                
                let message = format!("Completing recovery from OpenClaw session {}", op.session_id);
                let parent = repo.find_commit(parent_commit.unwrap())?;
                let end_id = repo.commit(None, &sig, &