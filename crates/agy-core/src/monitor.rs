use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use crate::analyzer::{
    ConversationAnalysis, CustomizationBreakdown, analyze_conversation_with_meta_and_customizations,
};
use crate::customizations::{CustomizationInventory, scan_customizations_cancellable};
use crate::fingerprint::{DbFingerprint, FileStamp, TranscriptFingerprint};
use crate::models::detect_conversation_model_label_from_db;
use crate::paths::GeminiPaths;
use crate::sessions::{SessionIndex, load_session_index};
use crate::usage::{
    DEFAULT_MAX_USAGE_SESSIONS, UsageCache, UsagePeriod, UsageReport, aggregate_usage,
};

const MAX_MODEL_SCANS_PER_TICK: usize = 40;
const MODEL_BACKFILL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone)]
pub enum MonitorCommand {
    SelectConversation(Option<String>),
    SetFollowLatest(bool),
    SetPaused(bool),
    RefreshNow,
    RefreshUsage(UsagePeriod),
    RefreshCustomizations,
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
    pub usage_report: Option<UsageReport>,
    pub usage_loading: bool,
    pub customizations: Option<CustomizationInventory>,
    pub customizations_loading: bool,
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
            usage_report: None,
            usage_loading: false,
            customizations: None,
            customizations_loading: false,
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
    breakdown_version: u64,
}

enum PollerMsg {
    Public(MonitorCommand),
    BreakdownUpdated,
}

pub struct MonitorHandle {
    tx: Sender<PollerMsg>,
    phase2_tx: Sender<Phase2Cmd>,
    shutdown_flag: Arc<AtomicBool>,
    usage_cancel: Arc<Mutex<Arc<AtomicBool>>>,
    custom_cancel: Arc<Mutex<Arc<AtomicBool>>>,
    snapshot: Arc<Mutex<MonitorSnapshot>>,
    index_reload_count: Arc<AtomicU64>,
    model_scan_count: Arc<AtomicU64>,
    thread: Option<JoinHandle<()>>,
    phase2_thread: Option<JoinHandle<()>>,
}

enum Phase2Cmd {
    RefreshUsage {
        period: UsagePeriod,
        generation: u64,
        cancel: Arc<AtomicBool>,
    },
    RefreshCustomizations {
        generation: u64,
        cancel: Arc<AtomicBool>,
    },
    Shutdown,
}

impl MonitorHandle {
    pub fn start(config: MonitorConfig) -> Self {
        let initial = MonitorSnapshot {
            follow_latest: config.follow_latest,
            ..Default::default()
        };
        let snapshot = Arc::new(Mutex::new(initial));
        let snap_clone = Arc::clone(&snapshot);
        let index_reload_count = Arc::new(AtomicU64::new(0));
        let reload_count_clone = Arc::clone(&index_reload_count);
        let model_scan_count = Arc::new(AtomicU64::new(0));
        let scan_count_clone = Arc::clone(&model_scan_count);
        let shutdown_flag = Arc::new(AtomicBool::new(false));
        let usage_cancel = Arc::new(Mutex::new(Arc::new(AtomicBool::new(false))));
        let custom_cancel = Arc::new(Mutex::new(Arc::new(AtomicBool::new(false))));
        let latest_breakdown = Arc::new(Mutex::new(None::<CustomizationBreakdown>));
        let breakdown_version = Arc::new(AtomicU64::new(0));
        let latest_index = Arc::new(Mutex::new(None::<SessionIndex>));
        let usage_gen = Arc::new(AtomicU64::new(0));
        let custom_gen = Arc::new(AtomicU64::new(0));

        let (tx, rx) = mpsc::channel::<PollerMsg>();
        let (phase2_tx, phase2_rx) = mpsc::channel::<Phase2Cmd>();

        let poller_breakdown = Arc::clone(&latest_breakdown);
        let poller_breakdown_version = Arc::clone(&breakdown_version);
        let poller_index = Arc::clone(&latest_index);
        let poller_phase2_tx = phase2_tx.clone();
        let poller_shutdown = Arc::clone(&shutdown_flag);
        let poller_usage_cancel = Arc::clone(&usage_cancel);
        let poller_custom_cancel = Arc::clone(&custom_cancel);
        let poller_usage_gen = Arc::clone(&usage_gen);
        let poller_custom_gen = Arc::clone(&custom_gen);
        let poller_config_paths = config.paths.clone();
        let poller_notify = config.notify.clone();

        let thread = thread::Builder::new()
            .name("agy-monitor-poller".to_string())
            .spawn(move || {
                run_poller(
                    config,
                    rx,
                    snap_clone,
                    reload_count_clone,
                    scan_count_clone,
                    poller_breakdown,
                    poller_breakdown_version,
                    poller_index,
                    poller_phase2_tx,
                    poller_shutdown,
                    poller_usage_cancel,
                    poller_custom_cancel,
                    poller_usage_gen,
                    poller_custom_gen,
                );
            })
            .expect("failed to spawn monitor polling thread");

        let p2_snap = Arc::clone(&snapshot);
        let p2_breakdown = Arc::clone(&latest_breakdown);
        let p2_breakdown_version = Arc::clone(&breakdown_version);
        let p2_poller_tx = tx.clone();
        let p2_index = Arc::clone(&latest_index);
        let p2_shutdown = Arc::clone(&shutdown_flag);
        let phase2_thread = thread::Builder::new()
            .name("agy-monitor-phase2".to_string())
            .spawn(move || {
                run_phase2_worker(
                    poller_config_paths,
                    poller_notify,
                    phase2_rx,
                    p2_poller_tx,
                    p2_snap,
                    p2_breakdown,
                    p2_breakdown_version,
                    p2_index,
                    p2_shutdown,
                    usage_gen,
                    custom_gen,
                );
            })
            .expect("failed to spawn monitor phase2 worker");

        Self {
            tx,
            phase2_tx,
            shutdown_flag,
            usage_cancel,
            custom_cancel,
            snapshot,
            index_reload_count,
            model_scan_count,
            thread: Some(thread),
            phase2_thread: Some(phase2_thread),
        }
    }

