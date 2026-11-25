//! What a result looks like: one status line first, then failures, then a bounded tail of
//! the output, then the next step. Also the structured form of the same result.

use std::time::Duration;

use regex::Regex;
use serde_json::{json, Value};

use crate::check::{CheckResult, Place};

/// Lines of output shown when the caller does not say.
pub const DEFAULT_MAX_OUTPUT_LINES: usize = 200;
/// Lines shown for a passing command when the caller does not say: its last lines are the summary.
pub const PASSING_OUTPUT_LINES: usize = 30;
/// Lines shown per command in a parallel call when the caller does not say.
pub const PARALLEL_FAILED_LINES: usize = 60;
pub const PARALLEL_PASSED_LINES: usize = 5;
/// The most lines a caller can ask for.
pub const MAX_OUTPUT_LINES_LIMIT: usize = 2000;
/// A longer line is cut.
pub const MAX_LINE_CHARS: usize = 400;
/// The most output text one result carries.
pub const MAX_OUTPUT_BYTES: usize = 48 * 1024;

fn ansi_pattern() -> &'static Regex {
    static PATTERN: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b[@-Z\\-_]")
            .expect("a valid pattern")
    })
}

/// `text` without terminal colour and cursor codes. A line a progress bar redrew with
/// carriage returns keeps its last state.
pub fn strip_ansi(text: &str) -> String {
    let plain = ansi_pattern().replace_all(text, "");
    plain
        .split('\n')
        .map(|line| {
            let line = line.strip_suffix('\r').unwrap_or(line);
            line.rsplit('\r').next().unwrap_or(line)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn cut_line(line: &str) -> String {
    if line.chars().count() <= MAX_LINE_CHARS {
        return line.to_string();
    }
    let kept: String = line.chars().take(MAX_LINE_CHARS).collect();
    let more = line.chars().count() - MAX_LINE_CHARS;
    format!("{kept} [+{more} characters]")
}

/// The end of some output, bounded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tail {
    pub text: String,
    pub shown: usize,
    /// Whether lines were left out.
    pub truncated: bool,
}

/// The last `max_lines` lines of `text`, each at most [`MAX_LINE_CHARS`] long and all of
/// them at most [`MAX_OUTPUT_BYTES`].
pub fn tail(text: &str, max_lines: usize) -> Tail {
    let plain = strip_ansi(text);
    let lines: Vec<&str> = plain.trim_end_matches('\n').split('\n').collect();
    let lines: Vec<&str> = if lines == [""] { Vec::new() } else { lines };
    let total = lines.len();
    let mut kept: Vec<String> = Vec::new();
    let mut bytes = 0;
    for line in lines.iter().rev().take(max_lines) {
        let line = cut_line(line);
        if bytes + line.len() + 1 > MAX_OUTPUT_BYTES && !kept.is_empty() {
            break;
        }
        bytes += line.len() + 1;
        kept.push(line);
    }
    kept.reverse();
    Tail {
        shown: kept.len(),
        truncated: kept.len() < total,
        text: kept.join("\n"),
    }
}

/// `1 line`, `30 lines`.
pub fn lines_text(count: usize) -> String {
    if count == 1 {
        "1 line".to_string()
    } else {
        format!("{count} lines")
    }
}

pub fn seconds_text(seconds: f64) -> String {
    format!("{seconds:.1} s")
}

/// The first line of a result: what happened, the exit status, how long, and where.
pub fn status_line(result: &CheckResult) -> String {
    let timing = result
        .seconds
        .map(|seconds| {
            // The whole call, when it took notably longer than the command itself.
            match result.wall_seconds.filter(|wall| *wall - seconds >= 1.0) {
                Some(wall) => format!(
                    " in {} ({} in all)",
                    seconds_text(seconds),
                    seconds_text(wall)
                ),
                None => format!(" in {}", seconds_text(seconds)),
            }
        })
        .unwrap_or_default();
    match (result.place, result.exit_code) {
        (Place::None, _) => format!(
            "Not run remotely: {}.",
            result
                .reason
                .as_deref()
                .unwrap_or("the service did not run it")
                .trim_end_matches('.')
        ),
        (place, exit) => {
            let verdict = match exit {
                Some(0) => "Passed (exit 0)".to_string(),
                Some(code) => format!("Failed (exit {code})"),
                None => "Ran".to_string(),
            };
            let place = match (place, result.cached) {
                (Place::Local, _) => format!(
                    "ran locally (MachineStream turned it away: {})",
                    result
                        .reason
                        .as_deref()
                        .unwrap_or("no reason given")
                        .trim_end_matches('.')
                ),
                (_, true) => match result.first_ran_seconds {
                    Some(first) => format!(
                        "from cache (same files and command; the remote run took {})",
                        seconds_text(first)
                    ),
                    None => "from cache (same files and command, ran remotely before)".to_string(),
                },
                (_, false) => "ran remotely".to_string(),
            };
            format!("{verdict}{timing}, {place}.")
        }
    }
}

/// How many output lines to show for `result` when the caller asked for `asked`.
fn line_budget(result: &CheckResult, asked: Option<usize>, parallel: bool) -> usize {
    if let Some(asked) = asked {
        return asked.min(MAX_OUTPUT_LINES_LIMIT);
    }
    match (result.exit_code == Some(0), parallel) {
        (true, false) => PASSING_OUTPUT_LINES,
        (false, false) => DEFAULT_MAX_OUTPUT_LINES,
        (true, true) => PARALLEL_PASSED_LINES,
        (false, true) => PARALLEL_FAILED_LINES,
    }
}

/// A result as text, and whether output was left out of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    pub text: String,
    pub truncated: bool,
}

/// The text of one command's result.
pub fn render_check(result: &CheckResult, max_output_lines: Option<usize>) -> Rendered {
    render(result, max_output_lines, false)
}

fn render(result: &CheckResult, max_output_lines: Option<usize>, parallel: bool) -> Rendered {
    let mut lines = vec![status_line(result)];
    let total = result.output_lines as usize;
    let shown = tail(
        &result.output,
        line_budget(result, max_output_lines, parallel),
    );
    let truncated = shown.shown < total;

    if !result.ran {
        lines.push(result.local_instruction());
        if let Some(next) = &result.next_step {
            lines.push(next.clone());
        }
    }
    match &result.log_path {
        Some(path) => lines.push(format!(
            "Run {}. Full output ({}): {path}",
            result.run_id,
            lines_text(total)
        )),
        None => lines.push(format!("Run {}.", result.run_id)),
    }
    if let Some(credits) = result
        .billing
        .credits_estimated
        .filter(|credits| *credits > 0)
    {
        lines.push(format!(
            "Estimated cost: {credits} Context Credits ({} of command time).",
            seconds_text(result.billing.billable_seconds)
        ));
    }
    if !result.withheld.is_empty() {
        let named: Vec<String> = result
            .withheld
            .iter()
            .take(5)
            .map(|item| format!("{} ({})", item.path, item.reason))
            .collect();
        let more = result.withheld.len().saturating_sub(5);
        let more = if more > 0 {
            format!(" and {more} more")
        } else {
            String::new()
        };
        lines.push(format!(
            "Left on this machine, not sent: {}{more}.",
            named.join("; ")
        ));
    }
    if let Some(report) = &result.diagnostic_report {
        lines.push(String::new());
        lines.push("Failures:".to_string());
        lines.push(report.clone());
    }
    if result.ran && shown.shown > 0 {
        lines.push(String::new());
        lines.push(if truncated {
            format!("Output (last {} of {total} lines):", shown.shown)
        } else {
            "Output:".to_string()
        });
        lines.push(shown.text.clone());
    }
    if result.ran {
        let next = match (result.exit_code, result.cached, truncated) {
            (Some(0), true, _) => Some(
                "Next: nothing to fix. This is the saved result for these exact files; pass fresh: true to run it again."
                    .to_string(),
            ),
            (Some(0), false, _) => None,
            (_, _, true) => Some(format!(
                "Next: fix the failures and run the check again. For more of the output call run_output with run_id {}.",
                result.run_id
            )),
            (_, _, false) => Some("Next: fix the failures and run the check again.".to_string()),
        };
        if let Some(next) = next {
            lines.push(String::new());
            lines.push(next);
        }
    }
    Rendered {
        text: lines.join("\n"),
        truncated,
    }
}

/// The structured form of one command's result.
pub fn structured_check(result: &CheckResult, truncated: bool) -> Value {
    json!({
        "command": result.command,
        "cwd": result.cwd,
        "ran": result.ran,
        "exit_code": result.exit_code,
        "passed": result.passed(),
        "seconds": result.seconds,
        "wall_seconds": result.wall_seconds.map(|wall| (wall * 100.0).round() / 100.0),
        "cached": result.cached,
        "where": result.place.as_str(),
        "run_id": result.run_id,
        "log_path": result.log_path,
        "output_lines": result.output_lines,
        "truncated": truncated,
        "failures": result.failures,
        "tree": result.tree,
        "reason": result.reason,
        "reason_code": result.reason_code,
        "run_locally": (!result.ran).then(|| result.command.clone()),
        "withheld": result.withheld,
        "billing": result.billing,
    })
}

/// The JSON Schema of [`structured_check`].
pub fn check_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "command": {"type": "string"},
            "cwd": {"type": "string", "description": "Folder inside the repository the command was for; empty for the root."},
            "ran": {"type": "boolean", "description": "Whether the command ran (remotely, or here when the local fallback is on). False means: run it locally."},
            "exit_code": {"anyOf": [{"type": "integer"}, {"type": "null"}], "description": "The command's own exit status."},
            "passed": {"anyOf": [{"type": "boolean"}, {"type": "null"}], "description": "exit_code is 0. Null when it did not run."},
            "seconds": {"anyOf": [{"type": "number"}, {"type": "null"}], "description": "Seconds the service reports for the check."},
            "wall_seconds": {"anyOf": [{"type": "number"}, {"type": "null"}], "description": "Seconds the whole call took as seen from this machine, with the snapshot and transfer."},
            "cached": {"type": "boolean", "description": "The result was saved from an earlier run of the same command on exactly these files."},
            "where": {"type": "string", "enum": ["remote", "local", "none"]},
            "run_id": {"type": "string", "description": "Pass to run_output to read the full output."},
            "log_path": {"anyOf": [{"type": "string"}, {"type": "null"}], "description": "Local file holding the full output."},
            "output_lines": {"type": "integer"},
            "truncated": {"type": "boolean", "description": "The text result shows only part of the output."},
            "failures": {"type": "array", "items": {"type": "object"}, "description": "Failures the service extracted (file, line, col, code, message), when it has them."},
            "tree": {"anyOf": [{"type": "string"}, {"type": "null"}], "description": "The git tree id of the snapshot the command was sent with."},
            "reason": {"anyOf": [{"type": "string"}, {"type": "null"}], "description": "Why it was not run remotely."},
            "reason_code": {"anyOf": [{"type": "string"}, {"type": "null"}]},
            "run_locally": {"anyOf": [{"type": "string"}, {"type": "null"}], "description": "The command to run on this machine when ran is false."},
            "withheld": {"type": "array", "items": {"type": "object", "properties": {"path": {"type": "string"}, "reason": {"type": "string"}}}, "description": "Paths left on this machine and not sent."},
            "billing": {
                "type": "object",
                "properties": {
                    "credits_estimated": {"anyOf": [{"type": "integer"}, {"type": "null"}], "description": "Estimated Context Credits for this call; null when the price could not be read."},
                    "billable_seconds": {"type": "number"},
                    "free_reason": {"anyOf": [{"type": "string"}, {"type": "null"}], "description": "cached, not_run_remotely, ran_locally or account_not_charged."},
                    "estimate": {"type": "boolean"},
                    "credits_per_10s": {"type": "integer"}
                },
                "required": ["credits_estimated", "billable_seconds", "free_reason", "estimate"]
            }
        },
        "required": ["command", "ran", "exit_code", "passed", "seconds", "cached", "where", "run_id", "log_path", "truncated", "failures", "billing"]
    })
}

