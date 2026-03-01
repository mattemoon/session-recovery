//! session-recovery — Recover file history from OpenClaw session logs
//!
//! See DESIGN.md for full specification.
//! See OUTPUT_FORMAT.md for CLI output design.

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

/// Default time range: 64*64*16*16 seconds ≈ 1,193 days ≈ 3.27 years
const DEFAULT_SINCE_SECONDS: i64 = 64 * 64 * 16 * 16;

/// Truncate commit ID to 12 characters
fn short_oid(oid: Oid) -> String {
    oid.to_string()[..12].to_string()
}

#[derive(Parser)]
#[command(name = "session-recovery")]
#[command(about = "Recover file history from OpenClaw session logs")]
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

    /// Directory to scan for sessions
    #[arg(long, default_value = "~/.openclaw/agents/main/sessions/")]
    sessions_dir: String,

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

    /// Disable commit collapsing
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

fn extract(path: &Path, includes: &[Pattern], excludes: &[Pattern], ignore_external: bool, repo_path: &Path, cutoff: Option<DateTime<Utc>>, verbose: bool) -> Result<(String, DateTime<Utc>, DateTime<Utc>, Vec<Op>)> {
    let file = File::open(path).with_context(|| format!("open: {}", path.display()))?;
    let rdr = BufReader::new(file);
    
    let mut ops = Vec::new();
    let mut model = "unknown".to_string();
    let mut sid = path.file_stem().and_then(|s| s.to_str()).unwrap_or("x").to_string();
    let mut first_ts: Option<DateTime<Utc>> = None;
    let mut last_ts: Option<DateTime<Utc>> = None;
    let mut last_was_user = false;

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
            last_was_user = true;
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
                    last_was_user = false;
                }
                "edit" => {
                    let old = args.get("oldText").or(args.get("old_string")).and_then(|v| v.as_str());
                    let new = args.get("newText").or(args.get("new_string")).and_then(|v| v.as_str()).unwrap_or("");
                    let (p, o) = match (fpath, old) { (Some(p), Some(o)) => (p, o), _ => continue };
                    if !should_include_path(p, includes, excludes, ignore_external, repo_path) { continue; }
                    if verbose { eprintln!("  [{}] edit: {}", ts.format("%H:%M:%S"), p); }
                    ops.push(Op { ts, tz, model: model.clone(), session: sid.clone(), kind: OpKind::Edit { old: o.into(), new: new.into() }, path: p.into() });
                    last_was_user = false;
                }
                _ => {
                    if !is_safe_tool(tool_name) { last_was_user = true; }
                }
            }
        }
    }
    
    let ft = first_ts.ok_or_else(|| anyhow::anyhow!("no timestamps in {}", path.display()))?;
    let lt = last_ts.unwrap_or(ft);
    Ok((sid, ft, lt, ops))
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
    let mut r = cur.to_string();
    if !r.is_empty() && !r.ends_with('\n') { r.push('\n'); }
    r.push_str("\n\n\n");
    r.push_str(new);
    if !new.ends_with('\n') { r.push('\n'); }
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

fn scan_sessions(dir: &Path, includes: &[Pattern], since: DateTime<Utc>, until: DateTime<Utc>, verbose: bool) -> Result<Vec<PathBuf>> {
    let mut sessions = Vec::new();
    
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") { continue; }
        
        let file = File::open(&path)?;
        let rdr = BufReader::new(file);
        let mut has_ops = false;
        let mut in_range = false;
        
        for line in rdr.lines().flatten().take(10000) {
            if line.trim().is_empty() { continue; }
            let e: Entry = match serde_json::from_str(&line) { Ok(e) => e, Err(_) => continue };
            
            if let Some((ts, _)) = e.timestamp.as_deref().and_then(parse_ts) {
                if ts >= since && ts <= until { in_range = true; }
            }
            
            if e.typ == "message" {
                if let Some(msg) = &e.message {
                    if let Some(arr) = msg.content.as_ref().and_then(|c| c.as_array()) {
                        for blk in arr {
                            if blk.get("type").and_then(|v| v.as_str()) != Some("toolCall") { continue; }
                            let name = blk.get("name").and_then(|v| v.as_str()).map(|s| s.to_lowercase());
                            if name.as_deref() != Some("write") && name.as_deref() != Some("edit") { continue; }
                            if let Some(args) = blk.get("arguments") {
                                if let Some(p) = args.get("file_path").or(args.get("path")).and_then(|v| v.as_str()) {
                                    if includes.is_empty() || includes.iter().any(|pat| pat.matches(p)) {
                                        has_ops = true;
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if has_ops && in_range { break; }
        }
        
        if has_ops && in_range {
            if verbose { eprintln!("  Found: {}", path.file_name().unwrap_or_default().to_string_lossy()); }
            sessions.push(path);
        }
    }
    
    sessions.sort();
    Ok(sessions)
}

fn format_date_range(first: DateTime<Utc>, last: DateTime<Utc>) -> String {
    let f = first.format("%Y-%m-%d").to_string();
    let l = last.format("%Y-%m-%d").to_string();
    if f == l { f } else { format!("{} to {}", f, l) }
}

fn print_header(args: &Args, branch: &str, since: DateTime<Utc>, until: DateTime<Utc>) {
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
        eprintln!("  • {} ({}, {} ops)", &si.id[..8], range, si.op_count);
    }
    eprintln!();
}

fn print_files(file_infos: &BTreeMap<String, FileInfo>, verbose: bool) {
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
    
    eprintln!("Summary");
    eprintln!("  Total commits:   {} (across {} sessions)", total_commits, session_infos.len());
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

fn print_merge_state(branch: &str, last_commit: Oid, merge_msg: &str, errors: bool) {
    eprintln!("Branch created: {} @ {}", branch, short_oid(last_commit));
    eprintln!();
    eprintln!("Merge State");
    eprintln!("  Repository is now in an uncommitted merge state.");
    eprintln!("  Current tree:    unchanged (--strategy ours)");
    eprintln!("  Recovery branch: {} (merged for history only)", branch);
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
    let mut effective_includes = includes.