    pub fn send(&self, cmd: MonitorCommand) {
        if matches!(cmd, MonitorCommand::Shutdown) {
            self.signal_cancel_all();
        }
        let _ = self.tx.send(PollerMsg::Public(cmd));
    }

    fn signal_cancel_all(&self) {
        self.shutdown_flag.store(true, Ordering::SeqCst);
        if let Ok(guard) = self.usage_cancel.lock() {
            guard.store(true, Ordering::SeqCst);
        }
        if let Ok(guard) = self.custom_cancel.lock() {
            guard.store(true, Ordering::SeqCst);
        }
        let _ = self.phase2_tx.send(Phase2Cmd::Shutdown);
    }

    pub fn snapshot(&self) -> MonitorSnapshot {
        self.snapshot.lock().map(|g| g.clone()).unwrap_or_default()
    }

    pub fn index_reload_count(&self) -> u64 {
        self.index_reload_count.load(Ordering::SeqCst)
    }

    pub fn model_scan_count(&self) -> u64 {
        self.model_scan_count.load(Ordering::SeqCst)
    }
}

impl Drop for MonitorHandle {
    fn drop(&mut self) {
        self.signal_cancel_all();
        let _ = self.tx.send(PollerMsg::Public(MonitorCommand::Shutdown));
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.phase2_thread.take() {
            let _ = handle.join();
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_poller(
    config: MonitorConfig,
    rx: mpsc::Receiver<PollerMsg>,
    snapshot: Arc<Mutex<MonitorSnapshot>>,
    index_reload_count: Arc<AtomicU64>,
    model_scan_count: Arc<AtomicU64>,
    latest_breakdown: Arc<Mutex<Option<CustomizationBreakdown>>>,
    breakdown_version: Arc<AtomicU64>,
    latest_index: Arc<Mutex<Option<SessionIndex>>>,
    phase2_tx: Sender<Phase2Cmd>,
    shutdown_flag: Arc<AtomicBool>,
    usage_cancel: Arc<Mutex<Arc<AtomicBool>>>,
    custom_cancel: Arc<Mutex<Arc<AtomicBool>>>,
    usage_gen: Arc<AtomicU64>,
    custom_gen: Arc<AtomicU64>,
) {
    let mut follow_latest = config.follow_latest;
    let mut paused = false;
    let mut selected_id: Option<String> = None;
    let mut cached_key: Option<AnalysisCacheKey> = None;
    let mut cached_analysis: Option<ConversationAnalysis> = None;
    let mut cached_model_from_db = false;
    let mut analysis_count: u64 = 0;
    let mut model_label_cache: HashMap<String, (DbFingerprint, String)> = HashMap::new();
    let mut cached_index: Option<SessionIndex> = None;
    let mut busy_this_poll: HashSet<String> = HashSet::new();

    // Perform initial poll immediately on startup.
    let mut pending_backfill = perform_poll(
        &config,
        &mut follow_latest,
        &mut selected_id,
        &mut cached_key,
        &mut cached_analysis,
        &mut cached_model_from_db,
        &mut analysis_count,
        &mut model_label_cache,
        &mut cached_index,
        &mut busy_this_poll,
        paused,
        false,
        false,
        &index_reload_count,
        &model_scan_count,
        &latest_breakdown,
        &breakdown_version,
        &latest_index,
        &snapshot,
    );
    let mut last_full_poll_at = Instant::now();

    loop {
        if shutdown_flag.load(Ordering::SeqCst) {
            break;
        }
        let wait_timeout = if !paused && pending_backfill {
            config
                .poll_interval
                .saturating_sub(last_full_poll_at.elapsed())
                .min(MODEL_BACKFILL_INTERVAL)
        } else if !paused {
            config
                .poll_interval
                .saturating_sub(last_full_poll_at.elapsed())
        } else {
            config.poll_interval
        };
        match rx.recv_timeout(wait_timeout) {
            Ok(PollerMsg::Public(MonitorCommand::Shutdown))
            | Err(RecvTimeoutError::Disconnected) => break,
            Ok(msg) => {
                let mut force_reanalyze = false;
                let mut should_poll = false;
                let mut refresh_breakdown = false;
                let mut reanalyze_cached = false;
                if matches!(
                    apply_poller_msg(
                        msg,
                        &mut follow_latest,
                        &mut paused,
                        &mut selected_id,
                        &mut should_poll,
                        &mut force_reanalyze,
                        &mut refresh_breakdown,
                        &mut reanalyze_cached,
                        &snapshot,
                        &config.notify,
                        &phase2_tx,
                        &shutdown_flag,
                        &usage_cancel,
                        &custom_cancel,
                        &usage_gen,
                        &custom_gen,
                    ),
                    CommandOutcome::Exit
                ) {
                    break;
                }

                // Drain any queued messages before polling or re-analyzing.
                while let Ok(next_msg) = rx.try_recv() {
                    if matches!(
                        apply_poller_msg(
                            next_msg,
                            &mut follow_latest,
                            &mut paused,
                            &mut selected_id,
                            &mut should_poll,
                            &mut force_reanalyze,
                            &mut refresh_breakdown,
                            &mut reanalyze_cached,
                            &snapshot,
                            &config.notify,
                            &phase2_tx,
                            &shutdown_flag,
                            &usage_cancel,
                            &custom_cancel,
                            &usage_gen,
                            &custom_gen,
                        ),
                        CommandOutcome::Exit
                    ) {
                        return;
                    }
                }

                if should_poll {
                    busy_this_poll.clear();
                    pending_backfill = perform_poll(
                        &config,
                        &mut follow_latest,
                        &mut selected_id,
                        &mut cached_key,
                        &mut cached_analysis,
                        &mut cached_model_from_db,
                        &mut analysis_count,
                        &mut model_label_cache,
                        &mut cached_index,
                        &mut busy_this_poll,
                        paused,
                        force_reanalyze,
                        refresh_breakdown,
                        &index_reload_count,
                        &model_scan_count,
                        &latest_breakdown,
                        &breakdown_version,
                        &latest_index,
                        &snapshot,
                    );
                    last_full_poll_at = Instant::now();
                } else if reanalyze_cached {
                    reanalyze_selected_with_cached_index(
                        &config,
                        selected_id.as_deref(),
                        cached_index.as_ref(),
                        &mut cached_key,
                        &mut cached_analysis,
                        &mut cached_model_from_db,
                        &mut analysis_count,
                        &latest_breakdown,
                        &breakdown_version,
                        &snapshot,
                    );
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if !paused {
                    if cached_index.is_none() || last_full_poll_at.elapsed() >= config.poll_interval
                    {
                        busy_this_poll.clear();
                        pending_backfill = perform_poll(
                            &config,
                            &mut follow_latest,
                            &mut selected_id,
                            &mut cached_key,
                            &mut cached_analysis,
                            &mut cached_model_from_db,
                            &mut analysis_count,
                            &mut model_label_cache,
                            &mut cached_index,
                            &mut busy_this_poll,
                            paused,
                            false,
                            false,
                            &index_reload_count,
                            &model_scan_count,
                            &latest_breakdown,
                            &breakdown_version,
                            &latest_index,
                            &snapshot,
                        );
                        last_full_poll_at = Instant::now();
                    } else if pending_backfill {
                        pending_backfill = perform_model_backfill_tick(
                            &config,
                            selected_id.as_deref(),
                            cached_key.as_ref(),
                            cached_analysis.as_ref(),
                            cached_model_from_db,
                            &mut model_label_cache,
                            &mut cached_index,
                            &mut busy_this_poll,
                            &model_scan_count,
                            &snapshot,
                        );
                    }
                }
            }
        }
    }
}

fn dispatch_usage_refresh(
    period: UsagePeriod,
    snapshot: &Arc<Mutex<MonitorSnapshot>>,
    notify: &Option<Arc<dyn Fn() + Send + Sync>>,
    phase2_tx: &Sender<Phase2Cmd>,
    shutdown_flag: &Arc<AtomicBool>,
    usage_cancel: &Arc<Mutex<Arc<AtomicBool>>>,
    usage_gen: &Arc<AtomicU64>,
) {
    if shutdown_flag.load(Ordering::SeqCst) {
        return;
    }
    let generation = usage_gen.fetch_add(1, Ordering::SeqCst) + 1;
    let next_cancel = Arc::new(AtomicBool::new(false));
    if let Ok(mut guard) = usage_cancel.lock() {
        guard.store(true, Ordering::SeqCst);
        *guard = Arc::clone(&next_cancel);
        if shutdown_flag.load(Ordering::SeqCst) {
            next_cancel.store(true, Ordering::SeqCst);
            return;
        }
    }
    if let Ok(mut guard) = snapshot.lock() {
        guard.usage_loading = true;
    }
    if let Some(cb) = notify {
        cb();
    }
    let _ = phase2_tx.send(Phase2Cmd::RefreshUsage {
        period,
        generation,
        cancel: next_cancel,
    });
}

fn dispatch_customizations_refresh(
    snapshot: &Arc<Mutex<MonitorSnapshot>>,
    notify: &Option<Arc<dyn Fn() + Send + Sync>>,
    phase2_tx: &Sender<Phase2Cmd>,
    shutdown_flag: &Arc<AtomicBool>,
    custom_cancel: &Arc<Mutex<Arc<AtomicBool>>>,
    custom_gen: &Arc<AtomicU64>,
) {
    if shutdown_flag.load(Ordering::SeqCst) {
        return;
    }
    let generation = custom_gen.fetch_add(1, Ordering::SeqCst) + 1;
    let next_cancel = Arc::new(AtomicBool::new(false));
    if let Ok(mut guard) = custom_cancel.lock() {
        guard.store(true, Ordering::SeqCst);
        *guard = Arc::clone(&next_cancel);
        if shutdown_flag.load(Ordering::SeqCst) {
            next_cancel.store(true, Ordering::SeqCst);
            return;
        }
    }
    if let Ok(mut guard) = snapshot.lock() {
        guard.customizations_loading = true;
    }
    if let Some(cb) = notify {
        cb();
    }
    let _ = phase2_tx.send(Phase2Cmd::RefreshCustomizations {
        generation,
        cancel: next_cancel,
    });
}

#[allow(clippy::too_many_arguments)]
fn run_phase2_worker(
    paths: GeminiPaths,
    notify: Option<Arc<dyn Fn() + Send + Sync>>,
    rx: mpsc::Receiver<Phase2Cmd>,
    poller_tx: Sender<PollerMsg>,
    snapshot: Arc<Mutex<MonitorSnapshot>>,
    latest_breakdown: Arc<Mutex<Option<CustomizationBreakdown>>>,
    breakdown_version: Arc<AtomicU64>,
    latest_index: Arc<Mutex<Option<SessionIndex>>>,
    shutdown_flag: Arc<AtomicBool>,
    usage_gen: Arc<AtomicU64>,
    custom_gen: Arc<AtomicU64>,
) {
    let mut usage_cache = UsageCache::new();

    while let Ok(cmd) = rx.recv() {
        if shutdown_flag.load(Ordering::SeqCst) {
            break;
        }
        match cmd {
            Phase2Cmd::Shutdown => break,
            Phase2Cmd::RefreshUsage {
                period,
                generation,
                cancel,
            } => {
                if shutdown_flag.load(Ordering::SeqCst)
                    || cancel.load(Ordering::SeqCst)
                    || usage_gen.load(Ordering::SeqCst) != generation
                {
                    continue;
                }
                let index = load_session_index(&paths);
                if let Ok(mut guard) = latest_index.lock() {
                    *guard = Some(index.clone());
                }
                if shutdown_flag.load(Ordering::SeqCst)
                    || cancel.load(Ordering::SeqCst)
                    || usage_gen.load(Ordering::SeqCst) != generation
                {
                    continue;
                }
                let report = aggregate_usage(
                    &index,
                    period,
                    DEFAULT_MAX_USAGE_SESSIONS,
                    &mut usage_cache,
                    &cancel,
                );
                if shutdown_flag.load(Ordering::SeqCst)
                    || usage_gen.load(Ordering::SeqCst) != generation
                {
                    continue;
                }
                if let Ok(mut guard) = snapshot.lock() {
                    if let Some(rep) = report {
                        guard.usage_report = Some(rep);
                    }
                    guard.usage_loading = false;
                }
                if let Some(cb) = &notify {
                    cb();
                }
            }
            Phase2Cmd::RefreshCustomizations { generation, cancel } => {
                if shutdown_flag.load(Ordering::SeqCst)
                    || cancel.load(Ordering::SeqCst)
                    || custom_gen.load(Ordering::SeqCst) != generation
                {
                    continue;
                }
                let inv = scan_customizations_cancellable(&paths, &cancel);
                if shutdown_flag.load(Ordering::SeqCst)
                    || custom_gen.load(Ordering::SeqCst) != generation
                {
                    continue;
                }
                let mut breakdown_changed = false;
                if let Some(inv_val) = inv {
                    let bd = inv_val.breakdown.clone();
                    if let Ok(mut guard) = latest_breakdown.lock()
                        && guard.as_ref() != Some(&bd)
                    {
                        *guard = Some(bd);
                        breakdown_version.fetch_add(1, Ordering::SeqCst);
                        breakdown_changed = true;
                    }
                    if let Ok(mut guard) = snapshot.lock() {
                        guard.customizations = Some(inv_val);
                        guard.customizations_loading = false;
                    }
                } else if let Ok(mut guard) = snapshot.lock() {
                    guard.customizations_loading = false;
                }
                if let Some(cb) = &notify {
                    cb();
                }
                if breakdown_changed {
                    let _ = poller_tx.send(PollerMsg::BreakdownUpdated);
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
fn apply_poller_msg(
    msg: PollerMsg,
    follow_latest: &mut bool,
    paused: &mut bool,
    selected_id: &mut Option<String>,
    should_poll: &mut bool,
    force_reanalyze: &mut bool,
    refresh_breakdown: &mut bool,
    reanalyze_cached: &mut bool,
    snapshot: &Arc<Mutex<MonitorSnapshot>>,
    notify: &Option<Arc<dyn Fn() + Send + Sync>>,
    phase2_tx: &Sender<Phase2Cmd>,
    shutdown_flag: &Arc<AtomicBool>,
    usage_cancel: &Arc<Mutex<Arc<AtomicBool>>>,
    custom_cancel: &Arc<Mutex<Arc<AtomicBool>>>,
    usage_gen: &Arc<AtomicU64>,
    custom_gen: &Arc<AtomicU64>,
) -> CommandOutcome {
    match msg {
        PollerMsg::Public(cmd) => apply_command(
            cmd,
            follow_latest,
            paused,
            selected_id,
            should_poll,
            force_reanalyze,
            refresh_breakdown,
            snapshot,
            notify,
            phase2_tx,
            shutdown_flag,
            usage_cancel,
            custom_cancel,
            usage_gen,
            custom_gen,
        ),
        PollerMsg::BreakdownUpdated => {
            *reanalyze_cached = true;
            CommandOutcome::Continue
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_command(
    cmd: MonitorCommand,
    follow_latest: &mut bool,
    paused: &mut bool,
    selected_id: &mut Option<String>,
    should_poll: &mut bool,
    force_reanalyze: &mut bool,
    refresh_breakdown: &mut bool,
    snapshot: &Arc<Mutex<MonitorSnapshot>>,
    notify: &Option<Arc<dyn Fn() + Send + Sync>>,
    phase2_tx: &Sender<Phase2Cmd>,
    shutdown_flag: &Arc<AtomicBool>,
    usage_cancel: &Arc<Mutex<Arc<AtomicBool>>>,
    custom_cancel: &Arc<Mutex<Arc<AtomicBool>>>,
    usage_gen: &Arc<AtomicU64>,
    custom_gen: &Arc<AtomicU64>,
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
            *refresh_breakdown = true;
        }
        MonitorCommand::RefreshUsage(period) => {
            dispatch_usage_refresh(
                period,
                snapshot,
                notify,
                phase2_tx,
                shutdown_flag,
                usage_cancel,
                usage_gen,
            );
        }
        MonitorCommand::RefreshCustomizations => {
            dispatch_customizations_refresh(
                snapshot,
                notify,
                phase2_tx,
                shutdown_flag,
                custom_cancel,
                custom_gen,
            );
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
fn analyze_selected_in_index(
    config: &MonitorConfig,
    index: &SessionIndex,
    selected_id: Option<&str>,
    force_reanalyze: bool,
    cached_key: &mut Option<AnalysisCacheKey>,
    cached_analysis: &mut Option<ConversationAnalysis>,
    cached_model_from_db: &mut bool,
    analysis_count: &mut u64,
    latest_breakdown: &Arc<Mutex<Option<CustomizationBreakdown>>>,
    breakdown_version: &Arc<AtomicU64>,
) -> (bool, Option<DbFingerprint>, Option<String>, bool) {
    if cached_analysis.as_ref().map(|a| a.conversation_id.as_str()) != selected_id {
        *cached_key = None;
        *cached_analysis = None;
        *cached_model_from_db = false;
    }

    let Some(conv_id) = selected_id else {
        *cached_key = None;
        *cached_analysis = None;
        *cached_model_from_db = false;
        return (false, None, None, false);
    };
    let Some(summary) = index.by_id.get(conv_id) else {
        *cached_key = None;
        *cached_analysis = None;
        *cached_model_from_db = false;
        return (false, None, None, false);
    };

    let app_data = &summary.app_data_dir;
    let logs_dir = app_data
        .join("brain")
        .join(conv_id)
        .join(".system_generated")
        .join("logs");
    let conv_db = app_data.join("conversations").join(format!("{conv_id}.db"));
    let pbtxt_path = app_data.join("antigravity_state.pbtxt");
    let db_fp = DbFingerprint::from_db_path(&conv_db);

    let (cached_bd, bd_ver) = if let Ok(guard) = latest_breakdown.lock() {
        (guard.clone(), breakdown_version.load(Ordering::SeqCst))
    } else {
        (None, breakdown_version.load(Ordering::SeqCst))
    };

    let mut key = AnalysisCacheKey {
        conv_id: conv_id.to_string(),
        transcript_fp: TranscriptFingerprint::from_logs_dir(&logs_dir),
        db_fp,
        pbtxt_fp: FileStamp::from_path(&pbtxt_path),
        breakdown_version: bd_ver,
    };

    let cache_hit = !force_reanalyze && cached_key.as_ref() == Some(&key);
    if cache_hit {
        let analysis_db_busy = cached_analysis.as_ref().is_some_and(|a| a.db_busy);
        return (analysis_db_busy, Some(db_fp), None, false);
    }

    let breakdown = if let Some(bd) = cached_bd {
        Some(bd)
    } else {
        let scanned = crate::customizations::scan_customizations(&config.paths).breakdown;
        if let Ok(mut guard) = latest_breakdown.lock() {
            if let Some(existing) = guard.as_ref() {
                key.breakdown_version = breakdown_version.load(Ordering::SeqCst);
                Some(existing.clone())
            } else {
                *guard = Some(scanned.clone());
                Some(scanned)
            }
        } else {
            Some(scanned)
        }
    };

    match analyze_conversation_with_meta_and_customizations(
        app_data, conv_id, None, None, breakdown,
    ) {
        Ok((res, model_from_db)) => {
            let analysis_db_busy = res.db_busy;
            // Do not freeze the cache key if the DB was busy so the next poll
            // retries reading model/limit metadata once the lock clears.
            *cached_key = if res.db_busy { None } else { Some(key) };
            *cached_analysis = Some(res);
            *cached_model_from_db = model_from_db;
            *analysis_count += 1;
            (analysis_db_busy, Some(db_fp), None, true)
        }
        Err(e) => {
            *cached_key = None;
            *cached_analysis = None;
            *cached_model_from_db = false;
            (false, Some(db_fp), Some(e.to_string()), true)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn reanalyze_selected_with_cached_index(
    config: &MonitorConfig,
    selected_id: Option<&str>,
    cached_index: Option<&SessionIndex>,
    cached_key: &mut Option<AnalysisCacheKey>,
    cached_analysis: &mut Option<ConversationAnalysis>,
    cached_model_from_db: &mut bool,
    analysis_count: &mut u64,
    latest_breakdown: &Arc<Mutex<Option<CustomizationBreakdown>>>,
    breakdown_version: &Arc<AtomicU64>,
    snapshot: &Arc<Mutex<MonitorSnapshot>>,
) {
    let Some(index) = cached_index else {
        return;
    };

    let (analysis_db_busy, _selected_db_fp, last_error, updated) = analyze_selected_in_index(
        config,
        index,
        selected_id,
        false,
        cached_key,
        cached_analysis,
        cached_model_from_db,
        analysis_count,
        latest_breakdown,
        breakdown_version,
    );
    if !updated {
        return;
    }

    let db_busy = index.db_busy || analysis_db_busy;
    let now = Utc::now();
    if let Ok(mut guard) = snapshot.lock() {
        guard.analysis = cached_analysis.clone();
        guard.last_updated_at = Some(now);
        guard.last_error = last_error;
        guard.db_busy = db_busy;
        guard.analysis_count = *analysis_count;
    }
    if let Some(cb) = &config.notify {
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
    cached_model_from_db: &mut bool,
    analysis_count: &mut u64,
    model_label_cache: &mut HashMap<String, (DbFingerprint, String)>,
    cached_index: &mut Option<SessionIndex>,
    busy_this_poll: &mut HashSet<String>,
    paused: bool,
    force_reanalyze: bool,
    refresh_breakdown: bool,
    index_reload_count: &Arc<AtomicU64>,
    model_scan_count: &Arc<AtomicU64>,
    latest_breakdown: &Arc<Mutex<Option<CustomizationBreakdown>>>,
    breakdown_version: &Arc<AtomicU64>,
    latest_index: &Arc<Mutex<Option<SessionIndex>>>,
    snapshot: &Arc<Mutex<MonitorSnapshot>>,
) -> bool {
    if refresh_breakdown {
        let fresh_bd = crate::customizations::scan_customizations(&config.paths).breakdown;
        if let Ok(mut guard) = latest_breakdown.lock()
            && guard.as_ref() != Some(&fresh_bd)
        {
            *guard = Some(fresh_bd);
            breakdown_version.fetch_add(1, Ordering::SeqCst);
        }
    }

    index_reload_count.fetch_add(1, Ordering::SeqCst);
    let mut index = load_session_index(&config.paths);

    if *follow_latest {
        if let Some(latest) = index.latest_root_id() {
            *selected_id = Some(latest.to_string());
        }
    } else if selected_id.is_none()
        && let Some(latest) = index.latest_root_id()
    {
        *selected_id = Some(latest.to_string());
    }

    let (analysis_db_busy, selected_db_fp, last_error, _) = analyze_selected_in_index(
        config,
        &index,
        selected_id.as_deref(),
        force_reanalyze,
        cached_key,
        cached_analysis,
        cached_model_from_db,
        analysis_count,
        latest_breakdown,
        breakdown_version,
    );

    let pending_backfill = populate_session_model_labels(
        &mut index,
        selected_id.as_deref(),
        selected_db_fp,
        analysis_db_busy,
        cached_analysis.as_ref(),
        *cached_model_from_db,
        true,
        model_label_cache,
        busy_this_poll,
        model_scan_count,
    );

    let db_busy = index.db_busy || analysis_db_busy;
    let now = Utc::now();

    if let Ok(mut guard) = latest_index.lock() {
        *guard = Some(index.clone());
    }
    if let Ok(mut guard) = snapshot.lock() {
        guard.index = index.clone();
        guard.selected_id = selected_id.clone();
        guard.analysis = cached_analysis.clone();
        guard.follow_latest = *follow_latest;
        guard.paused = paused;
        guard.last_updated_at = Some(now);
        guard.last_error = last_error;
        guard.db_busy = db_busy;
        guard.analysis_count = *analysis_count;
    }
    *cached_index = Some(index);

    if let Some(cb) = &config.notify {
        cb();
    }

    pending_backfill
}

#[allow(clippy::too_many_arguments)]
fn perform_model_backfill_tick(
    config: &MonitorConfig,
    selected_id: Option<&str>,
    cached_key: Option<&AnalysisCacheKey>,
    cached_analysis: Option<&ConversationAnalysis>,
    cached_model_from_db: bool,
    model_label_cache: &mut HashMap<String, (DbFingerprint, String)>,
    cached_index: &mut Option<SessionIndex>,
    busy_this_poll: &mut HashSet<String>,
    model_scan_count: &Arc<AtomicU64>,
    snapshot: &Arc<Mutex<MonitorSnapshot>>,
) -> bool {
    let Some(index) = cached_index.as_mut() else {
        return false;
    };

    let selected_db_fp = selected_id.and_then(|id| {
        index.by_id.get(id).map(|summary| {
            let conv_db = summary
                .app_data_dir
                .join("conversations")
                .join(format!("{id}.db"));
            DbFingerprint::from_db_path(&conv_db)
        })
    });
    let selected_db_busy = cached_analysis
        .is_some_and(|a| Some(a.conversation_id.as_str()) == selected_id && a.db_busy);
    let can_seed_selected = cached_key.is_some_and(|k| {
        Some(k.conv_id.as_str()) == selected_id && Some(k.db_fp) == selected_db_fp
    });

    let has_more = populate_session_model_labels(
        index,
        selected_id,
        selected_db_fp,
        selected_db_busy,
        cached_analysis,
        cached_model_from_db,
        can_seed_selected,
        model_label_cache,
        busy_this_poll,
        model_scan_count,
    );

    let db_busy = index.db_busy || selected_db_busy;
    let now = Utc::now();

    if let Ok(mut guard) = snapshot.lock() {
        guard.index = index.clone();
        guard.last_updated_at = Some(now);
        guard.db_busy = db_busy;
    }

    if let Some(cb) = &config.notify {
        cb();
    }

    has_more
}

#[allow(clippy::too_many_arguments)]
fn populate_session_model_labels(
    index: &mut SessionIndex,
    selected_id: Option<&str>,
    selected_db_fp: Option<DbFingerprint>,
    selected_db_busy: bool,
    cached_analysis: Option<&ConversationAnalysis>,
    cached_model_from_db: bool,
    can_seed_selected: bool,
    cache: &mut HashMap<String, (DbFingerprint, String)>,
    busy_this_poll: &mut HashSet<String>,
    model_scan_count: &Arc<AtomicU64>,
) -> bool {
    // Evict removed sessions to keep cache bounded.
    cache.retain(|id, _| index.by_id.contains_key(id));

    // Reuse the full analysis result for the selected session without re-opening its DB.
    if can_seed_selected
        && let (Some(sel), Some(sel_fp), Some(analysis)) =
            (selected_id, selected_db_fp, cached_analysis)
        && analysis.conversation_id == sel
        && !selected_db_busy
    {
        if cached_model_from_db {
            let label = if analysis.model.model_id.is_empty()
                || analysis.model.model_label == "Default model"
            {
                String::new()
            } else {
                analysis.model.model_label.clone()
            };
            cache.insert(sel.to_string(), (sel_fp, label));
        } else {
            let preserve_existing = cache
                .get(sel)
                .is_some_and(|(fp, lbl)| *fp == sel_fp && !lbl.is_empty());
            if !preserve_existing {
                cache.insert(sel.to_string(), (sel_fp, String::new()));
            }
        }
    }

    let mut ordered_ids: Vec<String> = Vec::with_capacity(index.by_id.len());
    let mut seen: HashSet<String> = HashSet::with_capacity(index.by_id.len());

    if let Some(sel) = selected_id
        && index.by_id.contains_key(sel)
        && seen.insert(sel.to_string())
    {
        ordered_ids.push(sel.to_string());
    }
    for node in index.roots.iter().take(40) {
        if seen.insert(node.summary.conversation_id.clone()) {
            ordered_ids.push(node.summary.conversation_id.clone());
        }
        for child in &node.children {
            if seen.insert(child.conversation_id.clone()) {
                ordered_ids.push(child.conversation_id.clone());
            }
        }
    }
    for node in index.roots.iter().skip(40) {
        if seen.insert(node.summary.conversation_id.clone()) {
            ordered_ids.push(node.summary.conversation_id.clone());
        }
        for child in &node.children {
            if seen.insert(child.conversation_id.clone()) {
                ordered_ids.push(child.conversation_id.clone());
            }
        }
    }
    for id in index.by_id.keys() {
        if seen.insert(id.clone()) {
            ordered_ids.push(id.clone());
        }
    }

    let mut scans = 0usize;
    let mut has_more = false;
    let mut resolved: HashMap<String, String> = HashMap::with_capacity(ordered_ids.len());

    for id in ordered_ids {
        let Some(summary) = index.by_id.get(&id) else {
            continue;
        };
        let db_path = summary
            .app_data_dir
            .join("conversations")
            .join(format!("{id}.db"));
        let fp = DbFingerprint::from_db_path(&db_path);

        if let Some((cached_fp, cached_label)) = cache.get(&id)
            && *cached_fp == fp
        {
            resolved.insert(id, cached_label.clone());
            continue;
        }

        if !fp.exists() {
            cache.insert(id.clone(), (fp, String::new()));
            resolved.insert(id, String::new());
            continue;
        }

        // Do not re-open the selected session's DB in the same tick if analyze_conversation just hit db_busy.
        if selected_db_busy && selected_id == Some(id.as_str()) {
            if let Some((_, old_label)) = cache.get(&id) {
                resolved.insert(id, old_label.clone());
            }
            continue;
        }

        if busy_this_poll.contains(&id) {
            if let Some((_, old_label)) = cache.get(&id) {
                resolved.insert(id, old_label.clone());
            }
            continue;
        }

        if scans >= MAX_MODEL_SCANS_PER_TICK {
            has_more = true;
            if let Some((_, old_label)) = cache.get(&id) {
                resolved.insert(id, old_label.clone());
            }
            continue;
        }

        scans += 1;
        model_scan_count.fetch_add(1, Ordering::SeqCst);
        let (label_opt, busy) = detect_conversation_model_label_from_db(&summary.app_data_dir, &id);
        let label = label_opt.unwrap_or_default();
        if !busy {
            cache.insert(id.clone(), (fp, label.clone()));
        } else {
            busy_this_poll.insert(id.clone());
            if let Some((_, old_label)) = cache.get(&id)
                && label.is_empty()
            {
                resolved.insert(id, old_label.clone());
                continue;
            }
        }
        resolved.insert(id, label);
    }

    // Keep the selected session synchronized with the full conversation analysis label
    // only when the cached analysis still matches the current DB fingerprint, was not busy,
    // and was derived from the session DB (never pbtxt fallback).
    if can_seed_selected
        && !selected_db_busy
        && cached_model_from_db
        && let (Some(sel), Some(analysis)) = (selected_id, cached_analysis)
        && analysis.conversation_id == sel
        && !analysis.model.model_label.is_empty()
        && analysis.model.model_label != "Default model"
    {
        resolved.insert(sel.to_string(), analysis.model.model_label.clone());
    }

    for (id, sum) in &mut index.by_id {
        if let Some(label) = resolved.get(id) {
            sum.model_label = label.clone();
        }
    }
    for node in &mut index.roots {
        if let Some(label) = resolved.get(&node.summary.conversation_id) {
            node.summary.model_label = label.clone();
        }
        for child in &mut node.children {
            if let Some(label) = resolved.get(&child.conversation_id) {
                child.model_label = label.clone();
            }
        }
    }

    has_more
}
