//! session-recovery — Recover file history from OpenClaw session logs
//!
//! See DESIGN.md for full specification.

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Duration, Utc};
use clap::Parser;
use git2::{FileMode, Oid, Repository, RepositoryState, Signature, Time};
use glob::Pattern;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Component, Path, PathBuf};

/// Default time range: 64*64*16*16 seconds ≈ 1,193 days ≈ 3.27 years
const DEFAULT_SINCE_SECONDS: i64 = 64 * 64 * 16 * 16;

#[derive(Parser)]
#[command(name = "session-recovery")]
#[command(about = "Recover file history from OpenClaw session logs")]
struct Args {
    /// Session .jsonl files to recover (optional if --scan-sessions)
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

    /// Collapse consecutive additive operations (default: true)
    #[arg(long, default_value = "true")]
    collapse: bool,

    /// Disable commit collapsing
    #[arg(long)]
    no_collapse: bool,

    /// Dry run - show what would be done
    #[arg(long)]
    dry_run: bool,

    /// List operations only
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
    is_user_interaction: bool, // true if this breaks a collapse sequence
}

#[derive(Debug, Clone)]
enum OpKind {
    Write(String),
    Edit { old: String, new: String },
    Start,
    End,
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

/// Check if a tool call is "safe" (read-only, no side effects)
fn is_safe_tool(name: &str) -> bool {
    let safe = ["read", "web_search", "web_fetch", "grep", "find", "ls", "cat", "head", "tail", "glob"];
    safe.iter().any(|s| name.eq_ignore_ascii_case(s))
}

/// Check if an edit is additive-only (new text contains old text)
fn is_additive_edit(old: &str, new: &str) -> bool {
    // If old is empty, it's additive
    if old.is_empty() { return true; }
    // If new contains old as substring, it's additive
    new.contains(old)
}

fn extract(
    path: &Path,
    includes: &[Pattern],
    excludes: &[Pattern],
    ignore_external: bool,
    repo_path: &Path,
    verbose: bool,
) -> Result<(String, DateTime<Utc>, DateTime<Utc>, Vec<Op>)> {
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
        if first_ts.is_none() { first_ts = Some(ts); }
        last_ts = Some(ts);

        if e.typ == "session" { if let Some(id) = e.id { sid = id; } continue; }
        if e.typ == "model_change" { if let Some(m) = e.model_id { model = m; } continue; }
        if e.typ != "message" { continue; }

        let msg = match e.message { Some(m) => m, None => continue };
        if let Some(m) = &msg.model { model = m.clone(); }

        // Track user messages (break collapse sequences)
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

            // Non-safe tool calls break collapse sequences
            let breaks_collapse = last_was_user || !is_safe_tool(tool_name);

            let fpath = args.get("file_path").or(args.get("path")).and_then(|v| v.as_str());

            match tool_name {
                "write" => {
                    let (p, c) = match (fpath, args.get("content").and_then(|v| v.as_str())) {
                        (Some(p), Some(c)) => (p, c), _ => continue
                    };

                    // Apply path filters
                    if !should_include_path(p, includes, excludes, ignore_external, repo_path) {
                        continue;
                    }

                    if verbose { eprintln!("[{}] write: {}", ts, p); }
                    ops.push(Op {
                        ts, tz, model: model.clone(), session: sid.clone(),
                        kind: OpKind::Write(c.into()), path: p.into(),
                        is_user_interaction: breaks_collapse,
                    });
                    last_was_user = false;
                }
                "edit" => {
                    let old = args.get("oldText").or(args.get("old_string")).and_then(|v| v.as_str());
                    let new = args.get("newText").or(args.get("new_string")).and_then(|v| v.as_str()).unwrap_or("");
                    let (p, o) = match (fpath, old) { (Some(p), Some(o)) => (p, o), _ => continue };

                    if !should_include_path(p, includes, excludes, ignore_external, repo_path) {
                        continue;
                    }

                    if verbose { eprintln!("[{}] edit: {}", ts, p); }
                    ops.push(Op {
                        ts, tz, model: model.clone(), session: sid.clone(),
                        kind: OpKind::Edit { old: o.into(), new: new.into() }, path: p.into(),
                        is_user_interaction: breaks_collapse,
                    });
                    last_was_user = false;
                }
                _ => {
                    // Other tool calls may break collapse
                    if !is_safe_tool(tool_name) {
                        last_was_user = true;
                    }
                }
            }
        }
    }

    let ft = first_ts.ok_or_else(|| anyhow::anyhow!("no timestamps in {}", path.display()))?;
    let lt = last_ts.unwrap_or(ft);
    Ok((sid, ft, lt, ops))
}

