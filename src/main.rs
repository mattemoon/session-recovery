//! session-recovery — Recover file history from OpenClaw and Claude Code session logs
//!
//! See DESIGN.md for full specification.
//! See OUTPUT_FORMAT.md for CLI output design.
//! See CLAUDE_CODE_SUPPORT.md for multi-format support.

mod consolidate;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Duration, Utc};
use clap::Parser;
use git2::{FileMode, Oid, Repository, RepositoryState, Signature, Time};
use glob::Pattern;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Component, Path, PathBuf};

/// Default time range: ~3 years (in seconds)
const DEFAULT_SINCE_SECONDS: i64 = 3 * 365 * 24 * 60 * 60;

/// Max gap between operations for consolidation (64 * 32 = 2048 seconds ≈ 34 minutes)
const CONSOLIDATION_MAX_GAP_SECONDS: i64 = 64 * 32;

/// Format a consolidated commit message with deduplicated operations
fn format_batch_commit_message(
    ops: &[(String, &str, String)], // (path, kind, session)
    session_formats: &HashMap<String, String>,
) -> String {
    use std::collections::BTreeMap;
    
    // Count operations per (kind, path) pair, preserving order with BTreeMap
    let mut op_counts: BTreeMap<(String, String), usize> = BTreeMap::new();
    for (path, kind, _) in ops {
        let key = (kind.to_string(), path.clone());
        *op_counts.entry(key).or_insert(0) += 1;
    }
    
    let mut msg = String::new();
    for ((kind, path), count) in &op_counts {
        if *count > 1 {
            msg.push_str(&format!("{}: {} (×{})\n", kind, path, count));
        } else {
            msg.push_str(&format!("{}: {}\n", kind, path));
        }
    }
    
    msg.push('\n');
    
    // Deduplicated session IDs
    let sessions: HashSet<_> = ops.iter().map(|(_, _, s)| s.as_str()).collect();
    for session in sessions {
        let format_name = session_formats.get(session).map(|s| s.as_str()).unwrap_or("Session");
        msg.push_str(&format!("{} session {}\n", format_name, session));
    }
    
    msg
}

/// Session log format
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogFormat {
    OpenClaw,
    ClaudeCode,
    Unknown,
}

impl std::fmt::Display for LogFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LogFormat::OpenClaw => write!(f, "openclaw"),
            LogFormat::ClaudeCode => write!(f, "claude-code"),
            LogFormat::Unknown => write!(f, "unknown"),
        }
    }
}

/// Truncate commit ID to 12 characters
fn short_oid(oid: Oid) -> String {
    oid.to_string()[..12].to_string()
}

#[derive(Parser)]
#[command(name = "session-recovery")]
#[command(about = "Recover file history from OpenClaw and Claude Code session logs")]
#[command(version)]
struct Args {
    /// Session .jsonl files to recover (optional if --scan-sessions or --at)
    sessions: Vec<PathBuf>,

    /// Target repository path
    #[arg(long, default_value = ".")]
    repo: PathBuf,

    /// Recovery branch name
    #[arg(long)]
    branch: Option<String>,

    /// Include files matching glob pattern (can repeat)
    #[arg(long = "include", value_name = "GLOB")]
    includes: Vec<String>,

    /// Exclude files matching glob pattern (can repeat)
    #[arg(long = "exclude", value_name = "GLOB")]
    excludes: Vec<String>,

    /// Ignore files outside the repository
    #[arg(long)]
    ignore_external: bool,

    /// Auto-discover sessions from directory
    #[arg(long)]
    scan_sessions: bool,

    /// OpenClaw sessions directory
    #[arg(long, default_value = "~/.openclaw/agents/main/sessions/")]
    sessions_dir: String,

    /// Claude Code projects directory (scans all subdirectories)
    #[arg(long, default_value = "~/.claude/projects/")]
    claude_sessions_dir: String,

    /// Only scan OpenClaw sessions (skip Claude Code)
    #[arg(long)]
    openclaw_only: bool,

    /// Only scan Claude Code sessions (skip OpenClaw)
    #[arg(long)]
    claude_only: bool,

    /// Only include sessions with activity after this time
    #[arg(long)]
    since: Option<String>,

    /// Only include sessions with activity before this time  
    #[arg(long)]
    until: Option<String>,

    /// Point-in-time recovery: path@timestamp
    #[arg(long = "at", value_name = "PATH@TIME")]
    at: Option<String>,

    /// Lookback window for --at (default: 14d)
    #[arg(long, default_value = "14d")]
    lookback: String,

    /// Disable commit collapsing (consolidation of rapid consecutive edits)
    /// When enabled (default), consecutive operations within 2048 seconds
    /// are consolidated into single commits unless they conflict.
    #[arg(long)]
    no_collapse: bool,

    /// Remove this prefix from file paths
    #[arg(long)]
    strip_prefix: Option<String>,

    /// Add this prefix to file paths
    #[arg(long)]
    add_prefix: Option<String>,

    /// Actually update branch refs (default: preview only, commits still created)
    #[arg(long, visible_alias = "yes")]
    confirm: bool,

    /// Merge strategy for recovery branch
    #[arg(long, value_name = "ours|theirs", default_value = "ours")]
    merge: String,

    /// List operations only (detailed preview)
    #[arg(long)]
    list_only: bool,

    /// Verbose output
    #[arg(short, long)]
    verbose: bool,
}

#[derive(Debug, Clone)]
struct Op {
    ts: DateTime<Utc>,
    tz: i32,
    model: String,
    session: String,
    kind: OpKind,
    path: String,
}

#[derive(Debug, Clone)]
enum OpKind {
    Write(String),
    Edit { old: String, new: String },
    Start,
    End,
}

/// Tracking information for a recovered session
#[derive(Debug)]
struct SessionInfo {
    id: String,
    format: LogFormat,
    first_ts: DateTime<Utc>,
    last_ts: DateTime<Utc>,
    op_count: usize,
    first_commit: Option<Oid>,
    last_commit: Option<Oid>,
}

/// Tracking information for a recovered file
#[derive(Debug, Default)]
struct FileInfo {
    sessions: HashSet<String>,
    versions: usize,
}

/// Warning about recovery issues
#[derive(Debug)]
struct Warning {
    path: String,
    ts: DateTime<Utc>,
    message: String,
    commit: Option<Oid>,
}

#[derive(Debug, Deserialize)]
struct Entry {
    #[serde(rename = "type")]
    typ: String,
    timestamp: Option<String>,
    #[serde(rename = "modelId")]
    model_id: Option<String>,
    message: Option<Msg>,
    id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Msg {
    role: Option<String>,
    model: Option<String>,
    content: Option<serde_json::Value>,
}

fn parse_ts(s: &str) -> Option<(DateTime<Utc>, i32)> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| (dt.with_timezone(&Utc), dt.offset().local_minus_utc() / 60))
}

fn expand_home(p: &str) -> PathBuf {
    if p.starts_with("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(&p[2..]);
        }
    }
    PathBuf::from(p)
}

fn parse_duration(s: &str) -> Duration {
    let s = s.trim();
    if let Some(n) = s.strip_suffix('d') {
        Duration::days(n.parse().unwrap_or(14))
    } else if let Some(n) = s.strip_suffix('w') {
        Duration::weeks(n.parse().unwrap_or(2))
    } else if let Some(n) = s.strip_suffix('h') {
        Duration::hours(n.parse().unwrap_or(24))
    } else {
        Duration::days(14)
    }
}

