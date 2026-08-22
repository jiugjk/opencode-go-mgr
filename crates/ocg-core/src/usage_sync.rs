//! Adaptive official OpenCode Go usage synchronization.
//!
//! Official usage is a periodic calibration baseline. Local forward_logs remain
//! the immediate real-time estimator after the last successful calibration.
//! Manual and background paths share one secure fetch + key CAS implementation.

use crate::db::{
    AccountUsageCalibrationSnapshot, AccountUsageSyncState, AccountUsageSyncSuccessMetadata,
    Database,
};
use crate::go_usage::{GoUsageError, GoUsageSnapshot};
use crate::models::UsageWindow;
use crate::pricing::PricingLimits;
use crate::state::CoreState;
use chrono::{DateTime, Duration, Utc};
use futures_util::future::FutureExt;
use parking_lot::Mutex as ParkingMutex;
use serde::Serialize;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration as StdDuration;
use tokio::sync::{Mutex as AsyncMutex, Notify};

/// Manual refresh may not re-attempt the same account more often than this.
pub const MANUAL_THROTTLE: Duration = Duration::seconds(60);
/// Ready accounts with local activity in the last day refresh about hourly.
pub const ACTIVE_CADENCE: Duration = Duration::hours(1);
/// Ready accounts without recent local activity refresh about daily.
pub const INACTIVE_CADENCE: Duration = Duration::hours(24);
/// Lookback used to classify an account as locally active.
pub const ACTIVITY_LOOKBACK: Duration = Duration::hours(24);
/// Expedited sync when local max Go usage is at or above this percent.
pub const EXPEDITE_THRESHOLD_PERCENT: f64 = 80.0;
/// Minimum gap between expedited reconciliations for one account.
pub const EXPEDITE_GUARD: Duration = Duration::minutes(15);
/// Lower bound for delayed official sync after a real inference 429.
pub const INFERENCE_429_DELAY_MIN: Duration = Duration::minutes(1);
/// Upper bound for delayed official sync after a real inference 429.
pub const INFERENCE_429_DELAY_MAX: Duration = Duration::minutes(2);
/// Bounded jitter after an official window reset before reconciling.
pub const RESET_JITTER_MAX: Duration = Duration::minutes(3);
/// Startup deferral spread so a restart does not stampede official fetches.
pub const STARTUP_SPREAD_MAX: Duration = Duration::minutes(15);
/// Idle sleep when nothing is due; wake notifications interrupt this.
pub const SCHEDULER_IDLE_TICK: StdDuration = StdDuration::from_secs(30);
/// Serial pacing between background refreshes.
pub const SCHEDULER_PACE: StdDuration = StdDuration::from_secs(2);

const FAILURE_BACKOFF: &[Duration] = &[
    Duration::minutes(5),
    Duration::minutes(15),
    Duration::hours(1),
    Duration::hours(6),
];

/// Why a refresh was requested. Does not change network/CAS behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageSyncTrigger {
    Manual,
    Scheduled,
    Expedited,
    Reset,
    Inference429,
}

/// Successful official refresh outcome shared by dashboard and scheduler.
#[derive(Debug, Clone, Serialize)]
pub struct OfficialUsageRefreshSuccess {
    pub usage: UsageWindow,
    pub source: &'static str,
    pub last_success_at: String,
    pub next_allowed_at: String,
}

