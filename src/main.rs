//! session-recovery — Recover file history from OpenClaw session logs
//!
//! See DESIGN.md for full specification.

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use clap::Parser;
use git2::{FileMode, Oid, Repository, RepositoryState, Signature, Time};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Component, Path, PathBuf};

#[derive(Parser)]
#[command(name = "session-recovery")]
#[command(about = "Recover file history from OpenClaw session logs")]
struct Args {
    /// Paths to session .jsonl files
    #[arg(required = true)]
    sessions: Vec<PathBuf>,

    /// Repository path
    #[arg(long, default_value = ".")]
    repo: PathBuf,

    /// Branch name
    #[arg(long)]
    branch: Option<String>,

    /// Ignore external files
    #[arg(long)]
    ignore_external: bool,

    /// Filter by path prefix
    #[arg(long)]
    filter: Option<String>,

    /// Dry run
    #[arg(long)]
    dry_run: bool,

    /// Verbose output
    #[arg(short, long)]
    verbose: bool,

    /// List only
    #[arg(long)]
    list_only: bool,
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
    Skip(usize),
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
    cwd: Option<String>,
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

fn extract(path: &Path, filter: Option<&str>, verbose: bool) -> Result<(String, DateTime<Utc>, DateTime<Utc>, Vec<Op>)> {
    let file = File::open(path).with_context(|| format!("open: {}", path.display()))?;
    let rdr = BufReader::new(file);
    
    let mut ops = Vec::new();
    let mut model = "unknown".to_string();
    let mut sid = path.file_stem().and_then(|s| s.to_str()).unwrap_or("x").to_string();
    let mut first_ts: Option<DateTime<Utc>> = None;
    let mut last_ts: Option<DateTime<Utc>> = None;

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
        if msg.role.as_deref() != Some("assistant") { continue; }
        
        let arr = match msg.content.as_ref().and_then(|c| c.as_array()) { Some(a) => a, None => continue };
        
        for blk in arr {
            let typ = blk.get("type").and_then(|v| v.as_str());
            let name = blk.get("name").and_then(|v| v.as_str()).map(|s| s.to_lowercase());
            let args = blk.get("arguments");
            
            if typ != Some("toolCall") { continue; }
            let args = match args { Some(a) => a, None => continue };
            
            let fpath = args.get("file_path").or(args.get("path")).and_then(|v| v.as_str());
            
            match name.as_deref() {
                Some("write") => {
                    let (p, c) = match (fpath, args.get("content").and_then(|v| v.as_str())) {
                        (Some(p), Some(c)) => (p, c), _ => continue
                    };
                    if let Some(f) = filter { if !p.contains(f) { continue; } }
                    if verbose { eprintln!("[{}] write: {}", ts, p); }
                    ops.push(Op { ts, tz, model: model.clone(), session: sid.clone(), kind: OpKind::Write(c.into()), path: p.into() });
                }
                Some("edit") => {
                    let old = args.get("oldText").or(args.get("old_string")).and_then(|v| v.as_str());
                    let new = args.get("newText").or(args.get("new_string")).and_then(|v| v.as_str()).unwrap_or("");
                    let (p, o) = match (fpath, old) { (Some(p), Some(o)) => (p, o), _ => continue };
                    if let Some(f) = filter { if !p.contains(f) { continue; } }
                    if verbose { eprintln!("[{}] edit: {}", ts, p); }
                    ops.push(Op { ts, tz, model: model.clone(), session: sid.clone(), kind: OpKind::Edit { old: o.into(), new: new.into() }, path: p.into() });
                }
                _ => {}
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

fn resolve(path: &str, repo: &Path, ignore_ext: bool) -> Option<PathBuf> {
    let abs = if Path::new(path).is_absolute() { PathBuf::from(path) } else { repo.join(path) };
    let resolved = abs.canonicalize().unwrap_or_else(|_| abs.clone());
    let repo_resolved = repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf());
    
    if let Ok(rel) = resolved.strip_prefix(&repo_resolved) {
        return Some(sanitize(rel));
    }
    if ignore_ext { return None; }
    
    // External: use _../ encoding
    let mut rparts: Vec<_> = repo_resolved.components().collect();
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
    // Append fallback
    let mut r = cur.to_string();
    if !r.is_empty() && !r.ends_with('\n') { r.push('\n'); }
    r.push_str("\n// [recovery] edit target not found, appending:\n");
    r.push_str(new);
    r.push('\n');
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

fn main() -> Result<()> {
    let args = Args::parse();
    
    if args.sessions.is_empty() { bail!("no sessions"); }
    
    // Extract all sessions
    let mut all_sessions = Vec::new();
    for sp in &args.sessions {
        if !sp.exists() { bail!("not found: {}", sp.display()); }
        let (sid, ft, lt, ops) = extract(sp, args.filter.as_deref(), args.verbose)?;
        eprintln!("Session {}: {} ops", sid, ops.len());
        all_sessions.push((sid, ft, lt, ops));
    }
    
    // Build combined operation list with markers
    let mut all_ops = Vec::new();
    let mut session_ids = Vec::new();
    
    for (sid, ft, lt, mut ops) in all_sessions {
        all_ops.push(Op { ts: ft, tz: 0, model: "system".into(), session: sid.clone(), kind: OpKind::Start, path: String::new() });
        all_ops.append(&mut ops);
        all_ops.push(Op { ts: lt, tz: 0, model: "system".into(), session: sid.clone(), kind: OpKind::End, path: String::new() });
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
                OpKind::Start => "start", OpKind::End => "end", OpKind::Skip(n) => "skip",
            };
            println!("[{}] {} {} ({})", o.ts, k, o.path, o.session);
        }
        return Ok(());
    }
    
    if args.dry_run {
        eprintln!("DRY RUN: would create {} commits", all_ops.len());
        return Ok(());
    }
    
    // Open repo and verify clean
    let repo_path = std::fs::canonicalize(&args.repo)?;
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
    
    for op in &all_ops {
        let (aname, aemail) = if op.model == "system" { ("OpenClaw".into(), "noreply@anthropic.com") } else { model_author(&op.model) };
        let gt = Time::new(op.ts.timestamp(), op.tz);
        let sig = Signature::new(&aname, aemail, &gt)?;
        
        match &op.kind {
            OpKind::Start => {
                // Create orphan
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
                    // Merge orphan
                    let pc = repo.find_commit(p)?;
                    let oc = repo.find_commit(oid)?;
                    let t = repo.find_tree(tree_id.unwrap())?;
                    let msg = format!("Including OpenClaw session {} in recovery", op.session);
                    let mid = repo.commit(None, &sig, &sig, &msg, &t, &[&pc, &oc])?;
                    parent = Some(mid);
                    commits += 1;
                    repo.reference(&branch_ref, mid, true, "merge")?;
                }
                seen_sessions.insert(op.session.clone());
            }
            
            OpKind::End => {
                if let Some(tid) = tree_id {
                    let t = repo.find_tree(tid)?;
                    let msg = format!("Completing recovery from OpenClaw session {}", op.session);
                    let pc = repo.find_commit(parent.unwrap())?;
                    let oid = repo.commit(None, &sig, &sig, &msg, &t, &[&pc])?;
                    parent = Some(oid);
                    commits += 1;
                    repo.reference(&branch_ref, oid, true, "end")?;
                }
            }
            
            OpKind::Write(content) => {
                let rp = match resolve(&op.path, &repo_path, args.ignore_external) {
                    Some(p) => p, None => { if args.verbose { eprintln!("skip external: {}", op.path); } continue; }
                };
                let ps = rp.to_string_lossy().to_string();
                files.insert(ps.clone(), content.clone());
                
                let blob = repo.blob(content.as_bytes())?;
                let base = tree_id.and_then(|t| repo.find_tree(t).ok());
                let new_tree = insert_file(&repo, base.as_ref(), &rp, blob)?;
                tree_id = Some(new_tree);
                
                let t = repo.find_tree(new_tree)?;
                let msg = format!("write: {}", ps);
                let pc = repo.find_commit(parent.unwrap())?;
                let oid = repo.commit(None, &sig, &sig, &msg, &t, &[&pc])?;
                parent = Some(oid);
                commits += 1;
                repo.reference(&branch_ref, oid, true, "write")?;
                
                if args.verbose { eprintln!("committed write: {}", ps); }
            }
            
            OpKind::Edit { old, new } => {
                let rp = match resolve(&op.path, &repo_path, args.ignore_external) {
                    Some(p) => p, None => { if args.verbose { eprintln!("skip external: {}", op.path); } continue; }
                };
                let ps = rp.to_string_lossy().to_string();
                let cur = files.get(&ps).cloned().unwrap_or_default();
                let (updated, ok) = apply_edit(&cur, old, new);
                files.insert(ps.clone(), updated.clone());
                
                let blob = repo.blob(updated.as_bytes())?;
                let base = tree_id.and_then(|t| repo.find_tree(t).ok());
                let new_tree = insert_file(&repo, base.as_ref(), &rp, blob)?;
                tree_id = Some(new_tree);
                
                let t = repo.find_tree(new_tree)?;
                let msg = if ok { format!("edit: {}", ps) } else { 
                    errors = true;
                    format!("⚠️ edit (appended): {}", ps) 
                };
                let pc = repo.find_commit(parent.unwrap())?;
                let oid = repo.commit(None, &sig, &sig, &msg, &t, &[&pc])?;
                parent = Some(oid);
                commits += 1;
                repo.reference(&branch_ref, oid, true, "edit")?;
                
                if args.verbose { eprintln!("committed edit: {} (ok={})", ps, ok); }
            }
            
            OpKind::Skip(n) => {
                errors = true;
                if let Some(tid) = tree_id {
                    let t = repo.find_tree(tid)?;
                    let msg = format!("⚠️ Skipped {} malformed lines", n);
                    let pc = repo.find_commit(parent.unwrap())?;
                    let oid = repo.commit(None, &sig, &sig, &msg, &t, &[&pc])?;
                    parent = Some(oid);
                    commits += 1;
                    repo.reference(&branch_ref, oid, true, "skip")?;
                }
            }
        }
    }
    
    eprintln!("Created {} commits on {}", commits, branch);
    
    // Prepare merge
    if let Some(head_id) = orig_head {
        let branch_commit = repo.find_commit(parent.unwrap())?;
        let ann = repo.find_annotated_commit(branch_commit.id())?;
        
        // Checkout HEAD first
        repo.set_head(&format!("refs/heads/{}", repo.head()?.shorthand().unwrap_or("main")))?;
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))?;
        
        // Merge with prefer-ours (keep current files, just add recovery history)
        // The recovery branch may have corrupted state from failed edits,
        // but the history is valuable. We keep our current files.
        let mut opts = git2::MergeOptions::new();
        opts.file_favor(git2::FileFavor::Ours);
        repo.merge(&[&ann], Some(&mut opts), None)?;
        
        // Prepare merge message
        let slist = if session_ids.len() == 1 { 
            format!("session {}", session_ids[0]) 
        } else { 
            format!("sessions {}", session_ids.join(", ")) 
        };
        let suffix = if errors { " (partial recovery with errors)" } else { "" };
        let mmsg = format!("Merge recovered OpenClaw {}{}", slist, suffix);
        
        // Write MERGE_MSG
        let git_dir = repo.path();
        std::fs::write(git_dir.join("MERGE_MSG"), &mmsg)?;
        
        eprintln!("\nRepository in uncommitted merge state.");
        eprintln!("Message: {}", mmsg);
        eprintln!("To complete: git commit");
        eprintln!("To abort: git merge --abort");
    }
    
    Ok(())
}