fn should_include_path(
    path: &str,
    includes: &[Pattern],
    excludes: &[Pattern],
    ignore_external: bool,
    repo_path: &Path,
) -> bool {
    // Check external
    if ignore_external {
        let abs = if Path::new(path).is_absolute() { PathBuf::from(path) } else { repo_path.join(path) };
        let resolved = abs.canonicalize().unwrap_or(abs);
        let repo_resolved = repo_path.canonicalize().unwrap_or(repo_path.to_path_buf());
        if resolved.strip_prefix(&repo_resolved).is_err() {
            return false;
        }
    }

    // Check includes (if any specified, must match at least one)
    if !includes.is_empty() {
        let matches_include = includes.iter().any(|p| p.matches(path));
        if !matches_include { return false; }
    }

    // Check excludes
    let matches_exclude = excludes.iter().any(|p| p.matches(path));
    if matches_exclude { return false; }

    true
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

fn resolve(path: &str, repo: &Path, ignore_ext: bool) -> Option<PathBuf> {
    let abs = if Path::new(path).is_absolute() { PathBuf::from(path) } else { repo.join(path) };
    let resolved = abs.canonicalize().unwrap_or_else(|_| abs.clone());
    let repo_resolved = repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf());

    if let Ok(rel) = resolved.strip_prefix(&repo_resolved) {
        return Some(sanitize(rel));
    }
    if ignore_ext { return None; }

    // External: use _../ encoding
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
    // Append fallback - just add blank lines and the new content
    let mut r = cur.to_string();
    if !r.is_empty() && !r.ends_with('\n') { r.push('\n'); }
    r.push_str("\n\n\n"); // Three blank lines to separate
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
        bail!("repo not clean: {:?}", repo.state());
    }
    for e in repo.statuses(None)?.iter() {
        let s = e.status();
        if s.intersects(git2::Status::INDEX_NEW | git2::Status::INDEX_MODIFIED |
                        git2::Status::INDEX_DELETED | git2::Status::WT_NEW |
                        git2::Status::WT_MODIFIED | git2::Status::WT_DELETED) {
            bail!("uncommitted changes");
        }
    }
    Ok(())
}