/// Typed refresh failure. Display never includes keys or upstream bodies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OfficialUsageRefreshError {
    NotFound,
    NotEligible(&'static str),
    Conflict(&'static str),
    Throttled {
        next_allowed_at: DateTime<Utc>,
        retry_after_secs: u64,
    },
    Upstream(GoUsageError),
    Internal(String),
}

impl std::fmt::Display for OfficialUsageRefreshError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => f.write_str("account not found"),
            Self::NotEligible(message) | Self::Conflict(message) => f.write_str(message),
            Self::Throttled {
                retry_after_secs, ..
            } => write!(
                f,
                "official Go usage refresh is temporarily throttled; retry after {retry_after_secs}s"
            ),
            Self::Upstream(error) => write!(f, "{error}"),
            Self::Internal(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for OfficialUsageRefreshError {}

type RefreshResult = Result<OfficialUsageRefreshSuccess, OfficialUsageRefreshError>;
type RefreshFuture =
    futures_util::future::Shared<Pin<Box<dyn Future<Output = Arc<RefreshResult>> + Send>>>;
type ClockFn = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;
type JitterFn = Arc<dyn Fn() -> f64 + Send + Sync>;
type FetchFuture = Pin<Box<dyn Future<Output = Result<GoUsageSnapshot, GoUsageError>> + Send>>;
type FetchFn = Arc<dyn Fn(crate::models::AppConfig, String) -> FetchFuture + Send + Sync>;
type CleanupHook = Arc<dyn Fn() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

#[derive(Clone)]
struct InflightEntry {
    generation: u64,
    future: RefreshFuture,
}

/// Process-wide gates for concurrency-1, in-flight dedupe, and wakeups.
pub struct UsageSyncRuntime {
    global: AsyncMutex<()>,
    inflight: AsyncMutex<HashMap<String, InflightEntry>>,
    inflight_generation: AtomicU64,
    /// Arc so the scheduler can wait without pinning `CoreState` alive.
    wake: Arc<Notify>,
    loop_started: AtomicBool,
    /// Optional injectable clock for tests. Production uses `Utc::now`.
    clock: ParkingMutex<Option<ClockFn>>,
    /// Optional injectable jitter (0.0..1.0) for tests.
    jitter: ParkingMutex<Option<JitterFn>>,
    /// Optional fetch seam for tests. Production uses `go_usage::fetch_go_usage`.
    fetch: ParkingMutex<Option<FetchFn>>,
    /// Optional hook run after an in-flight future resolves and before
    /// generation-scoped cleanup (tests only).
    before_inflight_cleanup: ParkingMutex<Option<CleanupHook>>,
}

impl Default for UsageSyncRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl UsageSyncRuntime {
    pub fn new() -> Self {
        Self {
            global: AsyncMutex::new(()),
            inflight: AsyncMutex::new(HashMap::new()),
            inflight_generation: AtomicU64::new(1),
            wake: Arc::new(Notify::new()),
            loop_started: AtomicBool::new(false),
            clock: ParkingMutex::new(None),
            jitter: ParkingMutex::new(None),
            fetch: ParkingMutex::new(None),
            before_inflight_cleanup: ParkingMutex::new(None),
        }
    }

    pub fn set_clock_for_test(&self, clock: impl Fn() -> DateTime<Utc> + Send + Sync + 'static) {
        *self.clock.lock() = Some(Arc::new(clock));
    }

    pub fn set_jitter_for_test(&self, jitter: impl Fn() -> f64 + Send + Sync + 'static) {
        *self.jitter.lock() = Some(Arc::new(jitter));
    }

    pub fn set_fetch_for_test(
        &self,
        fetch: impl Fn(crate::models::AppConfig, String) -> FetchFuture + Send + Sync + 'static,
    ) {
        *self.fetch.lock() = Some(Arc::new(fetch));
    }

    pub fn set_before_inflight_cleanup_for_test(
        &self,
        hook: impl Fn() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync + 'static,
    ) {
        *self.before_inflight_cleanup.lock() = Some(Arc::new(hook));
    }

    pub fn clear_test_seams(&self) {
        *self.clock.lock() = None;
        *self.jitter.lock() = None;
        *self.fetch.lock() = None;
        *self.before_inflight_cleanup.lock() = None;
    }

    pub fn now(&self) -> DateTime<Utc> {
        self.clock
            .lock()
            .as_ref()
            .map(|clock| clock())
            .unwrap_or_else(Utc::now)
    }

    fn jitter01(&self) -> f64 {
        self.jitter
            .lock()
            .as_ref()
            .map(|jitter| jitter().clamp(0.0, 1.0))
            .unwrap_or_else(random_jitter01)
    }

    fn wake(&self) {
        self.wake.notify_one();
    }

    fn wake_handle(&self) -> Arc<Notify> {
        self.wake.clone()
    }
}

fn random_jitter01() -> f64 {
    // Cheap deterministic-enough mix from UUID bits; tests inject an exact seam.
    let bits = uuid::Uuid::new_v4().as_u128();
    ((bits % 10_000) as f64) / 10_000.0
}

fn scale_duration(base: Duration, jitter01: f64) -> Duration {
    let millis = base.num_milliseconds().max(0) as f64;
    Duration::milliseconds((millis * jitter01.clamp(0.0, 1.0)).round() as i64)
}

fn duration_between(min: Duration, max: Duration, jitter01: f64) -> Duration {
    if max <= min {
        return min;
    }
    min + scale_duration(max - min, jitter01)
}

/// Failure backoff ladder: 5m → 15m → 1h → 6h (capped).
pub fn failure_backoff(failure_streak_after: u32) -> Duration {
    let index = failure_streak_after.saturating_sub(1) as usize;
    FAILURE_BACKOFF
        .get(index)
        .copied()
        .unwrap_or(*FAILURE_BACKOFF.last().expect("backoff ladder non-empty"))
}

pub fn cadence_for(active_in_lookback: bool) -> Duration {
    if active_in_lookback {
        ACTIVE_CADENCE
    } else {
        INACTIVE_CADENCE
    }
}

pub fn manual_next_allowed_at(
    last_attempt_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let last = last_attempt_at?;
    let until = last + MANUAL_THROTTLE;
    (until > now).then_some(until)
}

pub fn max_go_usage_percent(usage: &UsageWindow, limits: &PricingLimits) -> f64 {
    let pct = |cost: f64, limit: f64| {
        if limit <= 0.0 {
            0.0
        } else {
            ((cost / limit) * 100.0).clamp(0.0, 100.0)
        }
    };
    pct(usage.window_5h, limits.window_5h)
        .max(pct(usage.window_week, limits.window_week))
        .max(pct(usage.window_month, limits.window_month))
}

pub fn account_is_auto_sync_candidate(enabled: bool, setup_ready: bool, key_present: bool) -> bool {
    enabled && setup_ready && key_present
}

pub fn compute_next_after_success(
    now: DateTime<Utc>,
    active: bool,
    earliest_resets_in_minutes: i64,
    jitter01: f64,
) -> DateTime<Utc> {
    let cadence_at = now + cadence_for(active);
    if earliest_resets_in_minutes <= 0 {
        return cadence_at;
    }
    let reset_delay =
        Duration::minutes(earliest_resets_in_minutes) + scale_duration(RESET_JITTER_MAX, jitter01);
    let reset_at = now + reset_delay;
    cadence_at.min(reset_at)
}

pub fn compute_next_after_failure(
    now: DateTime<Utc>,
    failure_streak_after: u32,
    jitter01: f64,
) -> DateTime<Utc> {
    let base = failure_backoff(failure_streak_after);
    // Keep backoff dominant; add a little positive jitter up to 10% of the step.
    let jitter = scale_duration(base / 10, jitter01);
    now + base + jitter
}

pub fn compute_inference_429_delay(now: DateTime<Utc>, jitter01: f64) -> DateTime<Utc> {
    now + duration_between(INFERENCE_429_DELAY_MIN, INFERENCE_429_DELAY_MAX, jitter01)
}

pub fn compute_startup_deferral(
    now: DateTime<Utc>,
    account_id: &str,
    jitter01: f64,
) -> DateTime<Utc> {
    // Mix a stable per-account offset with runtime jitter so restarts spread
    // work without requiring a fetch on boot.
    let stable = deterministic_unit(account_id);
    let mixed = (0.5 * stable + 0.5 * jitter01).clamp(0.0, 1.0);
    now + scale_duration(STARTUP_SPREAD_MAX, mixed)
}

fn deterministic_unit(account_id: &str) -> f64 {
    let mut hash = 0u64;
    for byte in account_id.as_bytes() {
        hash = hash.wrapping_mul(131).wrapping_add(u64::from(*byte));
    }
    (hash % 10_000) as f64 / 10_000.0
}

/// Pull `next_eligible_at` earlier. `None` current means "unset"; the proposal wins.
pub fn pull_next_eligible_earlier(
    current: Option<DateTime<Utc>>,
    proposal: DateTime<Utc>,
) -> DateTime<Utc> {
    match current {
        Some(existing) => existing.min(proposal),
        None => proposal,
    }
}

/// True while a failure backoff floor must not be pulled forward by
/// threshold / cadence / reset logic.
pub fn in_failure_backoff(failure_streak: i64) -> bool {
    failure_streak > 0
}

/// High-usage expedite is allowed only when local max Go usage is high enough
/// and no official call (attempt, success, or prior expedite) happened inside
/// the 15-minute guard. Failure backoff is enforced separately by callers and
/// must never be pulled forward by this check alone.
pub fn should_run_expedited(
    max_percent: f64,
    last_expedited_at: Option<DateTime<Utc>>,
    last_attempt_at: Option<DateTime<Utc>>,
    last_success_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> bool {
    if max_percent < EXPEDITE_THRESHOLD_PERCENT {
        return false;
    }
    let most_recent = [last_expedited_at, last_attempt_at, last_success_at]
        .into_iter()
        .flatten()
        .max();
    match most_recent {
        None => true,
        Some(last) => now >= last + EXPEDITE_GUARD,
    }
}

/// Propose an earlier next-eligible time after inactive→active local traffic.
/// Returns `None` when no pull is warranted.
pub fn active_cadence_pull_proposal(
    last_success_at: Option<DateTime<Utc>>,
    current_next: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let last_success = last_success_at?;
    let active_due = last_success + ACTIVE_CADENCE;
    let proposal = if active_due <= now { now } else { active_due };
    (proposal < current_next).then_some(proposal)
}

/// Schedule a delayed official reconciliation after a real inference 429.
/// Never performs the network call inline and never touches cooldown state.
///
/// Intentionally allowed to pull earlier than a failure-backoff floor: the
/// 1–2 minute post-429 event is an explicit override of cadence/backoff
/// scheduling, tested separately from threshold/cadence pulls.
pub fn schedule_after_inference_429(state: &CoreState, account_id: &str) {
    let now = state.usage_sync.now();
    let jitter = state.usage_sync.jitter01();
    let proposal = compute_inference_429_delay(now, jitter);
    {
        let db = state.db.lock();
        if let Err(error) = db.pull_account_usage_sync_next_eligible(account_id, proposal, false) {
            let _ = db.log_gateway(
                "warn",
                "usage_sync",
                &format!("failed to schedule post-429 usage sync for {account_id}: {error}"),
            );
            return;
        }
    }
    state.usage_sync.wake();
}

/// Start the background reconciler once per `CoreState`. Safe to call
/// repeatedly; the loop is not cancelled by Gateway stop and exits only when
/// the owning `CoreState` is dropped (weak upgrade fails).
pub fn spawn_usage_sync_loop(state: CoreState) {
    if state.usage_sync.loop_started.swap(true, Ordering::AcqRel) {
        return;
    }
    let weak = Arc::downgrade(&state);
    tokio::spawn(async move {
        loop {
            let Some(state) = weak.upgrade() else {
                return;
            };
            if let Err(error) = run_scheduler_once(&state).await {
                let _ = state.db.lock().log_gateway(
                    "warn",
                    "usage_sync",
                    &format!("official usage scheduler tick failed: {error}"),
                );
            }
            // Clone the wake handle, then drop state before awaiting so tests
            // and shutdown can release the SQLite file promptly.
            let wake = state.usage_sync.wake_handle();
            drop(state);
            tokio::select! {
                _ = tokio::time::sleep(SCHEDULER_IDLE_TICK) => {}
                _ = wake.notified() => {}
            }
        }
    });
}

async fn run_scheduler_once(state: &CoreState) -> anyhow::Result<()> {
    let now = state.usage_sync.now();
    let limits = state.pricing_snapshot().limits.clone();
    let candidates = {
        let db = state.db.lock();
        list_auto_candidates(&db, now, &limits)?
    };
    for candidate in candidates {
        match candidate.action {
            CandidateAction::DeferStartup { until } => {
                let db = state.db.lock();
                db.pull_account_usage_sync_next_eligible(&candidate.account_id, until, true)?;
            }
            CandidateAction::Refresh { trigger } => {
                let _ = refresh_official_usage(state, &candidate.account_id, trigger).await;
                tokio::time::sleep(SCHEDULER_PACE).await;
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CandidateAction {
    DeferStartup { until: DateTime<Utc> },
    Refresh { trigger: UsageSyncTrigger },
}

#[derive(Debug, Clone)]
struct SyncCandidate {
    account_id: String,
    action: CandidateAction,
}

fn list_auto_candidates(
    db: &Database,
    now: DateTime<Utc>,
    limits: &PricingLimits,
) -> anyhow::Result<Vec<SyncCandidate>> {
    let accounts = db.list_accounts()?;
    let mut out = Vec::new();
    for account in accounts {
        if !account_is_auto_sync_candidate(
            account.enabled,
            account.setup_step.is_ready(),
            !account.key_cipher.is_empty(),
        ) {
            continue;
        }
        let sync = db.account_usage_sync_state(&account.id)?;
        let next = sync.as_ref().and_then(|s| s.next_eligible_at);
        if next.is_none() {
            let until = compute_startup_deferral(now, &account.id, deterministic_unit(&account.id));
            out.push(SyncCandidate {
                account_id: account.id,
                action: CandidateAction::DeferStartup { until },
            });
            continue;
        }
        let Some(next_at) = next else { continue };
        let sync_state = sync.as_ref();
        let failure_streak = sync_state.map(|s| s.failure_streak).unwrap_or(0);
        let backing_off = in_failure_backoff(failure_streak);

        if next_at > now {
            // Failure backoff floor is never pulled forward by threshold,
            // inactive→active cadence, or reset-style scheduling.
            if backing_off {
                continue;
            }

            let active =
                db.account_has_local_activity_since(&account.id, now - ACTIVITY_LOOKBACK)?;
            if active {
                if let Some(proposal) = active_cadence_pull_proposal(
                    sync_state.and_then(|s| s.last_success_at),
                    next_at,
                    now,
                ) {
                    db.pull_account_usage_sync_next_eligible(&account.id, proposal, true)?;
                    if proposal <= now {
                        out.push(SyncCandidate {
                            account_id: account.id.clone(),
                            action: CandidateAction::Refresh {
                                trigger: UsageSyncTrigger::Scheduled,
                            },
                        });
                        continue;
                    }
                }
            }

            let usage = db.account_usage_with_limits(&account.id, limits)?;
            let max_pct = max_go_usage_percent(&usage, limits);
            if should_run_expedited(
                max_pct,
                sync_state.and_then(|s| s.last_expedited_at),
                sync_state.and_then(|s| s.last_attempt_at),
                sync_state.and_then(|s| s.last_success_at),
                now,
            ) {
                let proposal = now;
                db.pull_account_usage_sync_next_eligible(&account.id, proposal, true)?;
                out.push(SyncCandidate {
                    account_id: account.id,
                    action: CandidateAction::Refresh {
                        trigger: UsageSyncTrigger::Expedited,
                    },
                });
            }
            continue;
        }

        let usage = db.account_usage_with_limits(&account.id, limits)?;
        let max_pct = max_go_usage_percent(&usage, limits);
        let trigger = if !backing_off
            && should_run_expedited(
                max_pct,
                sync_state.and_then(|s| s.last_expedited_at),
                sync_state.and_then(|s| s.last_attempt_at),
                sync_state.and_then(|s| s.last_success_at),
                now,
            ) {
            UsageSyncTrigger::Expedited
        } else {
            UsageSyncTrigger::Scheduled
        };
        out.push(SyncCandidate {
            account_id: account.id,
            action: CandidateAction::Refresh { trigger },
        });
    }
    Ok(out)
}

/// Remove an in-flight map entry only when it is still the same generation the
/// waiter observed. Stale waiters must not delete a newer F2 entry.
fn take_inflight_if_generation(
    map: &mut HashMap<String, InflightEntry>,
    account_id: &str,
    generation: u64,
) -> bool {
    match map.get(account_id) {
        Some(entry) if entry.generation == generation => {
            map.remove(account_id);
            true
        }
        _ => false,
    }
}

/// Shared manual/background entry. Enforces throttle (manual), global
/// concurrency 1, in-flight dedupe, secure fetch, key CAS, and sync metadata.
pub async fn refresh_official_usage(
    state: &CoreState,
    account_id: &str,
    trigger: UsageSyncTrigger,
) -> Result<OfficialUsageRefreshSuccess, OfficialUsageRefreshError> {
    if trigger == UsageSyncTrigger::Manual {
        let now = state.usage_sync.now();
        let sync = state
            .db
            .lock()
            .account_usage_sync_state(account_id)
            .map_err(|e| OfficialUsageRefreshError::Internal(e.to_string()))?;
        if let Some(until) = manual_next_allowed_at(sync.and_then(|s| s.last_attempt_at), now) {
            let retry_after_secs = (until - now).num_seconds().max(1) as u64;
            return Err(OfficialUsageRefreshError::Throttled {
                next_allowed_at: until,
                retry_after_secs,
            });
        }
    }

    let (future, generation) = {
        let mut inflight = state.usage_sync.inflight.lock().await;
        if let Some(existing) = inflight.get(account_id) {
            (existing.future.clone(), existing.generation)
        } else {
            let account_id_owned = account_id.to_string();
            let state_cloned = state.clone();
            let generation = state
                .usage_sync
                .inflight_generation
                .fetch_add(1, Ordering::Relaxed);
            let shared = async move {
                let result =
                    execute_official_usage_refresh(&state_cloned, &account_id_owned, trigger).await;
                Arc::new(result)
            }
            .boxed()
            .shared();
            inflight.insert(
                account_id.to_string(),
                InflightEntry {
                    generation,
                    future: shared.clone(),
                },
            );
            (shared, generation)
        }
    };

    let result = future.await;
    let cleanup_hook = state.usage_sync.before_inflight_cleanup.lock().clone();
    if let Some(hook) = cleanup_hook {
        hook().await;
    }
    {
        let mut inflight = state.usage_sync.inflight.lock().await;
        take_inflight_if_generation(&mut inflight, account_id, generation);
    }

    match &*result {
        Ok(success) => Ok(success.clone()),
        Err(error) => Err(error.clone()),
    }
}

async fn execute_official_usage_refresh(
    state: &CoreState,
    account_id: &str,
    trigger: UsageSyncTrigger,
) -> Result<OfficialUsageRefreshSuccess, OfficialUsageRefreshError> {
    let _guard = state.usage_sync.global.lock().await;
    let now = state.usage_sync.now();
    let limits = state.pricing_snapshot().limits.clone();
    let config = state.config();

    // Policy exclusions: do not begin an attempt / do not write backoff.
    let account = {
        let db = state.db.lock();
        match db.get_account(account_id) {
            Ok(Some(account)) => account,
            Ok(None) => return Err(OfficialUsageRefreshError::NotFound),
            Err(error) => {
                drop(db);
                // DB read failed after scheduler selected the account: treat as
                // a begun attempt so the due stamp cannot busy-loop.
                record_attempt_failure(state, account_id, now);
                return Err(OfficialUsageRefreshError::Internal(error.to_string()));
            }
        }
    };
    if !account.setup_step.is_ready() || account.key_cipher.is_empty() {
        return Err(OfficialUsageRefreshError::NotEligible(
            "only ready accounts with a stored key can refresh official Go usage",
        ));
    }
    if trigger != UsageSyncTrigger::Manual && !account.enabled {
        return Err(OfficialUsageRefreshError::NotEligible(
            "disabled accounts are not auto-synced",
        ));
    }
    let key_cipher = account.key_cipher.clone();
    let plaintext = match state.decrypt_key(&key_cipher) {
        Ok(key) => key,
        Err(error) => {
            record_attempt_failure(state, account_id, now);
            return Err(OfficialUsageRefreshError::Internal(error.to_string()));
        }
    };

    let snapshot = {
        let fetch = state.usage_sync.fetch.lock().clone();
        let result = if let Some(fetch) = fetch {
            fetch(config.clone(), plaintext.clone()).await
        } else {
            crate::go_usage::fetch_go_usage(&config, &plaintext).await
        };
        drop(plaintext);
        result
    };

    let snapshot = match snapshot {
        Ok(snapshot) => snapshot,
        Err(error) => {
            record_attempt_failure(state, account_id, now);
            return Err(OfficialUsageRefreshError::Upstream(error));
        }
    };

    let active = {
        let db = state.db.lock();
        match db.account_has_local_activity_since(account_id, now - ACTIVITY_LOOKBACK) {
            Ok(active) => active,
            Err(error) => {
                drop(db);
                record_attempt_failure(state, account_id, now);
                return Err(OfficialUsageRefreshError::Internal(error.to_string()));
            }
        }
    };
    let jitter = state.usage_sync.jitter01();
    let next_eligible =
        compute_next_after_success(now, active, snapshot.earliest_resets_in_minutes, jitter);
    let next_allowed = now + MANUAL_THROTTLE;
    let usage = {
        let db = state.db.lock();
        let committed = db.commit_official_usage_sync_success(
            account_id,
            &key_cipher,
            &AccountUsageCalibrationSnapshot {
                rolling_percent: snapshot.rolling_percent,
                weekly_percent: snapshot.weekly_percent,
                monthly_percent: snapshot.monthly_percent,
                rolling_resets_in_minutes: snapshot.rolling_resets_in_minutes,
                weekly_resets_in_minutes: snapshot.weekly_resets_in_minutes,
            },
            &limits,
            AccountUsageSyncSuccessMetadata {
                now,
                next_eligible_at: next_eligible,
                mark_expedited: trigger == UsageSyncTrigger::Expedited,
            },
        );
        match committed {
            Ok(Some(usage)) => usage,
            Ok(None) => {
                drop(db);
                record_attempt_failure(state, account_id, now);
                return Err(OfficialUsageRefreshError::Conflict(
                    "account key or setup changed while refreshing official Go usage",
                ));
            }
            Err(error) => {
                drop(db);
                record_attempt_failure(state, account_id, now);
                return Err(OfficialUsageRefreshError::Internal(error.to_string()));
            }
        }
    };

    Ok(OfficialUsageRefreshSuccess {
        usage,
        source: "official_go_usage",
        last_success_at: now.to_rfc3339(),
        next_allowed_at: next_allowed.to_rfc3339(),
    })
}

/// Record a safe retry/backoff outcome for any begun attempt that did not
/// succeed. Never logs keys, ciphertext, or upstream bodies. If persistence
/// itself fails, emit only a sanitized scheduler diagnostic.
fn record_attempt_failure(state: &CoreState, account_id: &str, now: DateTime<Utc>) {
    let jitter = state.usage_sync.jitter01();
    let db = state.db.lock();
    let current = db.account_usage_sync_state(account_id).ok().flatten();
    let streak = current.as_ref().map(|s| s.failure_streak).unwrap_or(0) + 1;
    let next = compute_next_after_failure(now, streak as u32, jitter);
    if let Err(error) = db.record_account_usage_sync_failure(account_id, now, streak, next) {
        let _ = db.log_gateway(
            "warn",
            "usage_sync",
            &format!("failed to persist usage-sync backoff for {account_id}: {error}"),
        );
    }
}

/// Dashboard helper: map sync metadata onto API fields.
pub fn dashboard_sync_fields(
    sync: Option<&AccountUsageSyncState>,
    now: DateTime<Utc>,
) -> (Option<String>, Option<String>) {
    let last_success = sync.and_then(|s| s.last_success_at.map(|t| t.to_rfc3339()));
    let next_allowed = sync
        .and_then(|s| manual_next_allowed_at(s.last_attempt_at, now))
        .map(|t| t.to_rfc3339());
    (last_success, next_allowed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::{KeyCipher, StaticKeyCipher};
    use crate::db::Database;
    use crate::models::{Account, AccountSetupStep, AccountType, AppConfig};
    use crate::state::CoreStateInner;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    fn fixed(ts: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(ts)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn failure_backoff_ladder_caps_at_six_hours() {
        assert_eq!(failure_backoff(1), Duration::minutes(5));
        assert_eq!(failure_backoff(2), Duration::minutes(15));
        assert_eq!(failure_backoff(3), Duration::hours(1));
        assert_eq!(failure_backoff(4), Duration::hours(6));
        assert_eq!(failure_backoff(99), Duration::hours(6));
    }

    #[test]
    fn active_and_inactive_cadence() {
        assert_eq!(cadence_for(true), ACTIVE_CADENCE);
        assert_eq!(cadence_for(false), INACTIVE_CADENCE);
    }

    #[test]
    fn manual_throttle_exposes_next_allowed() {
        let now = fixed("2026-08-18T12:00:00Z");
        assert_eq!(manual_next_allowed_at(None, now), None);
        assert_eq!(
            manual_next_allowed_at(Some(now - Duration::seconds(61)), now),
            None
        );
        assert_eq!(
            manual_next_allowed_at(Some(now - Duration::seconds(30)), now),
            Some(now + Duration::seconds(30))
        );
    }

    #[test]
    fn success_next_respects_cadence_and_earliest_reset() {
        let now = fixed("2026-08-18T12:00:00Z");
        let next = compute_next_after_success(now, true, 10, 0.0);
        assert_eq!(next, now + Duration::minutes(10));
        // Reset farther than hourly cadence → cadence wins for active accounts.
        let next = compute_next_after_success(now, true, 500, 0.0);
        assert_eq!(next, now + ACTIVE_CADENCE);
        // Reset farther than daily cadence → cadence wins for inactive accounts.
        let next = compute_next_after_success(now, false, 60 * 30, 0.0);
        assert_eq!(next, now + INACTIVE_CADENCE);
        // Reset sooner than daily cadence still schedules around the reset.
        let next = compute_next_after_success(now, false, 500, 0.0);
        assert_eq!(next, now + Duration::minutes(500));
        // A stale official reset must not create a near-immediate polling loop.
        let next = compute_next_after_success(now, true, 0, 1.0);
        assert_eq!(next, now + ACTIVE_CADENCE);
        let next = compute_next_after_success(now, false, -1, 1.0);
        assert_eq!(next, now + INACTIVE_CADENCE);
    }

    #[test]
    fn expedite_guard_is_fifteen_minutes() {
        let now = fixed("2026-08-18T12:00:00Z");
        assert!(!should_run_expedited(79.9, None, None, None, now));
        assert!(should_run_expedited(80.0, None, None, None, now));
        assert!(should_run_expedited(95.0, None, None, None, now));
        assert!(!should_run_expedited(
            95.0,
            Some(now - Duration::minutes(14)),
            None,
            None,
            now
        ));
        assert!(should_run_expedited(
            95.0,
            Some(now - Duration::minutes(15)),
            None,
            None,
            now
        ));
        // Any recent official attempt/success also anchors the 15m guard.
        assert!(!should_run_expedited(
            95.0,
            None,
            Some(now - Duration::minutes(5)),
            None,
            now
        ));
        assert!(!should_run_expedited(
            95.0,
            None,
            None,
            Some(now - Duration::minutes(1)),
            now
        ));
    }

    #[test]
    fn inference_429_delay_stays_within_one_to_two_minutes() {
        let now = fixed("2026-08-18T12:00:00Z");
        assert_eq!(
            compute_inference_429_delay(now, 0.0),
            now + INFERENCE_429_DELAY_MIN
        );
        assert_eq!(
            compute_inference_429_delay(now, 1.0),
            now + INFERENCE_429_DELAY_MAX
        );
    }

    #[test]
    fn auto_sync_excludes_disabled_non_ready_empty_key() {
        assert!(!account_is_auto_sync_candidate(false, true, true));
        assert!(!account_is_auto_sync_candidate(true, false, true));
        assert!(!account_is_auto_sync_candidate(true, true, false));
        assert!(account_is_auto_sync_candidate(true, true, true));
    }

    fn temp_dir(label: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("ocg-usage-sync-{}-{}", label, uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn test_state(label: &str) -> (PathBuf, CoreState) {
        let dir = temp_dir(label);
        let cipher: Arc<dyn KeyCipher + Send + Sync> =
            Arc::new(StaticKeyCipher::new("usage-sync-test"));
        let db = Database::open(dir.clone()).unwrap();
        let state = Arc::new(CoreStateInner::new(db, dir.clone(), cipher).unwrap());
        (dir, state)
    }

    fn ready_account(state: &CoreState, id: &str, key: &str) -> Account {
        Account {
            id: id.to_string(),
            name: id.to_string(),
            username: None,
            password_cipher: None,
            key_cipher: state.encrypt_key(key).unwrap(),
            enabled: true,
            account_type: AccountType::Key,
            setup_step: AccountSetupStep::Ready,
            referral_code: None,
            purchase_date: "2026-08-01".to_string(),
            expires_on: "2026-09-01".to_string(),
            cooldown_until: None,
            cooldown_generic_until: None,
            cooldown_5h_until: None,
            cooldown_week_until: None,
            cooldown_month_until: None,
            cooldown_free_until: None,
            last_error: None,
            auth_error: None,
            notes: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn sample_snapshot() -> GoUsageSnapshot {
        GoUsageSnapshot {
            rolling_status: crate::go_usage::GoUsageWindowStatus::RateLimited,
            weekly_status: crate::go_usage::GoUsageWindowStatus::Ok,
            monthly_status: crate::go_usage::GoUsageWindowStatus::Ok,
            rolling_percent: 50.0,
            weekly_percent: 20.0,
            monthly_percent: 10.0,
            rolling_resets_in_minutes: 180,
            weekly_resets_in_minutes: 1_440,
            earliest_resets_in_minutes: 180,
        }
    }

    #[tokio::test]
    async fn manual_throttle_and_dedupe_share_one_upstream_call() {
        let (dir, state) = test_state("throttle-dedupe");
        let account = ready_account(&state, "acc-1", "sk-acc-1");
        state.db.lock().create_account(&account).unwrap();

        let now = fixed("2026-08-18T12:00:00Z");
        state.usage_sync.set_clock_for_test(move || now);
        state.usage_sync.set_jitter_for_test(|| 0.0);
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_fetch = calls.clone();
        let release = Arc::new(tokio::sync::Notify::new());
        let entered = Arc::new(tokio::sync::Notify::new());
        let release_fetch = release.clone();
        let entered_fetch = entered.clone();
        state.usage_sync.set_fetch_for_test(move |_cfg, _key| {
            calls_fetch.fetch_add(1, AtomicOrdering::SeqCst);
            let release_fetch = release_fetch.clone();
            let entered_fetch = entered_fetch.clone();
            let snapshot = sample_snapshot();
            Box::pin(async move {
                entered_fetch.notify_one();
                release_fetch.notified().await;
                Ok(snapshot)
            })
        });

        let state_a = state.clone();
        let state_b = state.clone();
        let a = tokio::spawn(async move {
            refresh_official_usage(&state_a, "acc-1", UsageSyncTrigger::Manual).await
        });
        entered.notified().await;
        let b = tokio::spawn(async move {
            refresh_official_usage(&state_b, "acc-1", UsageSyncTrigger::Manual).await
        });
        // Let the second caller attach to the in-flight future before release.
        tokio::time::sleep(StdDuration::from_millis(20)).await;
        release.notify_waiters();
        let ra = a.await.unwrap().unwrap();
        let rb = b.await.unwrap().unwrap();
        assert_eq!(ra.last_success_at, rb.last_success_at);
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 1);

        let throttled = refresh_official_usage(&state, "acc-1", UsageSyncTrigger::Manual).await;
        match throttled {
            Err(OfficialUsageRefreshError::Throttled {
                retry_after_secs, ..
            }) => assert!(retry_after_secs <= 60),
            other => panic!("expected throttle, got {other:?}"),
        }

        state.usage_sync.clear_test_seams();
        drop(state);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn failure_preserves_last_success_and_calibration() {
        let (dir, state) = test_state("failure-preserve");
        let account = ready_account(&state, "acc-2", "sk-acc-2");
        state.db.lock().create_account(&account).unwrap();
        let now = fixed("2026-08-18T12:00:00Z");
        state.usage_sync.set_clock_for_test(move || now);
        state.usage_sync.set_jitter_for_test(|| 0.0);
        state.usage_sync.set_fetch_for_test(move |_cfg, _key| {
            let snapshot = sample_snapshot();
            Box::pin(async move { Ok(snapshot) })
        });
        refresh_official_usage(&state, "acc-2", UsageSyncTrigger::Manual)
            .await
            .unwrap();
        let before = state
            .db
            .lock()
            .account_usage_with_limits("acc-2", &state.pricing_snapshot().limits)
            .unwrap();
        let success_at = state
            .db
            .lock()
            .account_usage_sync_state("acc-2")
            .unwrap()
            .unwrap()
            .last_success_at;

        // Advance clock beyond manual throttle.
        let later = now + Duration::minutes(2);
        state.usage_sync.set_clock_for_test(move || later);
        state.usage_sync.set_fetch_for_test(move |_cfg, _key| {
            Box::pin(async move { Err(GoUsageError::Timeout) })
        });
        let err = refresh_official_usage(&state, "acc-2", UsageSyncTrigger::Manual)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            OfficialUsageRefreshError::Upstream(GoUsageError::Timeout)
        ));

        let after = state
            .db
            .lock()
            .account_usage_with_limits("acc-2", &state.pricing_snapshot().limits)
            .unwrap();
        assert_eq!(after.window_5h, before.window_5h);
        assert_eq!(after.window_week, before.window_week);
        assert_eq!(after.window_month, before.window_month);
        let sync = state
            .db
            .lock()
            .account_usage_sync_state("acc-2")
            .unwrap()
            .unwrap();
        assert_eq!(sync.last_success_at, success_at);
        assert_eq!(sync.failure_streak, 1);
        assert_eq!(sync.next_eligible_at, Some(later + failure_backoff(1)));

        drop(state);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn key_cas_leaves_windows_unchanged_when_account_changes() {
        let (dir, state) = test_state("cas");
        let account = ready_account(&state, "acc-3", "sk-acc-3");
        state.db.lock().create_account(&account).unwrap();
        let limits = state.pricing_snapshot().limits.clone();
        {
            let db = state.db.lock();
            db.calibrate_account_usage_snapshot(
                "acc-3",
                &AccountUsageCalibrationSnapshot {
                    rolling_percent: 11.0,
                    weekly_percent: 22.0,
                    monthly_percent: 33.0,
                    rolling_resets_in_minutes: 100,
                    weekly_resets_in_minutes: 200,
                },
                &limits,
            )
            .unwrap();
        }
        let before = state
            .db
            .lock()
            .account_usage_with_limits("acc-3", &limits)
            .unwrap();

        let now = fixed("2026-08-18T12:00:00Z");
        state.usage_sync.set_clock_for_test(move || now);
        state.usage_sync.set_jitter_for_test(|| 0.0);
        let state_for_fetch = state.clone();
        state.usage_sync.set_fetch_for_test(move |_cfg, _key| {
            // Swap the key while the network call is "in flight".
            let rotated = state_for_fetch.encrypt_key("sk-rotated").unwrap();
            state_for_fetch
                .db
                .lock()
                .update_account(
                    "acc-3",
                    &crate::models::AccountUpdate {
                        name: None,
                        username: None,
                        password: None,
                        key: Some("sk-rotated".to_string()),
                        enabled: None,
                        referral_code: None,
                        purchase_date: None,
                        notes: None,
                    },
                    Some(&rotated),
                    None,
                )
                .unwrap();
            let snapshot = sample_snapshot();
            Box::pin(async move { Ok(snapshot) })
        });

        let err = refresh_official_usage(&state, "acc-3", UsageSyncTrigger::Manual)
            .await
            .unwrap_err();
        assert!(matches!(err, OfficialUsageRefreshError::Conflict(_)));
        let after = state
            .db
            .lock()
            .account_usage_with_limits("acc-3", &limits)
            .unwrap();
        assert_eq!(after.window_5h, before.window_5h);
        assert_eq!(after.window_week, before.window_week);
        assert_eq!(after.window_month, before.window_month);
        let sync = state
            .db
            .lock()
            .account_usage_sync_state("acc-3")
            .unwrap()
            .unwrap();
        assert_eq!(sync.failure_streak, 1);
        assert_eq!(sync.next_eligible_at, Some(now + failure_backoff(1)));
        assert_eq!(sync.last_attempt_at, Some(now));
        // Manual 60s throttle still exposed after CAS conflict.
        assert_eq!(
            manual_next_allowed_at(sync.last_attempt_at, now),
            Some(now + MANUAL_THROTTLE)
        );

        state.usage_sync.clear_test_seams();
        drop(state);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn official_rate_limited_status_does_not_write_cooldown() {
        let (dir, state) = test_state("official-rate-limited");
        let account = ready_account(&state, "acc-4", "sk-acc-4");
        state.db.lock().create_account(&account).unwrap();
        let now = fixed("2026-08-18T12:00:00Z");
        state.usage_sync.set_clock_for_test(move || now);
        state.usage_sync.set_jitter_for_test(|| 0.0);
        state.usage_sync.set_fetch_for_test(move |_cfg, _key| {
            let snapshot = sample_snapshot();
            Box::pin(async move { Ok(snapshot) })
        });
        refresh_official_usage(&state, "acc-4", UsageSyncTrigger::Scheduled)
            .await
            .unwrap();
        let stored = state.db.lock().get_account("acc-4").unwrap().unwrap();
        assert!(stored.cooldown_until.is_none());
        assert!(stored.cooldown_5h_until.is_none());
        assert!(stored.cooldown_week_until.is_none());
        assert!(stored.cooldown_month_until.is_none());
        assert!(stored.cooldown_generic_until.is_none());
        assert!(stored.cooldown_free_until.is_none());

        drop(state);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn schedule_after_inference_429_only_pulls_next_eligible() {
        let (dir, state) = test_state("infer-429");
        let account = ready_account(&state, "acc-5", "sk-acc-5");
        state.db.lock().create_account(&account).unwrap();
        let now = fixed("2026-08-18T12:00:00Z");
        state.usage_sync.set_clock_for_test(move || now);
        state.usage_sync.set_jitter_for_test(|| 0.0);
        // Far-future cadence baseline.
        state
            .db
            .lock()
            .record_account_usage_sync_success(
                "acc-5",
                now - Duration::hours(1),
                now + Duration::hours(20),
                false,
            )
            .unwrap();
        schedule_after_inference_429(&state, "acc-5");
        let sync = state
            .db
            .lock()
            .account_usage_sync_state("acc-5")
            .unwrap()
            .unwrap();
        assert_eq!(sync.next_eligible_at, Some(now + INFERENCE_429_DELAY_MIN));
        let stored = state.db.lock().get_account("acc-5").unwrap().unwrap();
        assert!(stored.cooldown_until.is_none());

        drop(state);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn sync_metadata_survives_reopen() {
        let dir = temp_dir("persist");
        let cipher: Arc<dyn KeyCipher + Send + Sync> =
            Arc::new(StaticKeyCipher::new("usage-sync-persist"));
        let now = fixed("2026-08-18T12:00:00Z");
        {
            let db = Database::open(dir.clone()).unwrap();
            let state = Arc::new(CoreStateInner::new(db, dir.clone(), cipher.clone()).unwrap());
            let account = ready_account(&state, "persist", "sk-persist");
            state.db.lock().create_account(&account).unwrap();
            state
                .db
                .lock()
                .record_account_usage_sync_success("persist", now, now + ACTIVE_CADENCE, true)
                .unwrap();
            drop(state);
        }
        {
            let db = Database::open(dir.clone()).unwrap();
            let sync = db.account_usage_sync_state("persist").unwrap().unwrap();
            assert_eq!(sync.last_success_at, Some(now));
            assert_eq!(sync.next_eligible_at, Some(now + ACTIVE_CADENCE));
            assert_eq!(sync.failure_streak, 0);
            assert_eq!(sync.last_expedited_at, Some(now));
            // Defaults after migration: missing rows still open.
            assert_eq!(db.schema_version().unwrap(), 22);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn eligibility_lists_active_hourly_vs_inactive_daily_and_exclusions() {
        let dir = temp_dir("eligibility");
        let cipher: Arc<dyn KeyCipher + Send + Sync> =
            Arc::new(StaticKeyCipher::new("usage-sync-elig"));
        let db = Database::open(dir.clone()).unwrap();
        let state = Arc::new(CoreStateInner::new(db, dir.clone(), cipher).unwrap());
        let now = fixed("2026-08-18T12:00:00Z");

        let active = ready_account(&state, "active", "sk-active");
        let inactive = ready_account(&state, "inactive", "sk-inactive");
        let mut disabled = ready_account(&state, "disabled", "sk-disabled");
        disabled.enabled = false;
        let mut pending = ready_account(&state, "pending", "sk-pending");
        pending.setup_step = AccountSetupStep::Payment;
        pending.enabled = false;
        let mut empty = ready_account(&state, "empty", "sk-empty");
        empty.key_cipher.clear();

        {
            let db = state.db.lock();
            db.create_account(&active).unwrap();
            db.create_account(&inactive).unwrap();
            db.create_account(&disabled).unwrap();
            db.create_account(&pending).unwrap();
            db.create_account(&empty).unwrap();
            // Seed due times in the past so both ready accounts are refreshable.
            db.record_account_usage_sync_success(
                "active",
                now - Duration::hours(2),
                now - Duration::minutes(1),
                false,
            )
            .unwrap();
            db.record_account_usage_sync_success(
                "inactive",
                now - Duration::hours(30),
                now - Duration::minutes(1),
                false,
            )
            .unwrap();
            db.log_forward(&crate::models::ForwardLog {
                id: 0,
                timestamp: now - Duration::hours(1),
                model: "mimo-v2.5".into(),
                account_id: "active".into(),
                account_name: "active".into(),
                client_key_id: None,
                client_key_name: None,
                status: "success".into(),
                http_status: Some(200),
                route: String::new(),
                prompt_tokens: 0,
                completion_tokens: 0,
                cached_tokens: 0,
                cache_creation_tokens: 0,
                cost: Some(1.0),
                pricing_revision_id: None,
                quota_multiplier: None,
                local_adjustment_multiplier: None,
                service_tier: None,
                cost_state: "priced".into(),
                error_message: None,
                request_id: None,
                attempt: None,
                error_source: None,
                error_stage: None,
                duration_ms: None,
                diagnostic: None,
            })
            .unwrap();
        }

        let limits = state.pricing_snapshot().limits.clone();
        let candidates = {
            let db = state.db.lock();
            list_auto_candidates(&db, now, &limits).unwrap()
        };
        let ids: Vec<_> = candidates.iter().map(|c| c.account_id.as_str()).collect();
        assert!(ids.contains(&"active"));
        assert!(ids.contains(&"inactive"));
        assert!(!ids.contains(&"disabled"));
        assert!(!ids.contains(&"pending"));
        assert!(!ids.contains(&"empty"));

        let active_next = compute_next_after_success(now, true, 10_000, 0.0);
        let inactive_next = compute_next_after_success(now, false, 10_000, 0.0);
        assert_eq!(active_next, now + ACTIVE_CADENCE);
        assert_eq!(inactive_next, now + INACTIVE_CADENCE);

        drop(state);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unused_app_config_type_keeps_fetch_signature_honest() {
        let _ = AppConfig::default();
    }

    fn seed_high_usage(state: &CoreState, account_id: &str) {
        let limits = state.pricing_snapshot().limits.clone();
        state
            .db
            .lock()
            .calibrate_account_usage_snapshot(
                account_id,
                &AccountUsageCalibrationSnapshot {
                    rolling_percent: 90.0,
                    weekly_percent: 20.0,
                    monthly_percent: 10.0,
                    rolling_resets_in_minutes: 100,
                    weekly_resets_in_minutes: 1_000,
                },
                &limits,
            )
            .unwrap();
    }

    #[tokio::test]
    async fn failed_expedited_sync_stays_in_backoff_across_scheduler_scans() {
        let (dir, state) = test_state("expedite-backoff");
        let account = ready_account(&state, "hi", "sk-hi");
        state.db.lock().create_account(&account).unwrap();
        let now = fixed("2026-08-18T12:00:00Z");
        state.usage_sync.set_clock_for_test(move || now);
        state.usage_sync.set_jitter_for_test(|| 0.0);
        seed_high_usage(&state, "hi");
        state
            .db
            .lock()
            .record_account_usage_sync_success(
                "hi",
                now - Duration::hours(2),
                now + Duration::hours(20),
                false,
            )
            .unwrap();

        state.usage_sync.set_fetch_for_test(move |_cfg, _key| {
            Box::pin(async move { Err(GoUsageError::Timeout) })
        });
        // First scan pulls expedite and fails into 5m backoff.
        let limits = state.pricing_snapshot().limits.clone();
        let candidates = {
            let db = state.db.lock();
            list_auto_candidates(&db, now, &limits).unwrap()
        };
        assert!(candidates.iter().any(|c| {
            c.account_id == "hi"
                && matches!(
                    c.action,
                    CandidateAction::Refresh {
                        trigger: UsageSyncTrigger::Expedited
                    }
                )
        }));
        let _ = refresh_official_usage(&state, "hi", UsageSyncTrigger::Expedited).await;
        let sync = state
            .db
            .lock()
            .account_usage_sync_state("hi")
            .unwrap()
            .unwrap();
        assert_eq!(sync.failure_streak, 1);
        assert_eq!(sync.next_eligible_at, Some(now + failure_backoff(1)));

        // ~30s later the scheduler must not re-select despite still-high usage.
        let soon = now + Duration::seconds(30);
        let candidates = {
            let db = state.db.lock();
            list_auto_candidates(&db, soon, &limits).unwrap()
        };
        assert!(!candidates.iter().any(|c| c.account_id == "hi"));

        // Repeated failure advances the ladder.
        let after_backoff = now + failure_backoff(1);
        state.usage_sync.set_clock_for_test(move || after_backoff);
        let _ = refresh_official_usage(&state, "hi", UsageSyncTrigger::Scheduled).await;
        let sync = state
            .db
            .lock()
            .account_usage_sync_state("hi")
            .unwrap()
            .unwrap();
        assert_eq!(sync.failure_streak, 2);
        assert_eq!(
            sync.next_eligible_at,
            Some(after_backoff + failure_backoff(2))
        );

        state.usage_sync.clear_test_seams();
        drop(state);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn successful_high_usage_retry_is_not_immediately_re_expedited() {
        let (dir, state) = test_state("no-reexpedite");
        let account = ready_account(&state, "hi2", "sk-hi2");
        state.db.lock().create_account(&account).unwrap();
        // Usage-window reads use the production clock, so keep this integration
        // test's injected sync clock aligned instead of crossing a real reset.
        let now = Utc::now();
        state.usage_sync.set_clock_for_test(move || now);
        state.usage_sync.set_jitter_for_test(|| 0.0);
        seed_high_usage(&state, "hi2");
        // Prior success is old; a recent failure left the account due.
        state
            .db
            .lock()
            .record_account_usage_sync_success(
                "hi2",
                now - Duration::hours(2),
                now + Duration::hours(20),
                false,
            )
            .unwrap();
        state
            .db
            .lock()
            .record_account_usage_sync_failure(
                "hi2",
                now - Duration::minutes(6),
                1,
                now - Duration::minutes(1),
            )
            .unwrap();

        let calls = Arc::new(AtomicUsize::new(0));
        let calls_fetch = calls.clone();
        state.usage_sync.set_fetch_for_test(move |_cfg, _key| {
            calls_fetch.fetch_add(1, AtomicOrdering::SeqCst);
            let mut snapshot = sample_snapshot();
            snapshot.rolling_percent = 90.0;
            Box::pin(async move { Ok(snapshot) })
        });
        refresh_official_usage(&state, "hi2", UsageSyncTrigger::Scheduled)
            .await
            .unwrap();
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 1);

        let limits = state.pricing_snapshot().limits.clone();
        let soon = now + Duration::minutes(1);
        let candidates = {
            let db = state.db.lock();
            list_auto_candidates(&db, soon, &limits).unwrap()
        };
        assert!(
            !candidates.iter().any(|c| c.account_id == "hi2"),
            "successful retry at high usage must not be re-expedited inside 15m"
        );

        // After the 15m guard, high usage may expedite even though cadence/reset
        // next_eligible is still in the future (sample earliest reset is 180m).
        let later = now + EXPEDITE_GUARD;
        let candidates = {
            let db = state.db.lock();
            list_auto_candidates(&db, later, &limits).unwrap()
        };
        assert!(candidates.iter().any(|c| {
            c.account_id == "hi2"
                && matches!(
                    c.action,
                    CandidateAction::Refresh {
                        trigger: UsageSyncTrigger::Expedited
                    }
                )
        }));

        state.usage_sync.clear_test_seams();
        drop(state);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn take_inflight_if_generation_ignores_stale_waiters() {
        let mut map = HashMap::new();
        let success = OfficialUsageRefreshSuccess {
            usage: UsageWindow {
                account_id: "a".into(),
                window_5h: 0.0,
                window_week: 0.0,
                window_month: 0.0,
                resets_in_5h: None,
                resets_in_week: None,
                resets_in_month: None,
            },
            source: "official_go_usage",
            last_success_at: fixed("2026-08-18T12:00:00Z").to_rfc3339(),
            next_allowed_at: fixed("2026-08-18T12:01:00Z").to_rfc3339(),
        };
        let finished = async move { Arc::new(Ok(success) as RefreshResult) }
            .boxed()
            .shared();
        map.insert(
            "a".into(),
            InflightEntry {
                generation: 1,
                future: finished.clone(),
            },
        );
        assert!(take_inflight_if_generation(&mut map, "a", 1));
        map.insert(
            "a".into(),
            InflightEntry {
                generation: 2,
                future: finished,
            },
        );
        assert!(!take_inflight_if_generation(&mut map, "a", 1));
        assert_eq!(map.get("a").map(|e| e.generation), Some(2));
        assert!(take_inflight_if_generation(&mut map, "a", 2));
        assert!(map.is_empty());
    }

    #[tokio::test]
    async fn stale_waiter_does_not_drop_newer_inflight_generation() {
        let (dir, state) = test_state("inflight-gen");
        let account = ready_account(&state, "gen", "sk-gen");
        state.db.lock().create_account(&account).unwrap();
        let now = fixed("2026-08-18T12:00:00Z");
        state.usage_sync.set_clock_for_test(move || now);
        state.usage_sync.set_jitter_for_test(|| 0.0);

        let calls = Arc::new(AtomicUsize::new(0));
        let calls_fetch = calls.clone();
        let release_f1 = Arc::new(Notify::new());
        let entered_f1 = Arc::new(Notify::new());
        let release_f2 = Arc::new(Notify::new());
        let entered_f2 = Arc::new(Notify::new());
        let release_f1_fetch = release_f1.clone();
        let entered_f1_fetch = entered_f1.clone();
        let release_f2_fetch = release_f2.clone();
        let entered_f2_fetch = entered_f2.clone();
        state.usage_sync.set_fetch_for_test(move |_cfg, _key| {
            let n = calls_fetch.fetch_add(1, AtomicOrdering::SeqCst);
            let snapshot = sample_snapshot();
            if n == 0 {
                let release = release_f1_fetch.clone();
                let entered = entered_f1_fetch.clone();
                Box::pin(async move {
                    entered.notify_waiters();
                    release.notified().await;
                    Ok(snapshot)
                })
            } else {
                let release = release_f2_fetch.clone();
                let entered = entered_f2_fetch.clone();
                Box::pin(async move {
                    entered.notify_waiters();
                    release.notified().await;
                    Ok(snapshot)
                })
            }
        });

        let ticket = Arc::new(AtomicUsize::new(0));
        let hold_first = Arc::new(Notify::new());
        let hold_first_hook = hold_first.clone();
        let second_entered = Arc::new(Notify::new());
        let second_entered_hook = second_entered.clone();
        state
            .usage_sync
            .set_before_inflight_cleanup_for_test(move || {
                let ticket = ticket.clone();
                let hold_first = hold_first_hook.clone();
                let second_entered = second_entered_hook.clone();
                Box::pin(async move {
                    let which = ticket.fetch_add(1, AtomicOrdering::SeqCst);
                    if which == 0 {
                        hold_first.notified().await;
                    } else {
                        second_entered.notify_one();
                    }
                })
            });

        let state_w1 = state.clone();
        let state_w2 = state.clone();
        let w1 = tokio::spawn(async move {
            refresh_official_usage(&state_w1, "gen", UsageSyncTrigger::Manual).await
        });
        entered_f1.notified().await;
        let w2 = tokio::spawn(async move {
            refresh_official_usage(&state_w2, "gen", UsageSyncTrigger::Manual).await
        });
        tokio::time::sleep(StdDuration::from_millis(20)).await;
        release_f1.notify_waiters();
        // W2 reaches cleanup first (ticket 1) while W1 is held.
        second_entered.notified().await;

        // Start F2 while W1 still holds the stale cleanup.
        let later = now + Duration::minutes(2);
        state.usage_sync.set_clock_for_test(move || later);
        let state_w3 = state.clone();
        let w3 = tokio::spawn(async move {
            refresh_official_usage(&state_w3, "gen", UsageSyncTrigger::Manual).await
        });
        entered_f2.notified().await;
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 2);

        // Stale W1 cleanup must not delete F2.
        hold_first.notify_one();
        let state_w4 = state.clone();
        let w4 = tokio::spawn(async move {
            refresh_official_usage(&state_w4, "gen", UsageSyncTrigger::Manual).await
        });
        tokio::time::sleep(StdDuration::from_millis(20)).await;
        assert_eq!(
            calls.load(AtomicOrdering::SeqCst),
            2,
            "F2 must remain deduped after stale waiter cleanup"
        );
        release_f2.notify_waiters();
        w1.await.unwrap().unwrap();
        w2.await.unwrap().unwrap();
        w3.await.unwrap().unwrap();
        w4.await.unwrap().unwrap();
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 2);

        state.usage_sync.clear_test_seams();
        drop(state);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn decrypt_internal_failure_records_backoff_not_busy_loop() {
        let (dir, state) = test_state("decrypt-fail");
        let mut account = ready_account(&state, "bad", "sk-bad");
        // Store ciphertext the StaticKeyCipher cannot decrypt.
        account.key_cipher = "not-a-valid-cipher".into();
        state.db.lock().create_account(&account).unwrap();
        let now = fixed("2026-08-18T12:00:00Z");
        state.usage_sync.set_clock_for_test(move || now);
        state.usage_sync.set_jitter_for_test(|| 0.0);
        state
            .db
            .lock()
            .record_account_usage_sync_success(
                "bad",
                now - Duration::hours(2),
                now - Duration::minutes(1),
                false,
            )
            .unwrap();

        let err = refresh_official_usage(&state, "bad", UsageSyncTrigger::Scheduled)
            .await
            .unwrap_err();
        assert!(matches!(err, OfficialUsageRefreshError::Internal(_)));
        let sync = state
            .db
            .lock()
            .account_usage_sync_state("bad")
            .unwrap()
            .unwrap();
        assert_eq!(sync.failure_streak, 1);
        assert_eq!(sync.next_eligible_at, Some(now + failure_backoff(1)));
        assert!(sync.last_success_at.is_some());

        let limits = state.pricing_snapshot().limits.clone();
        let soon = now + Duration::seconds(30);
        let candidates = {
            let db = state.db.lock();
            list_auto_candidates(&db, soon, &limits).unwrap()
        };
        assert!(!candidates.iter().any(|c| c.account_id == "bad"));

        state.usage_sync.clear_test_seams();
        drop(state);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn inactive_to_active_pulls_hourly_without_overriding_failure_backoff() {
        let (dir, state) = test_state("inactive-active");
        let account = ready_account(&state, "wake", "sk-wake");
        state.db.lock().create_account(&account).unwrap();
        let now = fixed("2026-08-18T12:00:00Z");
        let last_success = now - Duration::hours(2);
        state
            .db
            .lock()
            .record_account_usage_sync_success(
                "wake",
                last_success,
                now + Duration::hours(20),
                false,
            )
            .unwrap();
        state
            .db
            .lock()
            .log_forward(&crate::models::ForwardLog {
                id: 0,
                timestamp: now - Duration::minutes(10),
                model: "mimo-v2.5".into(),
                account_id: "wake".into(),
                account_name: "wake".into(),
                client_key_id: None,
                client_key_name: None,
                status: "success".into(),
                http_status: Some(200),
                route: String::new(),
                prompt_tokens: 0,
                completion_tokens: 0,
                cached_tokens: 0,
                cache_creation_tokens: 0,
                cost: Some(1.0),
                pricing_revision_id: None,
                quota_multiplier: None,
                local_adjustment_multiplier: None,
                service_tier: None,
                cost_state: "priced".into(),
                error_message: None,
                request_id: None,
                attempt: None,
                error_source: None,
                error_stage: None,
                duration_ms: None,
                diagnostic: None,
            })
            .unwrap();

        let limits = state.pricing_snapshot().limits.clone();
        let candidates = {
            let db = state.db.lock();
            list_auto_candidates(&db, now, &limits).unwrap()
        };
        assert!(candidates.iter().any(|c| {
            c.account_id == "wake"
                && matches!(
                    c.action,
                    CandidateAction::Refresh {
                        trigger: UsageSyncTrigger::Scheduled
                    }
                )
        }));

        // Same transition must not override an active failure backoff floor.
        state
            .db
            .lock()
            .record_account_usage_sync_failure("wake", now, 1, now + failure_backoff(1))
            .unwrap();
        let candidates = {
            let db = state.db.lock();
            list_auto_candidates(&db, now + Duration::seconds(30), &limits).unwrap()
        };
        assert!(!candidates.iter().any(|c| c.account_id == "wake"));
        let sync = state
            .db
            .lock()
            .account_usage_sync_state("wake")
            .unwrap()
            .unwrap();
        assert_eq!(sync.next_eligible_at, Some(now + failure_backoff(1)));

        drop(state);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn inference_429_intentionally_overrides_failure_backoff_floor() {
        let (dir, state) = test_state("429-override");
        let account = ready_account(&state, "rl", "sk-rl");
        state.db.lock().create_account(&account).unwrap();
        let now = fixed("2026-08-18T12:00:00Z");
        state.usage_sync.set_clock_for_test(move || now);
        state.usage_sync.set_jitter_for_test(|| 0.0);
        state
            .db
            .lock()
            .record_account_usage_sync_failure(
                "rl",
                now - Duration::minutes(1),
                2,
                now + failure_backoff(2),
            )
            .unwrap();
        schedule_after_inference_429(&state, "rl");
        let sync = state
            .db
            .lock()
            .account_usage_sync_state("rl")
            .unwrap()
            .unwrap();
        assert_eq!(sync.failure_streak, 2);
        assert_eq!(
            sync.next_eligible_at,
            Some(now + INFERENCE_429_DELAY_MIN),
            "real inference 429 may intentionally pull earlier than failure backoff"
        );

        state.usage_sync.clear_test_seams();
        drop(state);
        let _ = std::fs::remove_dir_all(dir);
    }
}
