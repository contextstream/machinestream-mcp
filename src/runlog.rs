//! Saved runs: each check's full output and a small record of how it went.
//!
//! A run is `<id>.log` (the output, as it was printed) and `<id>.json` (the record), kept
//! under `~/.cache/machinestream/runs` with owner-only permissions. The newest 50 runs are
//! kept, and no more than 200 MB of them.

use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::credentials::set_private;

/// Runs kept at most.
pub const MAX_RUNS: usize = 50;
/// Bytes of runs kept at most.
pub const MAX_BYTES: u64 = 200 << 20;
/// How much of a run's output is held in memory for the result (the newest part).
const KEPT_BYTES: usize = 4 << 20;
/// Complete lines remembered for progress messages.
const RECENT_LINES: usize = 8;

/// The record of one run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunRecord {
    pub id: String,
    /// The tool that made it (`check`, `check_parallel`, `run`).
    pub tool: String,
    pub command: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub repo: String,
    /// RFC 3339.
    pub started_at: String,
    pub ran: bool,
    /// `remote`, `local` or `none`.
    #[serde(rename = "where")]
    pub place: String,
    pub exit_code: Option<i32>,
    pub seconds: Option<f64>,
    #[serde(default)]
    pub cached: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default)]
    pub lines: u64,
    #[serde(default)]
    pub bytes: u64,
}

impl RunRecord {
    /// `passed`, `failed (exit 1)` or `not run remotely`.
    pub fn result_word(&self) -> String {
        match (self.ran, self.exit_code) {
            (true, Some(0)) => "passed".to_string(),
            (true, Some(code)) => format!("failed (exit {code})"),
            (true, None) => "ran".to_string(),
            (false, _) => "not run remotely".to_string(),
        }
    }
}

/// A new run id: the UTC time to the second and four random hex characters.
pub fn new_id() -> String {
    let random = uuid::Uuid::new_v4().simple().to_string();
    format!(
        "{}-{}",
        chrono::Utc::now().format("%Y%m%d-%H%M%S"),
        &random[..4]
    )
}

/// Whether `id` has the shape of a run id (and so cannot name a path elsewhere).
pub fn valid_id(id: &str) -> bool {
    (8..=40).contains(&id.len())
        && id
            .chars()
            .all(|c| c.is_ascii_digit() || c.is_ascii_lowercase() || c == '-')
        && !id.starts_with('-')
}

/// The folder runs are kept in.
#[derive(Debug, Clone)]
pub struct RunStore {
    dir: PathBuf,
}

impl Default for RunStore {
    fn default() -> Self {
        Self::at(crate::config::cache_dir().join("runs"))
    }
}