/// How a parallel call came out, in one line.
pub fn parallel_summary(results: &[CheckResult], wall: Duration) -> String {
    let passed = results.iter().filter(|r| r.passed() == Some(true)).count();
    let failed = results.iter().filter(|r| r.passed() == Some(false)).count();
    let not_run = results.iter().filter(|r| !r.ran).count();
    let mut parts = Vec::new();
    if passed > 0 || (failed == 0 && not_run == 0) {
        parts.push(format!("{passed} passed"));
    }
    if failed > 0 {
        parts.push(format!("{failed} failed"));
    }
    if not_run > 0 {
        parts.push(format!("{not_run} not run remotely"));
    }
    format!(
        "{} ({} wall, one snapshot of the checkout).",
        parts.join(", "),
        seconds_text(wall.as_secs_f64())
    )
}

/// The text of a parallel call: the summary, then each command, failures first.
pub fn render_parallel(
    results: &[CheckResult],
    wall: Duration,
    max_output_lines: Option<usize>,
) -> (String, Vec<bool>) {
    let mut truncated = vec![false; results.len()];
    let mut order: Vec<usize> = (0..results.len()).collect();
    // Failed first, then not run, then passed; the caller's order within each.
    order.sort_by_key(|index| match results[*index].passed() {
        Some(false) => 0,
        None => 1,
        Some(true) => 2,
    });
    let mut blocks = vec![parallel_summary(results, wall)];
    for index in order {
        let result = &results[index];
        let rendered = render(result, max_output_lines, true);
        truncated[index] = rendered.truncated;
        blocks.push(format!(
            "[{}] {}\n{}",
            index + 1,
            result.command,
            rendered.text
        ));
    }
    (blocks.join("\n\n"), truncated)
}