fn scan_sessions(
    dir: &Path,
    includes: &[Pattern],
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    repo_path: &Path,
    verbose: bool,
) -> Result<Vec<PathBuf>> {
    let mut sessions = Vec::new();

    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }

        // Quick scan to check if session has relevant operations
        let file = File::open(&path)?;
        let rdr = BufReader::new(file);

        let mut has_relevant_ops = false;
        let mut in_time_range = false;

        for line in rdr.lines().flatten().take(10000) {
            if line.trim().is_empty() { continue; }
            let e: Entry = match serde_json::from_str(&line) { Ok(e) => e, Err(_) => continue };

            // Check timestamp
            if let Some((ts, _)) = e.timestamp.as_deref().and_then(parse_ts) {
                if ts >= since && ts <= until {
                    in_time_range = true;
                }
            }

            // Check for file operations matching includes
            if e.typ == "message" {
                if let Some(msg) = &e.message {
                    if let Some(arr) = msg.content.as_ref().and_then(|c| c.as_array()) {
                        for blk in arr {
                            if blk.get("type").and_then(|v| v.as_str()) != Some("toolCall") { continue; }
                            let name = blk.get("name").and_then(|v| v.as_str()).map(|s| s.to_lowercase());
                            if name.as_deref() != Some("write") && name.as_deref() != Some("edit") { continue; }

                            if let Some(args) = blk.get("arguments") {
                                if let Some(path) = args.get("file_path").or(args.get("path")).and_then(|v| v.as_str()) {
                                    // Check if path matches includes
                                    if includes.is_empty() || includes.iter().any(|p| p.matches(path)) {
                                        has_relevant_ops = true;
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
            }

            if has_relevant_ops && in_time_range { break; }
        }

        if has_relevant_ops && in_time_range {
            if verbose { eprintln!("Found session: {}", path.display()); }
            sessions.push(path);
        }
    }

    // Sort by filename (which includes timestamp for OpenClaw sessions)
    sessions.sort();
    Ok(sessions)
}

fn print_summary(args: &Args, sessions: &[PathBuf], includes: &[Pattern], excludes: &[Pattern], since: DateTime<Utc>, until: DateTime<Utc>) {
    eprintln!("session-recovery");
    eprintln!("━━━━━━━━━━━━━━━━━━━━━━━");
    eprintln!("Repository:      {} {}", args.repo.display(), if args.repo == PathBuf::from(".") { "(default)" } else { "" });
    eprintln!("Sessions:        {} {}", sessions.len(), if args.scan_sessions { "(auto-discovered)" } else { "(explicit)" });
    if !includes.is_empty() {
        eprintln!("Include:         {:?}", args.includes);
    }
    if !excludes.is_empty() {
        eprintln!("Exclude:         {:?}", args.excludes);
    }
    eprintln!("Ignore external: {}", if args.ignore_external { "yes" } else { "no (default)" });
    eprintln!("Time range:      {} to {}", since.format("%Y-%m-%d"), until.format("%Y-%m-%d"));
    eprintln!("Collapse:        {}", if args.no_collapse { "no" } else { "yes (default)" });
    eprintln!("Dry run:         {}", if args.dry_run { "yes" } else { "no" });
    eprintln!();
}

fn main() -> Result<()> {
    let args = Args::parse();

    // Parse glob patterns
    let includes: Vec<Pattern> = args.includes.iter()
        .map(|s| Pattern::new(s).context("invalid include pattern"))
        .collect::<Result<Vec<_>>>()?;
    let excludes: Vec<Pattern> = args.excludes.iter()
        .map(|s| Pattern::new(s).context("invalid exclude pattern"))
        .collect::<Result<Vec<_>>>()?;

    // Parse time range
    let now = Utc::now();
    let since = match &args.since {
        Some(s) => DateTime::parse_from_rfc3339(s)
            .map(|d| d.with_timezone(&Utc))
            .unwrap_or_else(|_| now - Duration::days(s.parse::<i64>().unwrap_or(30))),
        None => now - Duration::seconds(DEFAULT_SINCE_SECONDS),
    };
    let until = match &args.until {
        Some(s) => DateTime::parse_from_rfc3339(s)
            .map(|d| d.with_timezone(&Utc))
            .unwrap_or(now),
        None => now,
    };

    let repo_path = fs::canonicalize(&args.repo)?;

    // Determine sessions
    let sessions: Vec<PathBuf> = if args.scan_sessions || args.sessions.is_empty() {
        let dir = expand_home(&args.sessions_dir);
        if !dir.exists() {
            bail!("sessions directory not found: {}", dir.display());
        }
        scan_sessions(&dir, &includes, since, until, &repo_path, args.verbose)?
    } else {
        args.sessions.iter().map(|p| {
            if !p.exists() { bail!("session not found: {}", p.display()); }
            Ok(p.clone())
        }).collect::<Result<Vec<_>>>()?
    };

    if sessions.is_empty() {
        bail!("no sessions found");
    }

    print_summary(&args, &sessions, &includes, &excludes, since, until);

    // Extract all sessions
    let mut all_sessions = Vec::new();
    for sp in &sessions {
        let (sid, ft, lt, ops) = extract(sp, &includes, &excludes, args.ignore_external, &repo_path, args.verbose)?;
        if !ops.is_empty() || args.verbose {
            eprintln!("Session {}: {} ops", sid, ops.len());
        }
        all_sessions.push((sid, ft, lt, ops));
    }

    // Build combined operation list with markers
    let mut all_ops = Vec::new();
    let mut session_ids = Vec::new();

    for (sid, ft, lt, mut ops) in all_sessions {
        all_ops.push(Op { ts: ft, tz: 0, model: "system".into(), session: sid.clone(), kind: OpKind::Start, path: String::new(), is_user_interaction: true });
        all_ops.append(&mut ops);
        all_ops.push(Op { ts: lt, tz: 0, model: "system".into(), session: sid.clone(), kind: OpKind::End, path: String::new(), is_user_interaction: true });
        session_ids.push(sid);
    }

    all_ops.sort_by_key(|o| o.ts);

    let file_ops = all_ops.iter().filter(|o| matches!(o.kind, OpKind::Write(_) | OpKind::Edit {..})).count();
    if file_ops == 0 { bail!("no file operations"); }

    eprintln!("Total: {} operations ({} file ops)", all_ops.len(), file_ops);

    if args.list_only {
        for o in &all_ops {
            let k = match &o.kind {
                OpKind::Write(_) => "write", OpKind::Edit {..} => "edit",
                OpKind::Start => "start", OpKind::End => "end",
            };
            println!("[{}] {} {} ({})", o.ts, k, o.path, o.session);
        }
        return Ok(());
    }

    if args.dry_run {
        eprintln!("DRY RUN: would create ~{} commits", all_ops.len());
        return Ok(());
    }

    // Open repo and verify clean
    let repo = Repository::open(&repo_path)?;
    verify_clean(&repo)?;

    let orig_head = repo.head().ok().and_then(|h| h.target());
    let branch = args.branch.unwrap_or_else(|| format!("recovered-{}", session_ids.first().unwrap()));

    eprintln!("Creating branch: {}", branch);

    // State
    let mut files: HashMap<String, String> = HashMap::new();
    let mut tree_id: Option<Oid> = None;
    let mut parent: Option<Oid> = None;
    let mut commits = 0;
    let mut errors = false;
    let mut seen_sessions: HashSet<String> = HashSet::new();
    let branch_ref = format!("refs/heads/{}", branch);

    // Collapsing state
    let collapse = !args.no_collapse;
    let mut pending_collapse: Vec<&Op> = Vec::new();

    let commit_ops = |repo: &Repository, ops: &[&Op], files: &mut HashMap<String, String>,
                      tree_id: &mut Option<Oid>, parent: &mut Option<Oid>, commits: &mut usize,
                      errors: &mut bool, branch_ref: &str| -> Result<()> {
        if ops.is_empty() { return Ok(()); }

        let last_op = ops.last().unwrap();
        let (aname, aemail) = model_author(&last_op.model);
        let gt = Time::new(last_op.ts.timestamp(), last_op.tz);
        let sig = Signature::new(&aname, aemail, &gt)?;

        // Apply all ops to files and tree
        for op in ops {
            let rp = match resolve(&op.path, &repo_path, args.ignore_external) {
                Some(p) => p, None => continue
            };
            let ps = rp.to_string_lossy().to_string();

            let (content, ok) = match &op.kind {
                OpKind::Write(c) => {
                    files.insert(ps.clone(), c.clone());
                    (c.clone(), true)
                }
                OpKind::Edit { old, new } => {
                    let cur = files.get(&ps).cloned().unwrap_or_default();
                    let (updated, ok) = apply_edit(&cur, old, new);
                    files.insert(ps.clone(), updated.clone());
                    (updated, ok)
                }
                _ => continue,
            };

            if !ok { *errors = true; }

            let blob = repo.blob(content.as_bytes())?;
            let base = tree_id.and_then(|t| repo.find_tree(t).ok());
            *tree_id = Some(insert_file(repo, base.as_ref(), &rp, blob)?);
        }

        // Create commit
        let t = repo.find_tree(tree_id.unwrap())?;
        let msg = if ops.len() == 1 {
            let op = ops[0];
            match &op.kind {
                OpKind::Write(_) => format!("write: {}", op.path),
                OpKind::Edit { old, new } => {
                    if files.get(&op.path).map(|c| c.contains(old)).unwrap_or(false) || is_additive_edit(old, new) {
                        format!("edit: {}", op.path)
                    } else {
                        format!("⚠️ edit (appended): {}", op.path)
                    }
                }
                _ => "".into(),
            }
        } else {
            format!("[{} ops] edit: {}", ops.len(), ops.last().unwrap().path)
        };

        let pc = repo.find_commit(parent.unwrap())?;
        let oid = repo.commit(None, &sig, &sig, &msg, &t, &[&pc])?;
        *parent = Some(oid);
        *commits += 1;
        repo.reference(branch_ref, oid, true, "op")?;

        Ok(())
    };

    for op in &all_ops {
        match &op.kind {
            OpKind::Start => {
                // Flush any pending
                if !pending_collapse.is_empty() {
                    commit_ops(&repo, &pending_collapse, &mut files, &mut tree_id, &mut parent, &mut commits, &mut errors, &branch_ref)?;
                    pending_collapse.clear();
                }

                let (aname, aemail) = ("OpenClaw".into(), "noreply@anthropic.com");
                let gt = Time::new(op.ts.timestamp(), op.tz);
                let sig = Signature::new(&aname, aemail, &gt)?;

                let empty = repo.treebuilder(None)?.write()?;
                let etree = repo.find_tree(empty)?;
                let msg = format!("Beginning recovery from OpenClaw session {}", op.session);
                let oid = repo.commit(None, &sig, &sig, &msg, &etree, &[])?;
                commits += 1;

                if seen_sessions.is_empty() {
                    parent = Some(oid);
                    tree_id = Some(empty);
                    repo.reference(&branch_ref, oid, true, "init")?;
                } else if let Some(p) = parent {
                    let pc = repo.find_commit(p)?;
                    let oc = repo.find_commit(oid)?;
                    let t