fn is_safe_tool(name: &str) -> bool {
    let safe = ["read", "web_search", "web_fetch", "grep", "find", "ls", "cat", "head", "tail", "glob"];
    safe.iter().any(|s| name.eq_ignore_ascii_case(s))
}

fn should_include_path(path: &str, includes: &[Pattern], excludes: &[Pattern], ignore_external: bool, repo_path: &Path) -> bool {
    if ignore_external {
        let abs = if Path::new(path).is_absolute() { PathBuf::from(path) } else { repo_path.join(path) };
        let resolved = abs.canonicalize().unwrap_or(abs);
        let repo_resolved = repo_path.canonicalize().unwrap_or(repo_path.to_path_buf());
        if resolved.strip_prefix(&repo_resolved).is_err() {
            return false;
        }
    }
    if !includes.is_empty() && !includes.iter().any(|p| p.matches(path)) {
        return false;
    }
    if excludes.iter().any(|p| p.matches(path)) {
        return false;
    }
    true
}

/// Detect log format from first few lines of a session file
fn detect_log_format(path: &Path) -> LogFormat {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return LogFormat::Unknown,
    };
    let rdr = BufReader::new(file);
    
    for line in rdr.lines().take(50).flatten() {
        if line.trim().is_empty() { continue; }
        
        // Claude Code markers: "type":"assistant" with tool_use, or has "version" field with semver
        if line.contains(r#""type":"assistant""#) || line.contains(r#""type":"tool_use""#) {
            return LogFormat::ClaudeCode;
        }
        
        // OpenClaw markers: "type":"message" with toolCall
        if line.contains(r#""type":"message""#) || line.contains(r#""type":"toolCall""#) {
            return LogFormat::OpenClaw;
        }
        
        // Claude Code also has version field like "version":"2.1.39"
        if line.contains(r#""version":"2."#) || line.contains(r#""version":"3."#) {
            return LogFormat::ClaudeCode;
        }
        
        // OpenClaw session entry
        if line.contains(r#""type":"session""#) || line.contains(r#""type":"model_change""#) {
            return LogFormat::OpenClaw;
        }
    }
    
    LogFormat::Unknown
}

fn extract(path: &Path, includes: &[Pattern], excludes: &[Pattern], ignore_external: bool, repo_path: &Path, cutoff: Option<DateTime<Utc>>, verbose: bool) -> Result<(String, LogFormat, DateTime<Utc>, DateTime<Utc>, Vec<Op>)> {
    let format = detect_log_format(path);
    
    match format {
        LogFormat::ClaudeCode => extract_claude_code(path, includes, excludes, ignore_external, repo_path, cutoff, verbose),
        LogFormat::OpenClaw | LogFormat::Unknown => extract_openclaw(path, includes, excludes, ignore_external, repo_path, cutoff, verbose, format),
    }
}

fn extract_openclaw(path: &Path, includes: &[Pattern], excludes: &[Pattern], ignore_external: bool, repo_path: &Path, cutoff: Option<DateTime<Utc>>, verbose: bool, format: LogFormat) -> Result<(String, LogFormat, DateTime<Utc>, DateTime<Utc>, Vec<Op>)> {
    let file = File::open(path).with_context(|| format!("open: {}", path.display()))?;
    let rdr = BufReader::new(file);
    
    let mut ops = Vec::new();
    let mut model = "unknown".to_string();
    let mut sid = path.file_stem().and_then(|s| s.to_str()).unwrap_or("x").to_string();
    let mut first_ts: Option<DateTime<Utc>> = None;
    let mut last_ts: Option<DateTime<Utc>> = None;
    let mut _last_was_user = false;

    for line in rdr.lines().flatten() {
        if line.trim().is_empty() { continue; }
        let e: Entry = match serde_json::from_str(&line) { Ok(e) => e, Err(_) => continue };
        
        let (ts, tz) = match e.timestamp.as_deref().and_then(parse_ts) {
            Some(t) => t, None => continue
        };
        
        // Stop at cutoff if specified
        if let Some(cut) = cutoff {
            if ts > cut { continue; }
        }
        
        if first_ts.is_none() { first_ts = Some(ts); }
        last_ts = Some(ts);
        
        if e.typ == "session" { if let Some(id) = e.id { sid = id; } continue; }
        if e.typ == "model_change" { if let Some(m) = e.model_id { model = m; } continue; }
        if e.typ != "message" { continue; }
        
        let msg = match e.message { Some(m) => m, None => continue };
        if let Some(m) = &msg.model { model = m.clone(); }
        
        if msg.role.as_deref() == Some("user") {
            _last_was_user = true;
            continue;
        }
        
        if msg.role.as_deref() != Some("assistant") { continue; }
        
        let arr = match msg.content.as_ref().and_then(|c| c.as_array()) { Some(a) => a, None => continue };
        
        for blk in arr {
            let typ = blk.get("type").and_then(|v| v.as_str());
            let name = blk.get("name").and_then(|v| v.as_str()).map(|s| s.to_lowercase());
            let args = blk.get("arguments");
            
            if typ != Some("toolCall") { continue; }
            let args = match args { Some(a) => a, None => continue };
            let tool_name = name.as_deref().unwrap_or("");
            
            let fpath = args.get("file_path").or(args.get("path")).and_then(|v| v.as_str());
            
            match tool_name {
                "write" => {
                    let (p, c) = match (fpath, args.get("content").and_then(|v| v.as_str())) {
                        (Some(p), Some(c)) => (p, c), _ => continue
                    };
                    if !should_include_path(p, includes, excludes, ignore_external, repo_path) { continue; }
                    if verbose { eprintln!("  [{}] write: {}", ts.format("%H:%M:%S"), p); }
                    ops.push(Op { ts, tz, model: model.clone(), session: sid.clone(), kind: OpKind::Write(c.into()), path: p.into() });
                    _last_was_user = false;
                }
                "edit" => {
                    let old = args.get("oldText").or(args.get("old_string")).and_then(|v| v.as_str());
                    let new = args.get("newText").or(args.get("new_string")).and_then(|v| v.as_str()).unwrap_or("");
                    let (p, o) = match (fpath, old) { (Some(p), Some(o)) => (p, o), _ => continue };
                    if !should_include_path(p, includes, excludes, ignore_external, repo_path) { continue; }
                    if verbose { eprintln!("  [{}] edit: {}", ts.format("%H:%M:%S"), p); }
                    ops.push(Op { ts, tz, model: model.clone(), session: sid.clone(), kind: OpKind::Edit { old: o.into(), new: new.into() }, path: p.into() });
                    _last_was_user = false;
                }
                _ => {
                    if !is_safe_tool(tool_name) { _last_was_user = true; }
                }
            }
        }
    }
    
    let ft = first_ts.ok_or_else(|| anyhow::anyhow!("no timestamps in {}", path.display()))?;
    let lt = last_ts.unwrap_or(ft);
    Ok((sid, format, ft, lt, ops))
}