/// The structured form of a parallel call.
pub fn structured_parallel(results: &[CheckResult], truncated: &[bool], wall: Duration) -> Value {
    let credits: Option<u64> = results
        .iter()
        .map(|result| result.billing.credits_estimated)
        .sum();
    json!({
        "summary": {
            "passed": results.iter().filter(|r| r.passed() == Some(true)).count(),
            "failed": results.iter().filter(|r| r.passed() == Some(false)).count(),
            "not_run": results.iter().filter(|r| !r.ran).count(),
            "wall_seconds": (wall.as_secs_f64() * 10.0).round() / 10.0,
            "credits_estimated": credits,
        },
        "checks": results
            .iter()
            .zip(truncated)
            .map(|(result, truncated)| structured_check(result, *truncated))
            .collect::<Vec<_>>(),
    })
}

pub fn parallel_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "summary": {
                "type": "object",
                "properties": {
                    "passed": {"type": "integer"},
                    "failed": {"type": "integer"},
                    "not_run": {"type": "integer"},
                    "wall_seconds": {"type": "number"},
                    "credits_estimated": {"anyOf": [{"type": "integer"}, {"type": "null"}]}
                },
                "required": ["passed", "failed", "not_run", "wall_seconds"]
            },
            "checks": {"type": "array", "items": check_output_schema(), "description": "One result per command, in the order given."}
        },
        "required": ["summary", "checks"]
    })
}

