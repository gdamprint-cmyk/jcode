use super::FileAccess;
use super::util::coordination_scope_for_dir;
use std::collections::HashSet;
use std::path::Path;

/// Compute the sessions that may participate in narrow file-activity
/// coordination for one event.
///
/// Explicit swarm membership is kept intact. Independently-created sessions
/// are added only when their working directories resolve to the same
/// server-owned coordination scope. This deliberately returns a peer set, not
/// a swarm mutation, so unrelated root sessions never inherit one another's
/// plans, TODOs, or coordinator role.
pub(crate) fn coordination_session_ids<'a, I>(
    current_session_id: &str,
    explicit_swarm_members: Option<&HashSet<String>>,
    current_working_dir: Option<&Path>,
    candidates: I,
) -> HashSet<String>
where
    I: IntoIterator<Item = (&'a str, Option<&'a Path>)>,
{
    let mut ids = explicit_swarm_members.cloned().unwrap_or_default();
    let Some(current_scope) = coordination_scope_for_dir(current_working_dir) else {
        return ids;
    };

    ids.extend(
        candidates
            .into_iter()
            .filter(|(_, working_dir)| {
                coordination_scope_for_dir(*working_dir).as_deref() == Some(current_scope.as_str())
            })
            .map(|(session_id, _)| session_id.to_string()),
    );
    ids.insert(current_session_id.to_string());
    ids
}

pub(crate) fn parse_file_activity_line_range(summary: Option<&str>) -> Option<(u64, u64)> {
    let summary = summary?;
    let marker_start = summary
        .find("lines ")
        .map(|idx| idx + "lines ".len())
        .or_else(|| summary.find("line ").map(|idx| idx + "line ".len()))?;
    let rest = &summary[marker_start..];
    let mut digits = String::new();
    let mut chars = rest.chars().peekable();
    while let Some(ch) = chars.peek().copied() {
        if ch.is_ascii_digit() {
            digits.push(ch);
            chars.next();
        } else {
            break;
        }
    }
    let start = digits.parse::<u64>().ok()?;
    if chars.peek() == Some(&'-') {
        chars.next();
        let mut end_digits = String::new();
        while let Some(ch) = chars.peek().copied() {
            if ch.is_ascii_digit() {
                end_digits.push(ch);
                chars.next();
            } else {
                break;
            }
        }
        let end = end_digits.parse::<u64>().ok().unwrap_or(start);
        Some((start.min(end), start.max(end)))
    } else {
        Some((start, start))
    }
}

pub(crate) fn file_activity_scope_label(
    previous: &FileAccess,
    current: &crate::bus::FileTouch,
) -> &'static str {
    match (
        parse_file_activity_line_range(previous.summary.as_deref()),
        parse_file_activity_line_range(current.summary.as_deref()),
    ) {
        (Some((prev_start, prev_end)), Some((current_start, current_end)))
            if prev_start <= current_end && current_start <= prev_end =>
        {
            "overlapping lines"
        }
        (Some(_), Some(_)) => "same file, non-overlapping lines",
        _ => "same file",
    }
}