impl RunStore {
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn log_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.log"))
    }

    fn record_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    fn ensure_dir(&self) -> std::io::Result<()> {
        if self.dir.is_dir() {
            return Ok(());
        }
        std::fs::create_dir_all(&self.dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }

    /// Start capturing a run's output. When the file cannot be made, the output is still
    /// kept in memory for the result.
    pub fn capture(&self, id: &str) -> Capture {
        let file = self.ensure_dir().and_then(|()| {
            let path = self.log_path(id);
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            options.open(&path).map(|file| (file, path))
        });
        let (file, path) = match file {
            Ok((file, path)) => (Some(file), Some(path)),
            Err(error) => {
                tracing::debug!(%error, "the run's output is kept in memory only");
                (None, None)
            }
        };
        Capture {
            file,
            path,
            bytes: 0,
            lines: 0,
            partial: Vec::new(),
            recent: VecDeque::new(),
            kept: Vec::new(),
            dropped: false,
        }
    }

    /// Save a run's record, then prune old runs.
    pub fn save(&self, record: &RunRecord) {
        let saved = self.ensure_dir().and_then(|()| {
            let mut temporary = tempfile::NamedTempFile::new_in(&self.dir)?;
            set_private(temporary.path())?;
            temporary.write_all(&serde_json::to_vec(record)?)?;
            temporary
                .persist(self.record_path(&record.id))
                .map_err(|error| error.error)?;
            Ok(())
        });
        if let Err(error) = saved {
            tracing::debug!(%error, "the run's record was not saved");
        }
        self.prune(MAX_RUNS, MAX_BYTES);
    }

    /// Every saved run, newest first.
    pub fn list(&self) -> Vec<RunRecord> {
        let mut runs: Vec<RunRecord> = std::fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
            .filter_map(|entry| {
                let metadata = entry.metadata().ok()?;
                if !metadata.is_file() || metadata.len() > 1 << 20 {
                    return None;
                }
                serde_json::from_slice::<RunRecord>(&std::fs::read(entry.path()).ok()?).ok()
            })
            .filter(|record| valid_id(&record.id))
            .collect();
        // Newest first, by when the run started; ids only break ties.
        runs.sort_by(|a, b| (&b.started_at, &b.id).cmp(&(&a.started_at, &a.id)));
        runs
    }

    pub fn get(&self, id: &str) -> Option<RunRecord> {
        if !valid_id(id) {
            return None;
        }
        serde_json::from_slice(&std::fs::read(self.record_path(id)).ok()?).ok()
    }

    /// The newest run that has saved output.
    pub fn latest_with_output(&self) -> Option<RunRecord> {
        self.list()
            .into_iter()
            .find(|record| self.log_path(&record.id).is_file())
    }

    /// A run's saved output, when it has some.
    pub fn read_log(&self, id: &str) -> Option<String> {
        if !valid_id(id) {
            return None;
        }
        let path = self.log_path(id);
        let metadata = std::fs::symlink_metadata(&path).ok()?;
        if !metadata.is_file() {
            return None;
        }
        Some(String::from_utf8_lossy(&std::fs::read(path).ok()?).into_owned())
    }

    /// Keep the newest `max_runs` runs and at most `max_bytes` of them.
    pub fn prune(&self, max_runs: usize, max_bytes: u64) {
        let mut total: u64 = 0;
        for (index, record) in self.list().iter().enumerate() {
            let size = [self.log_path(&record.id), self.record_path(&record.id)]
                .iter()
                .filter_map(|path| std::fs::metadata(path).ok())
                .map(|metadata| metadata.len())
                .sum::<u64>();
            total += size;
            // The newest run is always kept, whatever its size.
            if index > 0 && (index >= max_runs || total > max_bytes) {
                let _ = std::fs::remove_file(self.log_path(&record.id));
                let _ = std::fs::remove_file(self.record_path(&record.id));
            }
        }
        self.remove_orphan_logs();
    }

    /// Logs whose record is gone (a run that was interrupted, or pruned by half).
    fn remove_orphan_logs(&self) {
        let hour = std::time::Duration::from_secs(3600);
        for entry in std::fs::read_dir(&self.dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "log") {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            let old = entry
                .metadata()
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .and_then(|modified| modified.elapsed().ok())
                .is_some_and(|age| age > hour);
            if old && !self.record_path(id).exists() {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

/// A run's output as it arrives: written to its file, counted, and its newest part kept.
pub struct Capture {
    file: Option<std::fs::File>,
    path: Option<PathBuf>,
    pub bytes: u64,
    pub lines: u64,
    partial: Vec<u8>,
    recent: VecDeque<String>,
    kept: Vec<u8>,
    dropped: bool,
}

impl Capture {
    /// A capture that keeps output in memory only.
    pub fn in_memory() -> Self {
        Self {
            file: None,
            path: None,
            bytes: 0,
            lines: 0,
            partial: Vec::new(),
            recent: VecDeque::new(),
            kept: Vec::new(),
            dropped: false,
        }
    }

    pub fn push(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        if let Some(file) = &mut self.file {
            if file.write_all(data).is_err() {
                self.file = None;
            }
        }
        self.bytes += data.len() as u64;
        self.kept.extend_from_slice(data);
        if self.kept.len() > KEPT_BYTES * 2 {
            let cut = self.kept.len() - KEPT_BYTES;
            // Cut at a line start so the kept text begins with a whole line.
            let cut = self.kept[cut..]
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(cut, |offset| cut + offset + 1);
            self.kept.drain(..cut);
            self.dropped = true;
        }
        for byte in data {
            if *byte == b'\n' {
                self.lines += 1;
                let line = String::from_utf8_lossy(&self.partial)
                    .trim_end()
                    .to_string();
                self.partial.clear();
                if !line.trim().is_empty() {
                    if self.recent.len() == RECENT_LINES {
                        self.recent.pop_front();
                    }
                    self.recent.push_back(line);
                }
            } else if self.partial.len() < 4096 {
                self.partial.push(*byte);
            }
        }
    }

    /// The newest non-empty lines, oldest first.
    pub fn recent_lines(&self, count: usize) -> Vec<String> {
        let skip = self.recent.len().saturating_sub(count);
        self.recent.iter().skip(skip).cloned().collect()
    }

    /// Lines of output so far, counting an unfinished last line.
    pub fn line_count(&self) -> u64 {
        self.lines + u64::from(!self.partial.is_empty())
    }

    /// The output held in memory (all of it unless it was very long).
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.kept).into_owned()
    }

    /// Whether the oldest output was dropped from memory (it is still in the file).
    pub fn dropped_older_output(&self) -> bool {
        self.dropped
    }

    /// Close the file. The path, when the output is on disk and there was some.
    pub fn finish(&mut self) -> Option<PathBuf> {
        if let Some(file) = self.file.take() {
            let _ = file.sync_all();
        }
        match self.path.take() {
            Some(path) if self.bytes > 0 => Some(path),
            Some(path) => {
                let _ = std::fs::remove_file(path);
                None
            }
            None => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str) -> RunRecord {
        RunRecord {
            id: id.to_string(),
            tool: "check".into(),
            command: "echo hello".into(),
            cwd: String::new(),
            repo: "example".into(),
            started_at: "2026-01-01T00:00:00Z".into(),
            ran: true,
            place: "remote".into(),
            exit_code: Some(0),
            seconds: Some(1.5),
            cached: false,
            reason: None,
            lines: 1,
            bytes: 6,
        }
    }

    #[test]
    fn ids_are_safe_file_names() {
        let id = new_id();
        assert!(valid_id(&id), "{id}");
        assert!(!valid_id("../../etc/passwd"));
        assert!(!valid_id("a/b"));
        assert!(!valid_id(""));
        assert!(!valid_id("-20260101-000000-abcd"));
        assert!(!valid_id("UPPER-20260101"));
    }

    #[test]
    fn output_is_saved_privately_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let store = RunStore::at(dir.path().join("runs"));
        let id = "20260101-000000-aaaa";
        let mut capture = store.capture(id);
        capture.push(b"first line\nsecond ");
        capture.push(b"line\n\nlast without newline");
        assert_eq!(capture.lines, 3);
        assert_eq!(capture.line_count(), 4);
        assert_eq!(
            capture.recent_lines(2),
            vec!["first line".to_string(), "second line".to_string()]
        );
        let path = capture.finish().expect("a log path");
        assert_eq!(path, store.log_path(id));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        store.save(&record(id));
        assert_eq!(
            store.read_log(id).unwrap(),
            "first line\nsecond line\n\nlast without newline"
        );
        assert_eq!(store.get(id).unwrap().command, "echo hello");
        assert_eq!(store.latest_with_output().unwrap().id, id);
        assert!(store.read_log("../escape").is_none());
    }

    #[test]
    fn a_run_without_output_leaves_no_log_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = RunStore::at(dir.path());
        let id = "20260101-000000-bbbb";
        let mut capture = store.capture(id);
        assert!(capture.finish().is_none());
        assert!(!store.log_path(id).exists());
    }

    #[test]
    fn pruning_keeps_the_newest_runs() {
        let dir = tempfile::tempdir().unwrap();
        let store = RunStore::at(dir.path());
        for index in 0..6 {
            let id = format!("20260101-00000{index}-cccc");
            let mut capture = store.capture(&id);
            capture.push(b"0123456789\n");
            capture.finish();
            store.save(&record(&id));
        }
        assert_eq!(store.list().len(), 6);
        store.prune(3, MAX_BYTES);
        let kept: Vec<String> = store.list().into_iter().map(|r| r.id).collect();
        assert_eq!(
            kept,
            vec![
                "20260101-000005-cccc",
                "20260101-000004-cccc",
                "20260101-000003-cccc"
            ]
        );
        assert!(!store.log_path("20260101-000000-cccc").exists());
    }

    #[test]
    fn pruning_by_size_always_keeps_the_newest_run() {
        let dir = tempfile::tempdir().unwrap();
        let store = RunStore::at(dir.path());
        for index in 0..3 {
            let id = format!("20260101-00000{index}-dddd");
            let mut capture = store.capture(&id);
            capture.push(&vec![b'x'; 2000]);
            capture.finish();
            store.save(&record(&id));
        }
        store.prune(MAX_RUNS, 1000);
        let kept: Vec<String> = store.list().into_iter().map(|r| r.id).collect();
        assert_eq!(kept, vec!["20260101-000002-dddd"]);
    }

    #[test]
    fn very_long_output_keeps_its_newest_part_in_memory() {
        let mut capture = Capture::in_memory();
        let line = format!("{}\n", "y".repeat(1023));
        for _ in 0..(10 << 10) {
            capture.push(line.as_bytes());
        }
        capture.push(b"the end\n");
        assert!(capture.dropped_older_output());
        let text = capture.text();
        assert!(text.len() <= KEPT_BYTES * 2);
        assert!(text.ends_with("the end\n"));
        assert!(text.starts_with('y'));
        assert_eq!(capture.lines, (10 << 10) + 1);
    }

    #[test]
    fn result_words() {
        let mut run = record("20260101-000000-eeee");
        assert_eq!(run.result_word(), "passed");
        run.exit_code = Some(101);
        assert_eq!(run.result_word(), "failed (exit 101)");
        run.ran = false;
        assert_eq!(run.result_word(), "not run remotely");
    }
}
