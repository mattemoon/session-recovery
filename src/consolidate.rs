//! Commit consolidation logic for session-recovery
//!
//! Consolidates consecutive operations into single commits when:
//! 1. Time gap between operations is < 2048 seconds (64*32)
//! 2. No added lines are also removed in the batch
//! 3. All operations are from the same session
//! 4. No unsupported tool calls in between

use std::collections::{HashMap, HashSet};

/// A batch of operations to be consolidated into a single commit
#[derive(Debug)]
pub struct ConsolidatedBatch {
    /// Operations in this batch (as indices into the original ops vec)
    pub op_indices: Vec<usize>,
    /// Session ID (all ops in batch must share this)
    pub session: String,
    /// Whether this batch was actually consolidated (vs single op or forced split)
    pub consolidated: bool,
}

/// Check if two consecutive operations can be consolidated
pub fn can_consolidate(
    prev_ts: i64,
    curr_ts: i64,
    prev_session: &str,
    curr_session: &str,
    max_gap_seconds: i64,
) -> bool {
    // Must be same session
    if prev_session != curr_session {
        return false;
    }
    // Time gap must be within threshold
    let gap = curr_ts - prev_ts;
    gap >= 0 && gap <= max_gap_seconds
}

/// Check if a batch of edits has conflicting line changes
/// (any line added that is also removed)
pub fn has_line_conflicts(operations: &[(String, String)]) -> bool {
    // operations: Vec<(old_text, new_text)> for edits
    let mut added_lines: HashSet<String> = HashSet::new();
    let mut removed_lines: HashSet<String> = HashSet::new();
    
    for (old, new) in operations {
        // Lines in old but not in new are removed
        for line in old.lines() {
            let line = line.trim().to_string();
            if !line.is_empty() && !new.contains(&line) {
                removed_lines.insert(line);
            }
        }
        // Lines in new but not in old are added
        for line in new.lines() {
            let line = line.trim().to_string();
            if !line.is_empty() && !old.contains(&line) {
                added_lines.insert(line);
            }
        }
    }
    
    // Conflict if any added line was also removed
    !added_lines.is_disjoint(&removed_lines)
}

/// Format a consolidated commit message
pub fn format_consolidated_message(
    operations: &[(&str, &str)], // (kind, path)
    sessions: &HashSet<&str>,
    session_formats: &HashMap<String, String>,
) -> String {
    let mut msg = String::new();
    
    // First paragraph: one line per operation
    for (kind, path) in operations {
        msg.push_str(&format!("{}: {}\n", kind, path));
    }
    
    // Blank line
    msg.push('\n');
    
    // Second paragraph: session IDs
    for session in sessions {
        let format_name = session_formats.get(*session).map(|s| s.as_str()).unwrap_or("Session");
        msg.push_str(&format!("{} session {}\n", format_name, session));
    }
    
    msg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_can_consolidate_same_session() {
        assert!(can_consolidate(1000, 1500, "abc", "abc", 2048));
        assert!(can_consolidate(1000, 3000, "abc", "abc", 2048)); // 2000 < 2048
        assert!(!can_consolidate(1000, 4000, "abc", "abc", 2048)); // 3000 > 2048
    }

    #[test]
    fn test_can_consolidate_different_session() {
        assert!(!can_consolidate(1000, 1500, "abc", "def", 2048));
    }

    #[test]
    fn test_line_conflicts_no_conflict() {
        let ops = vec![
            ("line1\nline2".to_string(), "line1\nline3".to_string()),
            ("line3\nline4".to_string(), "line3\nline5".to_string()),
        ];
        assert!(!has_line_conflicts(&ops));
    }

    #[test]
    fn test_line_conflicts_has_conflict() {
        let ops = vec![
            ("line1".to_string(), "line2".to_string()), // removes line1, adds line2
            ("line3".to_string(), "line1".to_string()), // removes line3, adds line1 (conflict!)
        ];
        assert!(has_line_conflicts(&ops));
    }
}