/// Which lines of a saved run to show.
#[derive(Debug, Default)]
pub struct OutputQuery {
    pub grep: Option<Regex>,
    /// Lines of context around each match.
    pub context: usize,
    pub head: Option<usize>,
    pub tail: Option<usize>,
    pub max_lines: usize,
}

/// The lines a query selects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    /// Numbered lines; a gap between two groups of lines is a `None`.
    pub lines: Vec<Option<(usize, String)>>,
    pub total_lines: usize,
    /// Lines that matched `grep`, before any limit.
    pub matches: usize,
    /// Whether selected lines were left out to stay within `max_lines`.
    pub truncated: bool,
}

/// Select lines of `text` (a run's output). Without `grep`, `head` and `tail` pick the ends;
/// with it they pick the first or last matches. The newest lines win when over `max_lines`,
/// unless `head` was asked for.
pub fn select_lines(text: &str, query: &OutputQuery) -> Selection {
    let plain = strip_ansi(text);
    let all: Vec<&str> = plain.trim_end_matches('\n').split('\n').collect();
    let all: Vec<&str> = if all == [""] { Vec::new() } else { all };
    let total_lines = all.len();
    let max_lines = query.max_lines.clamp(1, MAX_OUTPUT_LINES_LIMIT);

    let mut matches = 0;
    let mut wanted: Vec<usize> = match &query.grep {
        Some(pattern) => {
            let mut hits: Vec<usize> = (0..total_lines)
                .filter(|index| pattern.is_match(all[*index]))
                .collect();
            matches = hits.len();
            if let Some(head) = query.head {
                hits.truncate(head);
            } else if let Some(tail) = query.tail {
                hits.drain(..hits.len().saturating_sub(tail));
            }
            let mut wanted = Vec::new();
            for hit in hits {
                let from = hit.saturating_sub(query.context);
                let to = (hit + query.context).min(total_lines.saturating_sub(1));
                for index in from..=to {
                    if wanted.last().is_none_or(|last| *last < index) {
                        wanted.push(index);
                    }
                }
            }
            wanted
        }
        None => match (query.head, query.tail) {
            (Some(head), _) => (0..total_lines.min(head)).collect(),
            (None, Some(tail)) => (total_lines.saturating_sub(tail)..total_lines).collect(),
            (None, None) => (0..total_lines).collect(),
        },
    };
    let truncated = wanted.len() > max_lines;
    if truncated {
        if query.head.is_some() {
            wanted.truncate(max_lines);
        } else {
            wanted.drain(..wanted.len() - max_lines);
        }
    }
    let mut lines = Vec::with_capacity(wanted.len());
    let mut bytes = 0;
    let mut previous: Option<usize> = None;
    let mut cut_for_size = false;
    for index in wanted {
        let line = cut_line(all[index]);
        if bytes + line.len() > MAX_OUTPUT_BYTES * 2 {
            cut_for_size = true;
            break;
        }
        bytes += line.len() + 1;
        if previous.is_some_and(|previous| previous + 1 != index) {
            lines.push(None);
        }
        lines.push(Some((index + 1, line)));
        previous = Some(index);
    }
    Selection {
        lines,
        total_lines,
        matches,
        truncated: truncated || cut_for_size,
    }
}

