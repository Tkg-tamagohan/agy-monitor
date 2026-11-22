use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::analyzer::{ConversationAnalysis, analyze_conversation};
use crate::fingerprint::{DbFingerprint, FileStamp, TranscriptFingerprint};
use crate::paths::GeminiPaths;
use crate::sessions::{SessionIndex, load_session_index};

#[derive(Debug, Clone)]
pub enum MonitorCommand {
    SelectConversation(Option<String>),
    SetFollowLatest(bool),
    SetPaused(bool),
    RefreshNow,
    Shutdown,
}

#[derive(Debug, Clone)]
pub struct MonitorSnapshot {
    pub index: SessionIndex,
    pub selected_id: Option<String>,
    pub analysis: Option<ConversationAnalysis>,
    pub follow_latest: bool,
    pub paused: bool,
    pub last_updated_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub db_busy: bool,
    pub analysis_count: u64,
}

impl Default for MonitorSnapshot {
    fn default() -> Self {
        Self {
            index: SessionIndex::default(),
            selected_id: None,
            analysis: None,
            follow_latest: true,
            paused: false,
            last_updated_at: None,
            last_error: None,
            db_busy: false,
            analysis_count: 0,
        }
    }
}

pub struct MonitorConfig {
    pub paths: GeminiPaths,
    pub poll_interval: Duration,
    pub follow_latest: bool,
    pub notify: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl MonitorConfig {
    pub fn new(paths: GeminiPaths) -> Self {
        Self {
            paths,
            poll_interval: Duration::from_secs(2),
            follow_latest: true,
            notify: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AnalysisCacheKey {
    conv_id: String,
    transcript_fp: TranscriptFingerprint,
    db_fp: DbFingerprint,
    pbtxt_fp: FileStamp,
}

pub struct MonitorHandle {
    tx: Sender<MonitorCommand>,
    snapshot: Arc<Mutex<MonitorSnapshot>>,
    thread: Option<JoinHandle<()>>,
}

impl MonitorHandle {
    pub fn start(config: MonitorConfig) -> Self {
        let initial = MonitorSnapshot {
            follow_latest: config.follow_latest,
            ..Default::default()
        };
        let snapshot = Arc::new(Mutex::new(initial));
        let snap_clone = Arc::clone(&snapshot);
        let (tx, rx) = mpsc::channel::<MonitorCommand>();

        let thread = thread::Builder::new()
            .name("agy-monitor-poller".to_string())
            .spawn(move || {
                run_poller(config, rx, snap_clone);
            })
            .expect("failed to spawn monitor polling thread");

        Self {
            tx,
            snapshot,
            thread: Some(thread),
        }
    }

    pub fn send(&self, cmd: MonitorCommand) {
        let _ = self.tx.send(cmd);
    }

    pub fn snapshot(&self) -> MonitorSnapshot {
        self.snapshot.lock().map(|g| g.clone()).unwrap_or_default()
    }
}

impl Drop for MonitorHandle {
    fn drop(&mut self) {
        let _ = self.tx.send(MonitorCommand::Shutdown);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

fn run_poller(
    config: MonitorConfig,
    rx: mpsc::Receiver<MonitorCommand>,
    snapshot: Arc<Mutex<MonitorSnapshot>>,
) {
    let mut follow_latest = config.follow_latest;
    let mut paused = false;
    let mut selected_id: Option<String> = None;
    let mut cached_key: Option<AnalysisCacheKey> = None;
    let mut cached_analysis: Option<ConversationAnalysis> = None;
    let mut analysis_count: u64 = 0;

    // Perform initial poll immediately on startup.
    perform_poll(
        &config,
        &mut follow_latest,
        &mut selected_id,
        &mut cached_key,
        &mut cached_analysis,
        &mut analysis_count,
        paused,
        false,
        &snapshot,
    );

    loop {
        match rx.recv_timeout(config.poll_interval) {
            Ok(MonitorCommand::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
            Ok(cmd) => {
                let mut force_reanalyze = false;
                let mut should_poll = false;
                if matches!(
                    apply_command(
                        cmd,
                        &mut follow_latest,
                        &mut paused,
                        &mut selected_id,
                        &mut should_poll,
                        &mut force_reanalyze,
                        &snapshot,
                        &config.notify,
                    ),
                    CommandOutcome::Exit
                ) {
                    break;
                }

                // Drain any queued commands before polling.
                while let Ok(next_cmd) = rx.try_recv() {
                    if matches!(
                        apply_command(
                            next_cmd,
                            &mut follow_latest,
                            &mut paused,
                            &mut selected_id,
                            &mut should_poll,
                            &mut force_reanalyze,
                            &snapshot,
                            &config.notify,
                        ),
                        CommandOutcome::Exit
                    ) {
                        return;
                    }
                }

                if should_poll {
                    perform_poll(
                        &config,
                        &mut follow_latest,
                        &mut selected_id,
                        &mut cached_key,
                        &mut cached_analysis,
                        &mut analysis_count,
                        paused,
                        force_reanalyze,
                        &snapshot,
                    );
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if !paused {
                    perform_poll(
                        &config,
                        &mut follow_latest,
                        &mut selected_id,
                        &mut cached_key,
                        &mut cached_analysis,
                        &mut analysis_count,
                        paused,
                        false,
                        &snapshot,
                    );
                }
            }
        }
    }
}

enum CommandOutcome {
    Continue,
    Exit,
}

#[allow(clippy::too_many_arguments)]
fn apply_command(
    cmd: MonitorCommand,
    follow_latest: &mut bool,
    paused: &mut bool,
    selected_id: &mut Option<String>,
    should_poll: &mut bool,
    force_reanalyze: &mut bool,
    snapshot: &Arc<Mutex<MonitorSnapshot>>,
    notify: &Option<Arc<dyn Fn() + Send + Sync>>,
) -> CommandOutcome {
    match cmd {
        MonitorCommand::SelectConversation(id) => {
            if id.is_some() && *follow_latest {
                *follow_latest = false;
                *should_poll = true;
            }
            if *selected_id != id {
                *selected_id = id;
                *should_poll = true;
            }
        }
        MonitorCommand::SetFollowLatest(flag) => {
            *follow_latest = flag;
            *should_poll = true;
        }
        MonitorCommand::SetPaused(flag) => {
            *paused = flag;
            update_paused_state(snapshot, *paused, *follow_latest, notify);
            if !*paused {
                *should_poll = true;
            }
        }
        MonitorCommand::RefreshNow => {
            *should_poll = true;
            *force_reanalyze = true;
        }
        MonitorCommand::Shutdown => return CommandOutcome::Exit,
    }
    CommandOutcome::Continue
}

fn update_paused_state(
    snapshot: &Arc<Mutex<MonitorSnapshot>>,
    paused: bool,
    follow_latest: bool,
    notify: &Option<Arc<dyn Fn() + Send + Sync>>,
) {
    if let Ok(mut guard) = snapshot.lock() {
        guard.paused = paused;
        guard.follow_latest = follow_latest;
    }
    if let Some(cb) = notify {
        cb();
    }
}

#[allow(clippy::too_many_arguments)]
fn perform_poll(
    config: &MonitorConfig,
    follow_latest: &mut bool,
    selected_id: &mut Option<String>,
    cached_key: &mut Option<AnalysisCacheKey>,
    cached_analysis: &mut Option<ConversationAnalysis>,
    analysis_count: &mut u64,
    paused: bool,
    force_reanalyze: bool,
    snapshot: &Arc<Mutex<MonitorSnapshot>>,
) {
    let index = load_session_index(&config.paths);

    if *follow_latest {
        if let Some(latest) = index.latest_root_id() {
            *selected_id = Some(latest.to_string());
        }
    } else if selected_id.is_none()
        && let Some(latest) = index.latest_root_id()
    {
        *selected_id = Some(latest.to_string());
    }

    let mut last_error: Option<String> = None;
    let mut analysis_db_busy = false;

    if let Some(conv_id) = selected_id.clone() {
        if let Some(summary) = index.by_id.get(&conv_id) {
            let app_data = &summary.app_data_dir;
            let logs_dir = app_data
                .join("brain")
                .join(&conv_id)
                .join(".system_generated")
                .join("logs");
            let conv_db = app_data.join("conversations").join(format!("{conv_id}.db"));
            let pbtxt_path = app_data.join("antigravity_state.pbtxt");

            let key = AnalysisCacheKey {
                conv_id: conv_id.clone(),
                transcript_fp: TranscriptFingerprint::from_logs_dir(&logs_dir),
                db_fp: DbFingerprint::from_db_path(&conv_db),
                pbtxt_fp: FileStamp::from_path(&pbtxt_path),
            };

            let cache_hit = !force_reanalyze && cached_key.as_ref() == Some(&key);
            if !cache_hit {
                match analyze_conversation(app_data, &conv_id, None, None) {
                    Ok(res) => {
                        analysis_db_busy = res.db_busy;
                        // Do not freeze the cache key if the DB was busy so the next poll
                        // retries reading model/limit metadata once the lock clears.
                        *cached_key = if res.db_busy { None } else { Some(key) };
                        *cached_analysis = Some(res);
                        *analysis_count += 1;
                    }
                    Err(e) => {
                        last_error = Some(e.to_string());
                        *cached_key = None;
                        *cached_analysis = None;
                    }
                }
            } else if let Some(a) = cached_analysis.as_ref() {
                analysis_db_busy = a.db_busy;
            }
        } else {
            *cached_key = None;
            *cached_analysis = None;
        }
    } else {
        *cached_key = None;
        *cached_analysis = None;
    }

    let db_busy = index.db_busy || analysis_db_busy;
    let now = Utc::now();

    if let Ok(mut guard) = snapshot.lock() {
        guard.index = index;
        guard.selected_id = selected_id.clone();
        guard.analysis = cached_analysis.clone();
        guard.follow_latest = *follow_latest;
        guard.paused = paused;
        guard.last_updated_at = Some(now);
        guard.last_error = last_error;
        guard.db_busy = db_busy;
        guard.analysis_count = *analysis_count;
    }

    if let Some(cb) = &config.notify {
        cb();
    }
}