/// Extract operations from Claude Code session logs
fn extract_claude_code(path: &Path, includes: &[Pattern], excludes: &[Pattern], ignore_external: bool, repo_path: &Path, cutoff: Option<DateTime<Utc>>, verbose: bool) -> Result<(String, LogFormat, DateTime<Utc>, DateTime<Utc>, Vec<Op>)> {
    let file = File::open(path).with_context(|| format!("open: {}", path.display()))?;
    let rdr = BufReader::new(file);
    
    let mut ops = Vec::new();
    let mut model = "unknown".to_string();
    let mut sid = path.file_stem().and_then(|s| s.to_str()).unwrap_or("x").to_string();
    let mut cwd: Option<String> = None;
    let mut first_ts: Option<DateTime<Utc>> = None;
    let mut last_ts: Option<DateTime<Utc>> = None;

    for line in rdr.lines().flatten() {
        if line.trim().is_empty() { continue; }
        let e: serde_json::Value = match serde_json::from_str(&line) { Ok(e) => e, Err(_) => continue };
        
        // Extract timestamp
        let ts_str = e.get("timestamp").and_then(|v| v.as_str());
        let (ts, tz) = match ts_str.and_then(parse_ts) {
            Some(t) => t, 
            None => continue
        };
        
        // Stop at cutoff if specified
        if let Some(cut) = cutoff {
            if ts > cut { continue; }
        }
        
        if first_ts.is_none() { first_ts = Some(ts); }
        last_ts = Some(ts);
        
        // Extract session ID from message
        if let Some(session_id) = e.get("sessionId").and_then(|v| v.as_str()) {
            sid = session_id.to_string();
        }
        
        // Extract working directory
        if let Some(c) = e.get("cwd").and_then(|v| v.as_str()) {
            cwd = Some(c.to_string());
        }
        
        // Only process assistant messages
        let typ = e.get("type").and_then(|v| v.as_str());
        if typ != Some("assistant") { continue; }
        
        // Extract model
        if let Some(msg) = e.get("message") {
            if let Some(m) = msg.get("model").and_then(|v| v.as_str()) {
                model = m.to_string();
            }
            
            // Process tool_use blocks in content
            if let Some(content) = msg.get("content").and_then(|v| v.as_array()) {
                for blk in content {
                    let blk_type = blk.get("type").and_then(|v| v.as_str());
                    if blk_type != Some("tool_use") { continue; }
                    
                    let name = blk.get("name").and_then(|v| v.as_str()).map(|s| s.to_lowercase());
                    let input = match blk.get("input") { Some(i) => i, None => continue };
                    let tool_name = name.as_deref().unwrap_or("");
                    
                    let fpath = input.get("file_path").or(input.get("path")).and_then(|v| v.as_str());
                    
                    // Resolve relative paths against cwd
                    let resolved_path = match fpath {
                        Some(p) if !Path::new(p).is_absolute() => {
                            if let Some(ref c) = cwd {
                                PathBuf::from(c).join(p).to_string_lossy().to_string()
                            } else {
                                p.to_string()
                            }
                        }
                        Some(p) => p.to_string(),
                        None => continue,
                    };
                    
                    match tool_name {
                        "write" => {
                            let content = match input.get("content").and_then(|v| v.as_str()) {
                                Some(c) => c,
                                None => continue,
                            };
                            if !should_include_path(&resolved_path, includes, excludes, ignore_external, repo_path) { continue; }
                            if verbose { eprintln!("  [{}] write: {}", ts.format("%H:%M:%S"), resolved_path); }
                            ops.push(Op { ts, tz, model: model.clone(), session: sid.clone(), kind: OpKind::Write(content.into()), path: resolved_path });
                        }
                        "edit" => {
                            let old = input.get("old_string").and_then(|v| v.as_str());
                            let new = input.get("new_string").and_then(|v| v.as_str()).unwrap_or("");
                            let old_text = match old { Some(o) => o, None => continue };
                            if !should_include_path(&resolved_path, includes, excludes, ignore_external, repo_path) { continue; }
                            if verbose { eprintln!("  [{}] edit: {}", ts.format("%H:%M:%S"), resolved_path); }
                            ops.push(Op { ts, tz, model: model.clone(), session: sid.clone(), kind: OpKind::Edit { old: old_text.into(), new: new.into() }, path: resolved_path });
                        }
                        "multiedit" => {
                            // MultiEdit: array of edits, all with same timestamp
                            if let Some(edits) = input.get("edits").and_then(|v| v.as_array()) {
                                for edit in edits {
                                    let edit_path = edit.get("file_path").or(edit.get("path")).and_then(|v| v.as_str());
                                    let edit_resolved = match edit_path {
                                        Some(p) if !Path::new(p).is_absolute() => {
                                            if let Some(ref c) = cwd {
                                                PathBuf::from(c).join(p).to_string_lossy().to_string()
                                            } else {
                                                p.to_string()
                                            }
                                        }
                                        Some(p) => p.to_string(),
                                        None => continue,
                                    };
                                    let old = edit.get("old_string").and_then(|v| v.as_str());
                                    let new = edit.get("new_string").and_then(|v| v.as_str()).unwrap_or("");
                                    let old_text = match old { Some(o) => o, None => continue };
                                    if !should_include_path(&edit_resolved, includes, excludes, ignore_external, repo_path) { continue; }
                                    if verbose { eprintln!("  [{}] edit (multi): {}", ts.format("%H:%M:%S"), edit_resolved); }
                                    ops.push(Op { ts, tz, model: model.clone(), session: sid.clone(), kind: OpKind::Edit { old: old_text.into(), new: new.into() }, path: edit_resolved });
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    
    let ft = first_ts.ok_or_else(|| anyhow::anyhow!("no timestamps in {}", path.display()))?;
    let lt = last_ts.unwrap_or(ft);
    Ok((sid, LogFormat::ClaudeCode, ft, lt, ops))
}

fn sanitize(p: &Path) -> PathBuf {
    p.components().map(|c| {
        if let Component::Normal(s) = c {
            if s.to_string_lossy() == ".git" {
                return Component::Normal(std::ffi::OsStr::new("_.git"));
            }
        }
        c
    }).collect()
}

fn remap_path(path: &str, strip_prefix: Option<&str>, add_prefix: Option<&str>) -> String {
    let mut result = path.to_string();
    
    if let Some(prefix) = strip_prefix {
        if result.starts_with(prefix) {
            result = result[prefix.len()..].to_string();
            if result.starts_with('/') {
                result = result[1..].to_string();
            }
        }
    }
    
    if let Some(prefix) = add_prefix {
        result = format!("{}{}", prefix, result);
    }
    
    result
}

fn resolve(path: &str, repo: &Path, ignore_ext: bool, strip_prefix: Option<&str>, add_prefix: Option<&str>) -> Option<PathBuf> {
    let remapped = remap_path(path, strip_prefix, add_prefix);
    let path = &remapped;
    
    let abs = if Path::new(path).is_absolute() { PathBuf::from(path) } else { repo.join(path) };
    let resolved = abs.canonicalize().unwrap_or_else(|_| abs.clone());
    let repo_resolved = repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf());
    
    if let Ok(rel) = resolved.strip_prefix(&repo_resolved) {
        return Some(sanitize(rel));
    }
    if ignore_ext { return None; }
    
    let rparts: Vec<_> = repo_resolved.components().collect();
    let fparts: Vec<_> = resolved.components().collect();
    let common = rparts.iter().zip(&fparts).take_while(|(a,b)| a == b).count();
    let ups = rparts.len() - common;
    
    let mut result = PathBuf::new();
    for _ in 0..ups { result.push("_.."); }
    for p in &fparts[common..] { result.push(p); }
    Some(sanitize(&result))
}

fn model_author(m: &str) -> (String, &'static str) {
    let ml = m.to_lowercase();
    let name = if ml.contains("opus") { format!("Claude Opus{}", version(&ml, "opus")) }
        else if ml.contains("sonnet") { format!("Claude Sonnet{}", version(&ml, "sonnet")) }
        else if ml.contains("haiku") { format!("Claude Haiku{}", version(&ml, "haiku")) }
        else if ml.contains("claude") { "Claude".into() }
        else { m.into() };
    (name, "noreply@anthropic.com")
}

fn version(m: &str, v: &str) -> String {
    if let Some(i) = m.find(v) {
        let after: String = m[i+v.len()..].chars()
            .skip_while(|c| *c == '-' || *c == '_' || *c == '/')
            .take_while(|c| c.is_ascii_digit() || *c == '-' || *c == '.')
            .collect();
        if !after.is_empty() { return format!(" {}", after.replace('-', ".")); }
    }
    String::new()
}

fn apply_edit(cur: &str, old: &str, new: &str) -> (String, bool) {
    if cur.contains(old) { return (cur.replacen(old, new, 1), true); }
    // Mismatch: append with separators and trailing blank line
    let mut r = cur.to_string();
    if !r.is_empty() && !r.ends_with('\n') { r.push('\n'); }
    r.push_str("\n\n\n");
    r.push_str(new);
    if !new.ends_with('\n') { r.push('\n'); }
    r.push('\n'); // trailing blank line for mismatched edits
    (r, false)
}

fn insert_file(repo: &Repository, base: Option<&git2::Tree>, path: &Path, blob: Oid) -> Result<Oid> {
    let comps: Vec<_> = path.components().collect();
    if comps.is_empty() { bail!("empty path"); }
    
    fn rec(repo: &Repository, base: Option<&git2::Tree>, comps: &[Component], blob: Oid) -> Result<Oid> {
        let name = comps[0].as_os_str().to_str().unwrap();
        if comps.len() == 1 {
            let mut b = repo.treebuilder(base)?;
            b.insert(name, blob, FileMode::Blob.into())?;
            return Ok(b.write()?);
        }
        let sub = base.and_then(|t| t.get_name(name))
            .and_then(|e| e.to_object(repo).ok())
            .and_then(|o| o.into_tree().ok());
        let sub_id = rec(repo, sub.as_ref(), &comps[1..], blob)?;
        let mut b = repo.treebuilder(base)?;
        b.insert(name, sub_id, FileMode::Tree.into())?;
        Ok(b.write()?)
    }
    rec(repo, base, &comps, blob)
}

fn verify_clean(repo: &Repository) -> Result<()> {
    if repo.state() != RepositoryState::Clean {
        bail!("Repository not in clean state: {:?}", repo.state());
    }
    for e in repo.statuses(None)?.iter() {
        let s = e.status();
        if s.intersects(git2::Status::INDEX_NEW | git2::Status::INDEX_MODIFIED | git2::Status::INDEX_DELETED | git2::Status::WT_NEW | git2::Status::WT_MODIFIED | git2::Status::WT_DELETED) {
            bail!("Uncommitted changes in repository. Please commit or stash first.");
        }
    }
    Ok(())
}

/// Check if a session file has matching operations (format-aware)
fn session_has_matching_ops(path: &Path, includes: &[Pattern], since: DateTime<Utc>, until: DateTime<Utc>) -> bool {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let rdr = BufReader::new(file);
    let format = detect_log_format(path);
    let mut has_ops = false;
    let mut in_range = false;
    
    for line in rdr.lines().flatten().take(10000) {
        if line.trim().is_empty() { continue; }
        let e: serde_json::Value = match serde_json::from_str(&line) { Ok(e) => e, Err(_) => continue };
        
        // Check timestamp
        if let Some(ts_str) = e.get("timestamp").and_then(|v| v.as_str()) {
            if let Some((ts, _)) = parse_ts(ts_str) {
                if ts >= since && ts <= until { in_range = true; }
            }
        }
        
        match format {
            LogFormat::OpenClaw | LogFormat::Unknown => {
                let typ = e.get("type").and_then(|v| v.as_str());
                if typ == Some("message") {
                    if let Some(msg) = e.get("message") {
                        if let Some(arr) = msg.get("content").and_then(|c| c.as_array()) {
                            for blk in arr {
                                if blk.get("type").and_then(|v| v.as_str()) != Some("toolCall") { continue; }
                                let name = blk.get("name").and_then(|v| v.as_str()).map(|s| s.to_lowercase());
                                if name.as_deref() != Some("write") && name.as_deref() != Some("edit") { continue; }
                                if let Some(args) = blk.get("arguments") {
                                    if let Some(p) = args.get("file_path").or(args.get("path")).and_then(|v| v.as_str()) {
                                        if includes.is_empty() || includes.iter().any(|pat| pat.matches(p)) {
                                            has_ops = true;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            LogFormat::ClaudeCode => {
                let typ = e.get("type").and_then(|v| v.as_str());
                if typ == Some("assistant") {
                    if let Some(msg) = e.get("message") {
                        if let Some(arr) = msg.get("content").and_then(|c| c.as_array()) {
                            for blk in arr {
                                if blk.get("type").and_then(|v| v.as_str()) != Some("tool_use") { continue; }
                                let name = blk.get("name").and_then(|v| v.as_str()).map(|s| s.to_lowercase());
                                if name.as_deref() != Some("write") && name.as_deref() != Some("edit") && name.as_deref() != Some("multiedit") { continue; }
                                if let Some(input) = blk.get("input") {
                                    if let Some(p) = input.get("file_path").or(input.get("path")).and_then(|v| v.as_str()) {
                                        if includes.is_empty() || includes.iter().any(|pat| pat.matches(p)) {
                                            has_ops = true;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        
        if has_ops && in_range { break; }
    }
    
    has_ops && in_range
}

/// Scan OpenClaw sessions directory (flat structure)
fn scan_openclaw_sessions(dir: &Path, includes: &[Pattern], since: DateTime<Utc>, until: DateTime<Utc>, verbose: bool) -> Result<Vec<PathBuf>> {
    let mut sessions = Vec::new();
    
    if !dir.exists() {
        return Ok(sessions);
    }
    
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") { continue; }
        
        if session_has_matching_ops(&path, includes, since, until) {
            if verbose { eprintln!("  [openclaw] Found: {}", path.file_name().unwrap_or_default().to_string_lossy()); }
            sessions.push(path);
        }
    }
    
    sessions.sort();
    Ok(sessions)
}

/// Scan Claude Code projects directory (has project subdirectories)
fn scan_claude_code_sessions(dir: &Path, includes: &[Pattern], since: DateTime<Utc>, until: DateTime<Utc>, verbose: bool) -> Result<Vec<PathBuf>> {
    let mut sessions = Vec::new();
    
    if !dir.exists() {
        return Ok(sessions);
    }
    
    // Claude Code structure: ~/.claude/projects/{project-slug}/{session-id}.jsonl
    for project_entry in fs::read_dir(dir)? {
        let project_entry = project_entry?;
        let project_path = project_entry.path();
        if !project_path.is_dir() { continue; }
        
        for session_entry in fs::read_dir(&project_path).into_iter().flatten().flatten() {
            let path = session_entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") { continue; }
            
            if session_has_matching_ops(&path, includes, since, until) {
                if verbose { 
                    let project_name = project_path.file_name().unwrap_or_default().to_string_lossy();
                    eprintln!("  [claude-code/{}] Found: {}", project_name, path.file_name().unwrap_or_default().to_string_lossy()); 
                }
                sessions.push(path);
            }
        }
    }
    
    sessions.sort();
    Ok(sessions)
}

/// Scan all session sources
fn scan_sessions(openclaw_dir: &Path, claude_code_dir: &Path, includes: &[Pattern], since: DateTime<Utc>, until: DateTime<Utc>, verbose: bool, openclaw_only: bool, claude_only: bool) -> Result<Vec<PathBuf>> {
    let mut all_sessions = Vec::new();
    
    if !claude_only {
        let openclaw = scan_openclaw_sessions(openclaw_dir, includes, since, until, verbose)?;
        all_sessions.extend(openclaw);
    }
    
    if !openclaw_only {
        let claude = scan_claude_code_sessions(claude_code_dir, includes, since, until, verbose)?;
        all_sessions.extend(claude);
    }
    
    // Sort by path for consistent ordering
    all_sessions.sort();
    Ok(all_sessions)
}

fn format_date_range(first: DateTime<Utc>, last: DateTime<Utc>) -> String {
    let f = first.format("%Y-%m-%d").to_string();
    let l = last.format("%Y-%m-%d").to_string();
    if f == l { f } else { format!("{} to {}", f, l) }
}

fn print_header(args: &Args, branch: &str, _since: DateTime<Utc>, _until: DateTime<Utc>) {
    eprintln!("session-recovery v{}", env!("CARGO_PKG_VERSION"));
    eprintln!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    eprintln!();
    eprintln!("Configuration");
    eprintln!("  Repository:      {}", args.repo.display());
    eprintln!("  Target branch:   {}", branch);
    let mode_desc = if args.confirm {
        "apply (creating branch and preparing merge)"
    } else {
        "preview (commits created but refs unchanged)"
    };
    eprintln!("  Mode:            {}", mode_desc);
    eprintln!("  Merge strategy:  -s {}", args.merge);
    eprintln!();
}

fn print_filters(args: &Args, includes: &[Pattern], excludes: &[Pattern], since: DateTime<Utc>, until: DateTime<Utc>) {
    let has_filters = !includes.is_empty() || !excludes.is_empty() || args.ignore_external 
        || args.strip_prefix.is_some() || args.add_prefix.is_some();
    
    if has_filters || args.verbose {
        eprintln!("Filters");
        if !includes.is_empty() {
            eprintln!("  Include:         {}", args.includes.join(", "));
        }
        if !excludes.is_empty() {
            eprintln!("  Exclude:         {}", args.excludes.join(", "));
        }
        if args.ignore_external {
            eprintln!("  Ignore external: yes");
        }
        eprintln!("  Time range:      {} to {}", since.format("%Y-%m-%d"), until.format("%Y-%m-%d"));
        if args.strip_prefix.is_some() || args.add_prefix.is_some() {
            let mut remap = String::new();
            if let Some(ref s) = args.strip_prefix {
                remap.push_str(&format!("--strip-prefix {} ", s));
            }
            if let Some(ref a) = args.add_prefix {
                remap.push_str(&format!("--add-prefix {}", a));
            }
            eprintln!("  Path remap:      {}", remap.trim());
        }
        eprintln!();
    }
}

fn print_sessions(session_infos: &[SessionInfo]) {
    eprintln!("Sessions ({} found)", session_infos.len());
    for si in session_infos {
        let range = format_date_range(si.first_ts, si.last_ts);
        eprintln!("  • {} [{}] ({}, {} ops)", &si.id[..8], si.format, range, si.op_count);
    }
    eprintln!();
}

fn print_files(file_infos: &BTreeMap<String, FileInfo>, _verbose: bool) {
    eprintln!("Files to Recover");
    for (path, info) in file_infos {
        let session_word = if info.sessions.len() == 1 { "session" } else { "sessions" };
        eprintln!("  {}  ({} versions from {} {})", 
            path, info.versions, info.sessions.len(), session_word);
    }
    eprintln!();
}

fn print_processing_result(session_infos: &[SessionInfo], total_commits: usize, warnings: &[Warning]) {
    eprintln!("Processing...");
    for si in session_infos {
        if let (Some(first), Some(last)) = (si.first_commit, si.last_commit) {
            eprintln!("  ✓ Session {} → {} commits ({}..{})", 
                &si.id[..8], 
                si.op_count + 2, // +2 for start/end markers
                short_oid(first), 
                short_oid(last));
        }
    }
    eprintln!();
    
    // Count by format
    let openclaw_count = session_infos.iter().filter(|s| s.format == LogFormat::OpenClaw).count();
    let claude_code_count = session_infos.iter().filter(|s| s.format == LogFormat::ClaudeCode).count();
    
    eprintln!("Summary");
    eprintln!("  Total commits:   {} (across {} sessions)", total_commits, session_infos.len());
    if openclaw_count > 0 && claude_code_count > 0 {
        eprintln!("  Sources:         {} OpenClaw, {} Claude Code", openclaw_count, claude_code_count);
    }
    let files_count = session_infos.iter().map(|s| s.op_count).sum::<usize>();
    eprintln!("  File operations: {}", files_count);
    if !warnings.is_empty() {
        eprintln!("  Warnings:        {} (see below)", warnings.len());
    }
    eprintln!();
}

fn print_warnings(warnings: &[Warning]) {
    if warnings.is_empty() { return; }
    
    eprintln!("Warnings");
    for w in warnings {
        eprintln!("  ⚠️  {} @ {}", w.path, w.ts.format("%Y-%m-%dT%H:%M:%SZ"));
        eprintln!("      {}", w.message);
        if let Some(oid) = w.commit {
            eprintln!("      Commit: {}", short_oid(oid));
        }
    }
    eprintln!();
}

fn print_merge_state(branch: &str, last_commit: Oid, _merge_msg: &str, errors: bool, strategy: &str) {
    eprintln!("Branch created: {} @ {}", branch, short_oid(last_commit));
    eprintln!();
    eprintln!("Merge State");
    eprintln!("  Repository is now in an uncommitted merge state.");
    let tree_desc = if strategy == "theirs" {
        "from recovery branch (recovered files)"
    } else {
        "unchanged (current files preserved)"
    };
    eprintln!("  Current tree:    {}", tree_desc);
    eprintln!("  Strategy:        -s {}", strategy);
    eprintln!("  Recovery branch: {}", branch);
    eprintln!();
    eprintln!("  To complete:     git commit");
    eprintln!("  To abort:        git merge --abort");
    eprintln!("  To inspect:      git log --all --graph --oneline -20");
    if errors {
        eprintln!();
        eprintln!("  ⚠️  PARTIAL RECOVERY: Some operations failed or were skipped.");
        eprintln!("      Review warnings above before completing the merge.");
    }
}

fn print_preview_result(total_ops: usize, est_commits: usize, first_commit: Option<Oid>, last_commit: Option<Oid>) {
    eprintln!("Preview complete.");
    eprintln!("  {} file operations would create ~{} commits.", total_ops, est_commits);
    if let (Some(first), Some(last)) = (first_commit, last_commit) {
        eprintln!("  Commit range: {}..{}", short_oid(first), short_oid(last));
        eprintln!();
        eprintln!("  To inspect commits: git show {}", short_oid(first));
    }
    eprintln!();
    eprintln!("To apply this recovery (update refs), run again with --confirm");
}

fn main() -> Result<()> {
    let args = Args::parse();
    
    let includes: Vec<Pattern> = args.includes.iter()
        .map(|s| Pattern::new(s).context("invalid include pattern"))
        .collect::<Result<Vec<_>>>()?;
    let excludes: Vec<Pattern> = args.excludes.iter()
        .map(|s| Pattern::new(s).context("invalid exclude pattern"))
        .collect::<Result<Vec<_>>>()?;
    
    let now = Utc::now();
    
    // Handle --at option
    let (at_path, cutoff) = if let Some(at_str) = &args.at {
        let parts: Vec<&str> = at_str.rsplitn(2, '@').collect();
        if parts.len() != 2 {
            bail!("--at requires format: path@timestamp");
        }
        let ts = DateTime::parse_from_rfc3339(parts[0])
            .map(|d| d.with_timezone(&Utc))
            .unwrap_or(now);
        (Some(parts[1].to_string()), Some(ts))
    } else {
        (None, None)
    };
    
    let since = match &args.since {
        Some(s) => DateTime::parse_from_rfc3339(s).map(|d| d.with_timezone(&Utc)).unwrap_or_else(|_| now - Duration::seconds(DEFAULT_SINCE_SECONDS)),
        None => {
            if let Some(cut) = cutoff {
                cut - parse_duration(&args.lookback)
            } else {
                now - Duration::seconds(DEFAULT_SINCE_SECONDS)
            }
        }
    };
    let until = cutoff.unwrap_or_else(|| {
        args.until.as_ref().and_then(|s| DateTime::parse_from_rfc3339(s).ok().map(|d| d.with_timezone(&Utc))).unwrap_or(now)
    });
    
    let repo_path = fs::canonicalize(&args.repo)?;
    
    // Build include patterns for --at
    let mut effective_includes = includes.clone();
    if let Some(ref p) = at_path {
        effective_includes.push(Pattern::new(&format!("*{}*", p)).unwrap_or_else(|_| Pattern::new(p).unwrap()));
    }
    
    // Validate mutually exclusive flags
    if args.openclaw_only && args.claude_only {
        bail!("Cannot use both --openclaw-only and --claude-only");
    }
    
    // Scan or collect sessions
    let sessions: Vec<PathBuf> = if args.scan_sessions || args.sessions.is_empty() || at_path.is_some() {
        let openclaw_dir = expand_home(&args.sessions_dir);
        let claude_code_dir = expand_home(&args.claude_sessions_dir);
        
        let check_openclaw = !args.claude_only && openclaw_dir.exists();
        let check_claude = !args.openclaw_only && claude_code_dir.exists();
        
        if !check_openclaw && !check_claude { 
            bail!("No session directories found.\n\nTried:\n  OpenClaw: {}\n  Claude Code: {}\n\nTip: Use --sessions-dir or --claude-sessions-dir to specify locations", 
                openclaw_dir.display(), claude_code_dir.display()); 
        }
        if args.verbose { 
            eprintln!("Scanning sessions..."); 
            if check_openclaw { eprintln!("  OpenClaw: {}", openclaw_dir.display()); }
            if check_claude { eprintln!("  Claude Code: {}", claude_code_dir.display()); }
        }
        scan_sessions(&openclaw_dir, &claude_code_dir, &effective_includes, since, until, args.verbose, args.openclaw_only, args.claude_only)?
    } else {
        args.sessions.iter().filter_map(|p| if p.exists() { Some(p.clone()) } else { None }).collect()
    };
    
    if sessions.is_empty() { 
        eprintln!("session-recovery v{}", env!("CARGO_PKG_VERSION"));
        eprintln!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        eprintln!();
        eprintln!("No sessions found matching filters.");
        eprintln!();
        eprintln!("Suggestions:");
        eprintln!("  • Check --include patterns match your target files");
        eprintln!("  • Try --scan-sessions to auto-discover sessions");
        eprintln!("  • Adjust --since/--until time range (current: {} to {})", since.format("%Y-%m-%d"), until.format("%Y-%m-%d"));
        eprintln!("  • Use --verbose to see what's being filtered out");
        bail!("No sessions found");
    }
    
    // Extract operations from sessions
    let mut session_infos: Vec<SessionInfo> = Vec::new();
    let mut file_infos: BTreeMap<String, FileInfo> = BTreeMap::new();
    let mut all_ops = Vec::new();
    
    for sp in &sessions {
        let (sid, format, ft, lt, ops) = extract(sp, &effective_includes, &excludes, args.ignore_external, &repo_path, cutoff, args.verbose)?;
        
        // Track file stats
        for op in &ops {
            let fi = file_infos.entry(op.path.clone()).or_default();
            fi.sessions.insert(sid.clone());
            fi.versions += 1;
        }
        
        session_infos.push(SessionInfo {
            id: sid.clone(),
            format,
            first_ts: ft,
            last_ts: lt,
            op_count: ops.len(),
            first_commit: None,
            last_commit: None,
        });
        
        // Store format prefix for commit messages
        let format_name = match format {
            LogFormat::ClaudeCode => "Claude Code",
            LogFormat::OpenClaw => "OpenClaw",
            LogFormat::Unknown => "unknown",
        };
        
        all_ops.push(Op { ts: ft, tz: 0, model: format_name.into(), session: sid.clone(), kind: OpKind::Start, path: String::new() });
        for op in ops {
            all_ops.push(op);
        }
        all_ops.push(Op { ts: lt, tz: 0, model: format_name.into(), session: sid.clone(), kind: OpKind::End, path: String::new() });
    }
    
    all_ops.sort_by_key(|o| o.ts);
    
    let file_ops = all_ops.iter().filter(|o| matches!(o.kind, OpKind::Write(_) | OpKind::Edit {..})).count();
    if file_ops == 0 { 
        bail!("No file operations found in sessions. Check your --include filters.");
    }
    
    // Determine branch name
    let branch = args.branch.clone().unwrap_or_else(|| {
        format!("recovered-{}", &session_infos[0].id[..8])
    });
    
    // Print header
    print_header(&args, &branch, since, until);
    print_filters(&args, &effective_includes, &excludes, since, until);
    print_sessions(&session_infos);
    print_files(&file_infos, args.verbose);
    
    // List-only mode: just show operations
    if args.list_only {
        eprintln!("Operations ({} total)", all_ops.len());
        let mut current_session = String::new();
        for o in &all_ops {
            if o.session != current_session {
                current_session = o.session.clone();
                let si = session_infos.iter().find(|s| s.id == current_session).unwrap();
                eprintln!();
                eprintln!("Session {} ({})", &current_session[..8], format_date_range(si.first_ts, si.last_ts));
            }
            let k = match &o.kind { 
                OpKind::Write(_) => "write", 
                OpKind::Edit {..} => "edit", 
                OpKind::Start => continue,
                OpKind::End => continue,
            };
            eprintln!("  [{}] {}  {}", o.ts.format("%Y-%m-%dT%H:%M:%SZ"), k, o.path);
        }
        return Ok(());
    }
    
    // Open repository and verify clean state
    let repo = Repository::open(&repo_path)?;
    verify_clean(&repo)?;
    
    let orig_head = repo.head().ok().and_then(|h| h.target());
    
    // Build session ID → format name map
    let session_formats: HashMap<String, String> = session_infos.iter()
        .map(|s| {
            let fmt = match s.format {
                LogFormat::ClaudeCode => "Claude Code",
                LogFormat::OpenClaw => "OpenClaw",
                LogFormat::Unknown => "Session",
            };
            (s.id.clone(), fmt.to_string())
        })
        .collect();
    
    // Group operations into consolidation batches (if enabled)
    // A batch is a sequence of file ops with:
    // - Time gap < 2048 seconds between consecutive ops
    // - Same session
    // - No line conflicts (added lines not also removed)
    let batches: Vec<Vec<usize>> = if args.no_collapse {
        // No consolidation: each file op is its own batch
        all_ops.iter().enumerate()
            .filter(|(_, o)| matches!(o.kind, OpKind::Write(_) | OpKind::Edit { .. }))
            .map(|(i, _)| vec![i])
            .collect()
    } else {
        // Group into batches
        let mut result: Vec<Vec<usize>> = Vec::new();
        let mut current_batch: Vec<usize> = Vec::new();
        let mut last_ts: Option<i64> = None;
        let mut last_session: Option<&str> = None;
        
        for (i, op) in all_ops.iter().enumerate() {
            match &op.kind {
                OpKind::Write(_) | OpKind::Edit { .. } => {
                    let can_extend = match (last_ts, last_session) {
                        (Some(lt), Some(ls)) => {
                            consolidate::can_consolidate(lt, op.ts.timestamp(), ls, &op.session, CONSOLIDATION_MAX_GAP_SECONDS)
                        }
                        _ => false,
                    };
                    
                    if can_extend {
                        current_batch.push(i);
                    } else {
                        if !current_batch.is_empty() {
                            result.push(current_batch);
                        }
                        current_batch = vec![i];
                    }
                    last_ts = Some(op.ts.timestamp());
                    last_session = Some(&op.session);
                }
                OpKind::Start | OpKind::End => {
                    // Session boundaries break batches
                    if !current_batch.is_empty() {
                        result.push(current_batch);
                        current_batch = Vec::new();
                    }
                    last_ts = None;
                    last_session = None;
                }
            }
        }
        if !current_batch.is_empty() {
            result.push(current_batch);
        }
        result
    };
    
    if args.verbose {
        let consolidated_count = batches.iter().filter(|b| b.len() > 1).count();
        if consolidated_count > 0 {
            eprintln!("Consolidation: {} operations → {} batches ({} consolidated)", 
                file_ops, batches.len(), consolidated_count);
            eprintln!();
        }
    }
    
    // Process operations and create commits
    let mut files: HashMap<String, String> = HashMap::new();
    let mut tree_id: Option<Oid> = None;
    let mut parent: Option<Oid> = None;
    let mut total_commits = 0;
    let mut warnings: Vec<Warning> = Vec::new();
    let mut seen_sessions: HashSet<String> = HashSet::new();
    let mut session_commits: HashMap<String, (Option<Oid>, Option<Oid>)> = HashMap::new();
    let branch_ref = format!("refs/heads/{}", branch);
    // Map op index → batch index for quick lookup
    let mut op_to_batch: HashMap<usize, usize> = HashMap::new();
    for (batch_idx, batch) in batches.iter().enumerate() {
        for &op_idx in batch {
            op_to_batch.insert(op_idx, batch_idx);
        }
    }
    
    // Track batch state for consolidated commits
    let mut current_batch_ops: Vec<(String, &str, String)> = Vec::new(); // (path, kind, session)
    let mut current_batch_idx: Option<usize> = None;
    let mut pending_batch_tree: Option<Oid> = None;
    let mut pending_batch_ts: Option<DateTime<Utc>> = None;
    let mut pending_batch_tz: i32 = 0;
    let mut pending_batch_model: String = String::new();
    
    for (op_idx, op) in all_ops.iter().enumerate() {
        match &op.kind {
            OpKind::Start => {
                // op.model contains format name ("OpenClaw", "Claude Code") for Start/End ops
                let source = &op.model;
                let sig = Signature::new(source, "noreply@anthropic.com", &Time::new(op.ts.timestamp(), op.tz))?;
                let empty = repo.treebuilder(None)?.write()?;
                let etree = repo.find_tree(empty)?;
                let msg = format!("Beginning recovery from {} session {}", source, op.session);
                let oid = repo.commit(None, &sig, &sig, &msg, &etree, &[])?;
                total_commits += 1;
                
                session_commits.entry(op.session.clone()).or_insert((None, None)).0 = Some(oid);
                
                if seen_sessions.is_empty() {
                    parent = Some(oid);
                    tree_id = Some(empty);
                    if args.confirm {
                        repo.reference(&branch_ref, oid, true, "init recovery branch")?;
                    }
                } else if let Some(p) = parent {
                    let pc = repo.find_commit(p)?;
                    let oc = repo.find_commit(oid)?;
                    let t = repo.find_tree(tree_id.unwrap())?;
                    let msg = format!("Including {} session {} in recovery", source, op.session);
                    let mid = repo.commit(None, &sig, &sig, &msg, &t, &[&pc, &oc])?;
                    parent = Some(mid);
                    total_commits += 1;
                    if args.confirm {
                        repo.reference(&branch_ref, mid, true, "merge session")?;
                    }
                }
                seen_sessions.insert(op.session.clone());
            }
            OpKind::End => {
                if let Some(tid) = tree_id {
                    let source = &op.model;
                    let sig = Signature::new(source, "noreply@anthropic.com", &Time::new(op.ts.timestamp(), op.tz))?;
                    let t = repo.find_tree(tid)?;
                    let msg = format!("Completing recovery from {} session {}", source, op.session);
                    let pc = repo.find_commit(parent.unwrap())?;
                    let oid = repo.commit(None, &sig, &sig, &msg, &t, &[&pc])?;
                    parent = Some(oid);
                    total_commits += 1;
                    
                    session_commits.entry(op.session.clone()).or_insert((None, None)).1 = Some(oid);
                    
                    if args.confirm {
                        repo.reference(&branch_ref, oid, true, "end session")?;
                    }
                }
            }
            OpKind::Write(content) => {
                let rp = match resolve(&op.path, &repo_path, args.ignore_external, args.strip_prefix.as_deref(), args.add_prefix.as_deref()) { 
                    Some(p) => p, 
                    None => continue 
                };
                let ps = rp.to_string_lossy().to_string();
                files.insert(ps.clone(), content.clone());
                
                let blob = repo.blob(content.as_bytes())?;
                let base = tree_id.and_then(|t| repo.find_tree(t).ok());
                let new_tree = insert_file(&repo, base.as_ref(), &rp, blob)?;
                tree_id = Some(new_tree);
                
                // Track this op for batch
                let batch_idx = op_to_batch.get(&op_idx).copied();
                current_batch_ops.push((ps.clone(), "write", op.session.clone()));
                pending_batch_tree = Some(new_tree);
                pending_batch_ts = Some(op.ts);
                pending_batch_tz = op.tz;
                pending_batch_model = op.model.clone();
                
                // Check if this is the last op in the batch
                let is_batch_end = match batch_idx {
                    Some(bi) => {
                        let batch = &batches[bi];
                        batch.last() == Some(&op_idx)
                    }
                    None => true,
                };
                
                if is_batch_end {
                    // Create commit for this batch
                    let (aname, aemail) = model_author(&pending_batch_model);
                    let sig = Signature::new(&aname, aemail, &Time::new(pending_batch_ts.unwrap().timestamp(), pending_batch_tz))?;
                    let t = repo.find_tree(pending_batch_tree.unwrap())?;
                    
                    // Build commit message
                    let msg = if current_batch_ops.len() == 1 {
                        // Single op: simple message
                        let format_name = session_formats.get(&op.session).map(|s| s.as_str()).unwrap_or("Session");
                        format!("write: {}\n\n{} session {}", ps, format_name, op.session)
                    } else {
                        // Multiple ops: consolidated message with deduplication
                        format_batch_commit_message(&current_batch_ops, &session_formats)
                    };
                    
                    let pc = repo.find_commit(parent.unwrap())?;
                    let oid = repo.commit(None, &sig, &sig, &msg, &t, &[&pc])?;
                    parent = Some(oid);
                    total_commits += 1;
                    
                    if args.confirm {
                        repo.reference(&branch_ref, oid, true, "write")?;
                    }
                    
                    // Clear batch state
                    current_batch_ops.clear();
                }
            }
            OpKind::Edit { old, new } => {
                let rp = match resolve(&op.path, &repo_path, args.ignore_external, args.strip_prefix.as_deref(), args.add_prefix.as_deref()) { 
                    Some(p) => p, 
                    None => continue 
                };
                let ps = rp.to_string_lossy().to_string();
                let cur = files.get(&ps).cloned().unwrap_or_default();
                let (updated, ok) = apply_edit(&cur, old, new);
                files.insert(ps.clone(), updated.clone());
                
                let blob = repo.blob(updated.as_bytes())?;
                let base = tree_id.and_then(|t| repo.find_tree(t).ok());
                let new_tree = insert_file(&repo, base.as_ref(), &rp, blob)?;
                tree_id = Some(new_tree);
                
                // Determine edit kind label
                let kind_label = if ok { "edit" } else { "⚠️ edit (mismatched)" };
                
                // Track this op for batch
                current_batch_ops.push((ps.clone(), kind_label, op.session.clone()));
                pending_batch_tree = Some(new_tree);
                pending_batch_ts = Some(op.ts);
                pending_batch_tz = op.tz;
                pending_batch_model = op.model.clone();
                
                // Check if this is the last op in the batch
                let batch_idx = op_to_batch.get(&op_idx).copied();
                let is_batch_end = match batch_idx {
                    Some(bi) => {
                        let batch = &batches[bi];
                        batch.last() == Some(&op_idx)
                    }
                    None => true,
                };
                
                // Track warning (always, even if batch continues)
                let pending_warning = if !ok {
                    Some(Warning {
                        path: ps.clone(),
                        ts: op.ts,
                        message: "Edit target not found, content appended as mismatched".into(),
                        commit: None, // Will be filled in when we commit
                    })
                } else {
                    None
                };
                
                if is_batch_end {
                    // Create commit for this batch
                    let (aname, aemail) = model_author(&pending_batch_model);
                    let sig = Signature::new(&aname, aemail, &Time::new(pending_batch_ts.unwrap().timestamp(), pending_batch_tz))?;
                    let t = repo.find_tree(pending_batch_tree.unwrap())?;
                    
                    // Build commit message
                    let msg = if current_batch_ops.len() == 1 {
                        // Single op: simple message
                        let format_name = session_formats.get(&op.session).map(|s| s.as_str()).unwrap_or("Session");
                        format!("{}: {}\n\n{} session {}", kind_label, ps, format_name, op.session)
                    } else {
                        // Multiple ops: consolidated message
                        let mut msg = String::new();
                        for (path, kind, _) in &current_batch_ops {
                            msg.push_str(&format!("{}: {}\n", kind, path));
                        }
                        msg.push('\n');
                        let sessions: HashSet<_> = current_batch_ops.iter().map(|(_, _, s)| s.as_str()).collect();
                        for session in sessions {
                            let format_name = session_formats.get(session).map(|s| s.as_str()).unwrap_or("Session");
                            msg.push_str(&format!("{} session {}\n", format_name, session));
                        }
                        msg
                    };
                    
                    let pc = repo.find_commit(parent.unwrap())?;
                    let oid = repo.commit(None, &sig, &sig, &msg, &t, &[&pc])?;
                    parent = Some(oid);
                    total_commits += 1;
                    
                    // Add warning with commit ID
                    if let Some(mut w) = pending_warning {
                        w.commit = Some(oid);
                        warnings.push(w);
                    }
                    
                    if args.confirm {
                        repo.reference(&branch_ref, oid, true, "edit")?;
                    }
                    
                    // Clear batch state
                    current_batch_ops.clear();
                } else if let Some(w) = pending_warning {
                    // Batch continues, but we need to track the warning
                    // We'll add it with the batch commit ID later
                    warnings.push(Warning {
                        path: w.path,
                        ts: w.ts,
                        message: w.message,
                        commit: None, // Unknown until batch commits
                    });
                }
            }
        }
    }
    
    // Update session_infos with commit IDs
    for si in &mut session_infos {
        if let Some((first, last)) = session_commits.get(&si.id) {
            si.first_commit = *first;
            si.last_commit = *last;
        }
    }
    
    // Get first and last commits overall
    let first_commit = session_infos.first().and_then(|s| s.first_commit);
    let last_commit = parent;
    
    // Print results
    print_processing_result(&session_infos, total_commits, &warnings);
    print_warnings(&warnings);
    
    if !args.confirm {
        print_preview_result(file_ops, total_commits, first_commit, last_commit);
        return Ok(());
    }
    
    // Set up merge state (--confirm mode only)
    if let Some(head_id) = orig_head {
        let branch_commit = repo.find_commit(parent.unwrap())?;
        let ann = repo.find_annotated_commit(branch_commit.id())?;
        
        // Validate merge strategy
        let use_theirs = match args.merge.as_str() {
            "ours" => false,
            "theirs" => true,
            other => bail!("Invalid merge strategy '{}'. Use 'ours' or 'theirs'.", other),
        };
        
        repo.merge(&[&ann], None, None)?;
        
        // Checkout the appropriate tree based on strategy
        let tree_to_use = if use_theirs {
            // "theirs" = use the recovery branch's tree (the recovered files)
            branch_commit.tree()?
        } else {
            // "ours" = keep our original tree (just add history)
            let our_commit = repo.find_commit(head_id)?;
            our_commit.tree()?
        };
        repo.checkout_tree(tree_to_use.as_object(), Some(git2::build::CheckoutBuilder::new().force()))?;
        
        // Build session list with format labels
        let session_labels: Vec<_> = session_infos.iter().map(|s| {
            let fmt = match s.format {
                LogFormat::ClaudeCode => "Claude Code",
                LogFormat::OpenClaw => "OpenClaw", 
                LogFormat::Unknown => "unknown",
            };
            format!("{} ({})", &s.id[..8], fmt)
        }).collect();
        let slist = if session_labels.len() == 1 { 
            format!("session {}", session_labels[0]) 
        } else { 
            format!("sessions {}", session_labels.join(", ")) 
        };
        let suffix = if !warnings.is_empty() { " (partial recovery with errors)" } else { "" };
        let mmsg = format!("Merge recovered {}{}", slist, suffix);
        
        let git_dir = repo.path();
        fs::write(git_dir.join("MERGE_MSG"), &mmsg)?;
        
        print_merge_state(&branch, parent.unwrap(), &mmsg, !warnings.is_empty(), &args.merge);
    } else {
        eprintln!("Branch created: {} @ {}", branch, short_oid(parent.unwrap()));
        eprintln!();
        eprintln!("No existing HEAD to merge with.");
        eprintln!("To use this branch: git checkout {}", branch);
    }
    
    Ok(())
}



                    if verbose { eprintln!("  [{}] write: {}", ts.format("%H:%M:%S"), p); }
                    ops.push(Op { ts, tz, model: model.clone(), session: sid.clone(), kind: OpKind::Write(c.into()), path: p.into() });
                    _last_was_user = false;