/// A selection as text: each line with its number, groups apart by `--`.
pub fn render_selection(selection: &Selection) -> String {
    let width = selection.total_lines.to_string().len();
    selection
        .lines
        .iter()
        .map(|line| match line {
            Some((number, text)) => format!("{number:>width$}: {text}"),
            None => "--".to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// How long ago, in words: `12 s ago`, `5 min ago`, `3 h ago`, `2 d ago`.
pub fn age_text(started_at: &str, now: chrono::DateTime<chrono::Utc>) -> String {
    let Ok(started) = chrono::DateTime::parse_from_rfc3339(started_at) else {
        return "unknown".to_string();
    };
    let seconds = (now - started.with_timezone(&chrono::Utc))
        .num_seconds()
        .max(0);
    match seconds {
        0..=89 => format!("{seconds} s ago"),
        90..=5399 => format!("{} min ago", (seconds + 30) / 60),
        5400..=129_599 => format!("{} h ago", (seconds + 1800) / 3600),
        _ => format!("{} d ago", (seconds + 43_200) / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::billing::{self, Billing, Price, Usage};
    use crate::check::WithheldPath;

    fn ran(exit: i32, output: &str) -> CheckResult {
        CheckResult {
            command: "cargo check".into(),
            cwd: String::new(),
            ran: true,
            place: Place::Remote,
            exit_code: Some(exit),
            seconds: Some(4.21),
            cached: false,
            first_ran_seconds: None,
            wall_seconds: None,
            tree: None,
            run_id: "20260101-000000-abcd".into(),
            log_path: Some("/tmp/runs/20260101-000000-abcd.log".into()),
            reason: None,
            reason_code: None,
            next_step: None,
            failures: Vec::new(),
            diagnostic_report: None,
            withheld: Vec::new(),
            billing: billing::estimate(
                Usage::Remote { seconds: 4.21 },
                Some(Price {
                    credits_per_10s: Some(20),
                    no_charge: false,
                }),
            ),
            output: output.to_string(),
            output_lines: output.lines().count() as u64,
            output_bytes: output.len() as u64,
        }
    }

    fn turned_away(reason: &str) -> CheckResult {
        CheckResult {
            ran: false,
            place: Place::None,
            exit_code: None,
            seconds: None,
            log_path: None,
            reason: Some(reason.to_string()),
            reason_code: Some("declined"),
            billing: Billing::free("not_run_remotely"),
            ..ran(0, "")
        }
    }

    #[test]
    fn colour_codes_and_redrawn_lines_are_cleaned() {
        assert_eq!(
            strip_ansi("\x1b[1m\x1b[32m   Compiling\x1b[0m demo v0.1.0"),
            "   Compiling demo v0.1.0"
        );
        assert_eq!(
            strip_ansi("10%\r50%\r100% done\nnext\r\n"),
            "100% done\nnext\n"
        );
        assert_eq!(strip_ansi("\x1b]0;title\x07plain"), "plain");
        assert_eq!(strip_ansi("no codes"), "no codes");
    }

    #[test]
    fn a_tail_is_bounded_by_lines() {
        let text: String = (1..=500).map(|n| format!("line {n}\n")).collect();
        let shown = tail(&text, 3);
        assert_eq!(shown.text, "line 498\nline 499\nline 500");
        assert_eq!(shown.shown, 3);
        assert!(shown.truncated);
        let all = tail("a\nb\n", 10);
        assert_eq!(all.text, "a\nb");
        assert!(!all.truncated);
        assert_eq!(tail("", 10).shown, 0);
    }

    #[test]
    fn a_tail_is_bounded_by_line_length_and_total_size() {
        let long = "x".repeat(MAX_LINE_CHARS + 25);
        let shown = tail(&long, 5);
        assert!(shown.text.ends_with("[+25 characters]"));
        let many: String = (0..2000)
            .map(|_| format!("{}\n", "y".repeat(300)))
            .collect();
        let bounded = tail(&many, 2000);
        assert!(bounded.text.len() <= MAX_OUTPUT_BYTES);
        assert!(bounded.truncated);
        assert!(bounded.shown < 2000);
    }

    #[test]
    fn status_lines_lead_with_the_answer() {
        assert_eq!(
            status_line(&ran(0, "")),
            "Passed (exit 0) in 4.2 s, ran remotely."
        );
        assert_eq!(
            status_line(&ran(101, "")),
            "Failed (exit 101) in 4.2 s, ran remotely."
        );
        let mut slow_call = ran(0, "");
        slow_call.wall_seconds = Some(6.84);
        assert_eq!(
            status_line(&slow_call),
            "Passed (exit 0) in 4.2 s (6.8 s in all), ran remotely."
        );
        slow_call.wall_seconds = Some(4.9);
        assert_eq!(
            status_line(&slow_call),
            "Passed (exit 0) in 4.2 s, ran remotely."
        );
        let mut cached = ran(0, "");
        cached.cached = true;
        cached.seconds = Some(0.31);
        cached.first_ran_seconds = Some(12.04);
        assert_eq!(
            status_line(&cached),
            "Passed (exit 0) in 0.3 s, from cache (same files and command; the remote run took 12.0 s)."
        );
        assert_eq!(
            status_line(&turned_away("the fixture declines this command.")),
            "Not run remotely: the fixture declines this command."
        );
        let mut local = ran(1, "");
        local.place = Place::Local;
        local.reason = Some("the service could not be reached".into());
        assert_eq!(
            status_line(&local),
            "Failed (exit 1) in 4.2 s, ran locally (MachineStream turned it away: the service could not be reached)."
        );
    }

    #[test]
    fn a_passing_result_is_short() {
        let output: String = (1..=120).map(|n| format!("ok {n}\n")).collect();
        let rendered = render_check(&ran(0, &output), None);
        let lines: Vec<&str> = rendered.text.lines().collect();
        assert_eq!(lines[0], "Passed (exit 0) in 4.2 s, ran remotely.");
        assert!(lines[1].starts_with("Run 20260101-000000-abcd. Full output (120 lines): "));
        assert_eq!(
            lines[2],
            "Estimated cost: 9 Context Credits (4.2 s of command time)."
        );
        assert!(rendered.text.contains("Output (last 30 of 120 lines):"));
        assert!(rendered.text.ends_with("ok 120"));
        assert!(rendered.truncated);
        assert!(!rendered.text.contains("ok 90\n"));
    }

    #[test]
    fn a_failing_result_shows_failures_then_output_then_the_next_step() {
        let mut failed = ran(101, "error[E0308]: mismatched types\n --> src/lib.rs:3:5\n");
        failed.diagnostic_report =
            Some("[diagnostics: 1 failure(s)]\n- src/lib.rs:3:5 [E0308] mismatched types".into());
        let rendered = render_check(&failed, None);
        let text = &rendered.text;
        let failures = text.find("Failures:").unwrap();
        let output = text.find("Output:").unwrap();
        let next = text.find("Next: fix the failures").unwrap();
        assert!(text.starts_with("Failed (exit 101) in 4.2 s, ran remotely."));
        assert!(failures < output && output < next);
        assert!(!rendered.truncated);
    }

    #[test]
    fn the_callers_line_limit_is_honoured_for_passing_runs_too() {
        let output: String = (1..=120).map(|n| format!("ok {n}\n")).collect();
        let rendered = render_check(&ran(0, &output), Some(100));
        assert!(rendered.text.contains("Output (last 100 of 120 lines):"));
        let none = render_check(&ran(0, &output), Some(0));
        assert!(!none.text.contains("Output"));
        assert!(none.truncated);
    }

    #[test]
    fn a_turned_away_result_says_what_to_run_locally() {
        let mut away = turned_away("remote checks are off for this account");
        away.next_step = Some("Call setup to see the price and turn them on.".into());
        away.cwd = "web".into();
        let rendered = render_check(&away, None);
        let lines: Vec<&str> = rendered.text.lines().collect();
        assert_eq!(
            lines[0],
            "Not run remotely: remote checks are off for this account."
        );
        assert_eq!(
            lines[1],
            "Run this command locally: cargo check (in the folder web)"
        );
        assert_eq!(lines[2], "Call setup to see the price and turn them on.");
        assert_eq!(lines[3], "Run 20260101-000000-abcd.");
        assert!(!rendered.text.contains("Estimated cost"));
        assert!(!rendered.text.contains("Next:"));
    }

    #[test]
    fn withheld_paths_are_named() {
        let mut result = ran(0, "ok\n");
        result.withheld = (0..7)
            .map(|n| WithheldPath {
                path: format!("secrets/{n}.env"),
                reason: "looks like a credential".into(),
            })
            .collect();
        let text = render_check(&result, None).text;
        assert!(text
            .contains("Left on this machine, not sent: secrets/0.env (looks like a credential); "));
        assert!(text.contains(" and 2 more."));
    }

    #[test]
    fn the_structured_form_has_the_documented_fields() {
        let mut failed = ran(101, "boom\n");
        failed.failures = vec![json!({"file": "src/lib.rs", "line": 3})];
        let value = structured_check(&failed, true);
        assert_eq!(value["ran"], true);
        assert_eq!(value["exit_code"], 101);
        assert_eq!(value["passed"], false);
        assert_eq!(value["cached"], false);
        assert_eq!(value["where"], "remote");
        assert_eq!(value["run_id"], "20260101-000000-abcd");
        assert_eq!(value["truncated"], true);
        assert_eq!(value["failures"][0]["file"], "src/lib.rs");
        assert_eq!(value["billing"]["credits_estimated"], 9);
        assert_eq!(value["billing"]["billable_seconds"], 4.21);
        assert_eq!(value["billing"]["free_reason"], Value::Null);
        assert_eq!(value["run_locally"], Value::Null);
        let schema = check_output_schema();
        for field in schema["required"].as_array().unwrap() {
            assert!(
                value.get(field.as_str().unwrap()).is_some(),
                "missing {field}"
            );
        }

        let away = structured_check(&turned_away("out of credits"), false);
        assert_eq!(away["ran"], false);
        assert_eq!(away["passed"], Value::Null);
        assert_eq!(away["where"], "none");
        assert_eq!(away["run_locally"], "cargo check");
        assert_eq!(away["billing"]["free_reason"], "not_run_remotely");
        assert_eq!(away["billing"]["credits_estimated"], 0);
    }

    #[test]
    fn a_parallel_result_leads_with_the_summary_and_puts_failures_first() {
        let mut passed = ran(0, "fine\n");
        passed.command = "cargo fmt --check".into();
        let mut failed = ran(1, "bad\n");
        failed.command = "cargo clippy".into();
        let mut away = turned_away("the service could not be reached");
        away.command = "cargo test".into();
        let results = vec![passed, failed, away];
        let wall = Duration::from_millis(12_440);
        assert_eq!(
            parallel_summary(&results, wall),
            "1 passed, 1 failed, 1 not run remotely (12.4 s wall, one snapshot of the checkout)."
        );
        let (text, truncated) = render_parallel(&results, wall, None);
        assert!(text.starts_with("1 passed, 1 failed, 1 not run remotely"));
        let clippy = text.find("[2] cargo clippy").unwrap();
        let test = text.find("[3] cargo test").unwrap();
        let fmt = text.find("[1] cargo fmt --check").unwrap();
        assert!(clippy < test && test < fmt);
        assert_eq!(truncated, vec![false, false, false]);
        let value = structured_parallel(&results, &truncated, wall);
        assert_eq!(value["summary"]["passed"], 1);
        assert_eq!(value["summary"]["failed"], 1);
        assert_eq!(value["summary"]["not_run"], 1);
        assert_eq!(value["summary"]["wall_seconds"], 12.4);
        assert_eq!(value["summary"]["credits_estimated"], 18);
        assert_eq!(value["checks"][0]["command"], "cargo fmt --check");
        assert_eq!(value["checks"][2]["run_locally"], "cargo test");
    }

    #[test]
    fn an_all_passing_parallel_summary_is_plain() {
        let results = vec![ran(0, ""), ran(0, "")];
        assert_eq!(
            parallel_summary(&results, Duration::from_secs(3)),
            "2 passed (3.0 s wall, one snapshot of the checkout)."
        );
    }

    fn numbered(count: usize) -> String {
        (1..=count).map(|n| format!("line {n}\n")).collect()
    }

    #[test]
    fn head_and_tail_pick_the_ends() {
        let text = numbered(50);
        let head = select_lines(
            &text,
            &OutputQuery {
                head: Some(2),
                max_lines: 100,
                ..Default::default()
            },
        );
        assert_eq!(render_selection(&head), " 1: line 1\n 2: line 2");
        assert_eq!(head.total_lines, 50);
        assert!(!head.truncated);
        let tail = select_lines(
            &text,
            &OutputQuery {
                tail: Some(2),
                max_lines: 100,
                ..Default::default()
            },
        );
        assert_eq!(render_selection(&tail), "49: line 49\n50: line 50");
    }

    #[test]
    fn the_newest_lines_win_when_over_the_limit() {
        let text = numbered(50);
        let selection = select_lines(
            &text,
            &OutputQuery {
                max_lines: 3,
                ..Default::default()
            },
        );
        assert!(selection.truncated);
        assert_eq!(
            render_selection(&selection),
            "48: line 48\n49: line 49\n50: line 50"
        );
    }

    #[test]
    fn grep_shows_matches_with_context_and_gaps() {
        let text = numbered(40);
        let selection = select_lines(
            &text,
            &OutputQuery {
                grep: Some(Regex::new("line (5|6|30)$").unwrap()),
                context: 1,
                max_lines: 100,
                ..Default::default()
            },
        );
        assert_eq!(selection.matches, 3);
        assert_eq!(
            render_selection(&selection),
            " 4: line 4\n 5: line 5\n 6: line 6\n 7: line 7\n--\n29: line 29\n30: line 30\n31: line 31"
        );
        let last = select_lines(
            &text,
            &OutputQuery {
                grep: Some(Regex::new("line 1").unwrap()),
                tail: Some(1),
                max_lines: 100,
                ..Default::default()
            },
        );
        assert_eq!(last.matches, 11);
        assert_eq!(render_selection(&last), "19: line 19");
    }

    #[test]
    fn an_empty_run_selects_nothing() {
        let selection = select_lines(
            "",
            &OutputQuery {
                max_lines: 10,
                ..Default::default()
            },
        );
        assert_eq!(selection.total_lines, 0);
        assert!(selection.lines.is_empty());
        assert_eq!(render_selection(&selection), "");
    }

    #[test]
    fn ages_read_naturally() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-01-02T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(age_text("2026-01-01T23:59:48Z", now), "12 s ago");
        assert_eq!(age_text("2026-01-01T23:55:00Z", now), "5 min ago");
        assert_eq!(age_text("2026-01-01T21:00:00Z", now), "3 h ago");
        assert_eq!(age_text("2025-12-31T00:00:00Z", now), "2 d ago");
        assert_eq!(age_text("not a time", now), "unknown");
    }
}
