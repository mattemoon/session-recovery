//! session-recovery — Recover file history from OpenClaw session logs
//!
//! Extracts write/edit operations from .jsonl session files and reconstructs
//! them as a git branch with proper timestamps and authorship.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use clap::Parser;
use git2::{Repository, Signature, Time};
use serde::Deserialize;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(name = "session-recovery")]
#[command(about = "Recover file history from OpenClaw session logs")]
struct Args {
    /// Path to the session .jsonl file
    session: PathBuf,

    /// Repository path (default: current directory)
    #[arg(long, default_value = ".")]
    repo: PathBuf,

    /// Branch name for reconstructed history
    #[arg(long)]
    branch: Option<String>,

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

#[derive(Debug, Clone)]
struct FileOperation {
    timestamp: DateTime<Utc>,
    model: String,
    op_type: OpType,
    path: String,
}

#[derive(Debug, Clone)]
enum OpType {
    Write { content: String },
    Edit { old_text: String, new_text: String },
}

#[derive(Debug, Deserialize)]
struct SessionEntry {
    #[serde(rename = "type")]
    entry_type: String,
    timestamp: Option<String>,
    #[serde(rename = "modelId")]
    model_id: Option<String>,
    message: Option<Message>,
}

#[derive(Debug, Deserialize)]
struct Message {
    role: Option<String>,
    model: Option<String>,
    content: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct ToolCall {
    #[serde(rename = "type")]
    call_type: Option<String>,
    name: Option<String>,
    arguments: Option<serde_json::Value>,
}

fn parse_timestamp(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Convert model ID to git author.
/// Returns (name, email) where email is always noreply@anthropic.com for determinism.
fn model_to_author(model: &str) -> (String, &'static str) {
    const EMAIL: &str = "noreply@anthropic.com";
    
    // Extract a clean model name for the author
    // e.g., "claude-opus-4-5" -> "Claude Opus 4.5"
    //       "anthropic/claude-sonnet-4" -> "Claude Sonnet 4"
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
        model.to_string() // Use raw model ID as fallback
    };
    
    (name, EMAIL)
}

/// Format model name nicely, extracting version if present.
/// e.g., "claude-opus-4-5" -> "Claude Opus 4.5"
fn format_model_name(model: &str, variant: &str) -> String {
    // Try to extract version number
    let model_lower = model.to_lowercase();
    
    // Look for patterns like "4-5", "4.5", "4"
    let version = if let Some(idx) = model_lower.find(variant.to_lowercase().as_str()) {
        let after_variant = &model_lower[idx + variant.len()..];
        // Extract digits and separators
        let version_part: String = after_variant
            .chars()
            .skip_while(|c| *c == '-' || *c == '_' || *c == '/')
            .take_while(|c| c.is_ascii_digit() || *c == '-' || *c == '.' || *c == '_')
            .collect();
        
        if !version_part.is_empty() {
            // Convert "4-5" to "4.5"
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

fn extract_operations(
    session_path: &Path,
    filter: Option<&str>,
    verbose: bool,
) -> Result<(Vec<FileOperation>, Option<DateTime<Utc>>, String)> {
    let file = File::open(session_path)
        .with_context(|| format!("Failed to open session file: {}", session_path.display()))?;
    let reader = BufReader::new(file);

    let mut operations = Vec::new();
    let mut current_model = String::from("unknown");
    let mut first_timestamp: Option<DateTime<Utc>> = None;

    for (line_num, line) in reader.lines().enumerate() {
        let line = line.with_context(|| format!("Failed to read line {}", line_num + 1))?;
        if line.trim().is_empty() {
            continue;
        }

        let entry: SessionEntry = match serde_json::from_str(&line) {
            Ok(e) => e,
            Err(_) => continue,
        };

        // Track first timestamp
        if let Some(ref ts_str) = entry.timestamp {
            if first_timestamp.is_none() {
                first_timestamp = parse_timestamp(ts_str);
            }
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

        let timestamp = match entry.timestamp.as_deref().and_then(parse_timestamp) {
            Some(ts) => ts,
            None => continue, // Skip entries without valid timestamps for determinism
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

                    let content = args.get("content").and_then(|v| v.as_str()).map(String::from);

                    if let (Some(path), Some(content)) = (file_path, content) {
                        // Apply filter
                        if let Some(f) = filter {
                            if !path.contains(f) {
                                continue;
                            }
                        }

                        if verbose {
                            eprintln!("[{}] write: {}", timestamp, path);
                        }

                        operations.push(FileOperation {
                            timestamp,
                            model: current_model.clone(),
                            op_type: OpType::Write { content },
                            path,
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
                        // Apply filter
                        if let Some(f) = filter {
                            if !path.contains(f) {
                                continue;
                            }
                        }

                        if verbose {
                            eprintln!("[{}] edit: {}", timestamp, path);
                        }

                        operations.push(FileOperation {
                            timestamp,
                            model: current_model.clone(),
                            op_type: OpType::Edit { old_text, new_text },
                            path,
                        });
                    }
                }
                _ => {}
            }
        }
    }

    Ok((operations, first_timestamp, current_model))
}

fn make_path_relative(path: &str, repo_path: &Path) -> Option<PathBuf> {
    let path = Path::new(path);

    // Try to make it relative to repo
    if let Ok(rel) = path.strip_prefix(repo_path) {
        return Some(rel.to_path_buf());
    }

    // If path is already relative, use it
    if path.is_relative() {
        return Some(path.to_path_buf());
    }

    None
}

fn replay_operations(
    repo: &Repository,
    operations: &[FileOperation],
    first_timestamp: DateTime<Utc>,
    primary_model: &str,
    branch_name: &str,
    repo_path: &Path,
    verbose: bool,
) -> Result<usize> {
    // Create orphan branch
    // First, get the current HEAD to return to later
    let original_head = repo.head().ok().and_then(|h| h.target());

    // Create initial empty tree
    let tree_builder = repo.treebuilder(None)?;
    let empty_tree_id = tree_builder.write()?;
    let empty_tree = repo.find_tree(empty_tree_id)?;

    // Create initial commit
    // For determinism: author AND committer are both derived from model
    // This ensures identical commit hashes when re-run
    let (author_name, author_email) = model_to_author(primary_model);
    let git_time = Time::new(first_timestamp.timestamp(), 0); // UTC offset = 0
    let author = Signature::new(&author_name, author_email, &git_time)?;
    // Committer = Author for determinism (not from git config)
    let committer = Signature::new(&author_name, author_email, &git_time)?;

    let initial_message = format!(
        "Initial commit (session start)\n\nReconstructed from OpenClaw session log\nSession started: {}\nPrimary model: {}",
        first_timestamp, primary_model
    );

    let initial_commit = repo.commit(None, &author, &committer, &initial_message, &empty_tree, &[])?;

    // Create branch pointing to initial commit
    let branch_ref = format!("refs/heads/{}", branch_name);
    repo.reference(&branch_ref, initial_commit, true, "session-recovery: initial commit")?;

    // Track file contents for edits
    let mut file_contents: HashMap<String, String> = HashMap::new();
    let mut commit_count = 1;
    let mut last_tree_id = empty_tree_id;
    let mut parent_commit_id = initial_commit;

    for op in operations {
        let rel_path = match make_path_relative(&op.path, repo_path) {
            Some(p) => p,
            None => {
                if verbose {
                    eprintln!("Skipping file outside repo: {}", op.path);
                }
                continue;
            }
        };

        let path_str = rel_path.to_string_lossy().to_string();

        let new_content = match &op.op_type {
            OpType::Write { content } => {
                file_contents.insert(path_str.clone(), content.clone());
                content.clone()
            }
            OpType::Edit { old_text, new_text } => {
                let current = file_contents.get(&path_str).cloned().unwrap_or_default();
                let updated = current.replace(old_text, new_text);
                file_contents.insert(path_str.clone(), updated.clone());
                updated
            }
        };

        // Create blob for new content
        let blob_id = repo.blob(new_content.as_bytes())?;

        // Build new tree with this file
        let parent_tree = repo.find_tree(last_tree_id)?;
        let mut tree_builder = repo.treebuilder(Some(&parent_tree))?;

        // Ensure parent directories exist in tree
        let components: Vec<_> = rel_path.components().collect();
        if components.len() > 1 {
            // Need to handle nested paths - for now, just insert at top level
            // This is a simplification; proper implementation would build nested trees
        }

        tree_builder.insert(
            rel_path.file_name().unwrap().to_str().unwrap(),
            blob_id,
            0o100644,
        )?;
        let new_tree_id = tree_builder.write()?;
        let new_tree = repo.find_tree(new_tree_id)?;

        // Create commit
        // For determinism: author AND committer both from model, same timestamp
        let (author_name, author_email) = model_to_author(&op.model);
        let git_time = Time::new(op.timestamp.timestamp(), 0); // UTC offset = 0
        let author = Signature::new(&author_name, author_email, &git_time)?;
        let committer = Signature::new(&author_name, author_email, &git_time)?;

        let op_name = match &op.op_type {
            OpType::Write { .. } => "write",
            OpType::Edit { .. } => "edit",
        };

        let message = format!(
            "{}: {}\n\nModel: {}\nTimestamp: {}",
            op_name, path_str, op.model, op.timestamp
        );

        let parent = repo.find_commit(parent_commit_id)?;
        let new_commit =
            repo.commit(None, &author, &committer, &message, &new_tree, &[&parent])?;

        // Update branch ref
        repo.reference(&branch_ref, new_commit, true, &format!("session-recovery: {}", op_name))?;

        parent_commit_id = new_commit;
        last_tree_id = new_tree_id;
        commit_count += 1;

        if verbose {
            eprintln!("Committed: {} {}", op_name, path_str);
        }
    }

    // Now merge into original HEAD
    if let Some(orig_id) = original_head {
        let branch_commit = repo.find_commit(parent_commit_id)?;

        // Create annotated commit for merge
        let annotated = repo.find_annotated_commit(branch_commit.id())?;

        // Start merge (don't commit)
        let mut merge_opts = git2::MergeOptions::new();
        repo.merge(&[&annotated], Some(&mut merge_opts), None)?;

        eprintln!("\nRepository is now in uncommitted merge state.");
        eprintln!("To complete: git commit");
        eprintln!("To abort: git merge --abort");
    }

    Ok(commit_count)
}

fn main() -> Result<()> {
    let args = Args::parse();

    if !args.session.exists() {
        anyhow::bail!("Session file not found: {}", args.session.display());
    }

    eprintln!("Extracting operations from: {}", args.session.display());

    let (operations, first_timestamp, primary_model) =
        extract_operations(&args.session, args.filter.as_deref(), args.verbose)?;

    eprintln!("Found {} operations", operations.len());

    if operations.is_empty() {
        eprintln!("No operations to replay");
        return Ok(());
    }

    let first_ts = match first_timestamp {
        Some(ts) => ts,
        None => anyhow::bail!("No valid timestamp found in session log — cannot create deterministic commits"),
    };
    eprintln!("First timestamp: {}", first_ts);
    eprintln!("Primary model: {}", primary_model);

    if args.list_only {
        println!("\nOperations:");
        for op in &operations {
            let op_name = match &op.op_type {
                OpType::Write { .. } => "write",
                OpType::Edit { .. } => "edit",
            };
            println!("[{}] {} {} ({})", op.timestamp, op_name, op.path, op.model);
        }
        return Ok(());
    }

    if args.dry_run {
        eprintln!("\n=== DRY RUN ===");
        eprintln!("Would create branch and replay {} operations", operations.len());
        return Ok(());
    }

    // Open repository
    let repo_path = fs::canonicalize(&args.repo)?;
    let repo = Repository::open(&repo_path)
        .with_context(|| format!("Failed to open repository: {}", repo_path.display()))?;

    // Branch name: use explicit name, or derive deterministically from session filename
    let branch_name = args.branch.unwrap_or_else(|| {
        let session_stem = args.session
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown");
        format!("recovered-{}", session_stem)
    });

    eprintln!("Creating branch: {}", branch_name);

    let commit_count = replay_operations(
        &repo,
        &operations,
        first_ts,
        &primary_model,
        &branch_name,
        &repo_path,
        args.verbose,
    )?;

    eprintln!("\nCreated {} commits on branch '{}'", commit_count, branch_name);

    Ok(())
}
