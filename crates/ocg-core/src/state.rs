use crate::crypto::KeyCipher;
use crate::db::Database;
use crate::models::{
    AppConfig, normalize_client_root_url, normalize_opencode_invite_url, normalize_proxy_url,
};
use crate::pricing::{
    PricingEstimate, PricingSnapshot, embedded_seed, ensure_current_adjustment_policy,
    ensure_seed_model_coverage,
};
use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use std::fmt;
use std::path::PathBuf;
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
const CLIENT_ROOT_URL_ENV: &str = "OCG_CLIENT_ROOT_URL";

pub struct GatewayHandle {
    pub port: u16,
    pub shutdown: tokio::sync::oneshot::Sender<()>,
    pub task: tokio::task::JoinHandle<()>,
}

pub type AutoStartSync = fn(bool) -> crate::Result<()>;

pub type DockVisibilitySync = Arc<dyn Fn(bool) -> crate::Result<()> + Send + Sync + 'static>;

pub type DesktopUpdateStarter = Arc<dyn Fn(String) -> crate::Result<()> + Send + Sync + 'static>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DesktopUpdatePhase {
    Idle,
    Checking,
    Downloading,
    Installing,
    Failed,
}

impl DesktopUpdatePhase {
    fn is_busy(self) -> bool {
        matches!(self, Self::Checking | Self::Downloading | Self::Installing)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DesktopUpdateStatus {
    pub phase: DesktopUpdatePhase,
    pub downloaded: u64,
    pub total: Option<u64>,
    pub error: Option<String>,
    pub current_version: String,
    pub install_supported: bool,
}

impl DesktopUpdateStatus {
    fn new() -> Self {
        Self {
            phase: DesktopUpdatePhase::Idle,
            downloaded: 0,
            total: None,
            error: None,
            current_version: env!("CARGO_PKG_VERSION").to_string(),
            install_supported: false,
        }
    }
}

#[derive(Debug)]
pub enum DesktopUpdateStartError {
    Unsupported,
    Busy,
    Starter(anyhow::Error),
}

impl fmt::Display for DesktopUpdateStartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported => f.write_str("desktop update installation is unavailable"),
            Self::Busy => f.write_str("a desktop update is already in progress"),
            Self::Starter(error) => write!(f, "failed to start desktop update: {error}"),
        }
    }
}

impl std::error::Error for DesktopUpdateStartError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Starter(error) => Some(error.as_ref()),
            Self::Unsupported | Self::Busy => None,
        }
    }
}

// Note: Mutex lock ordering is (1) settings_update, (2) db, (3) config,
// (4) http_client, (5) gateway, (6) pricing, (7) routing, (8)
// credential_snapshot. desktop_update_status and the async pricing_refresh
// guard are never held while acquiring another sync lock. The credential
// snapshot write lock is always taken last (after db/config reads) and never
// held while acquiring another lock; auth-path readers take only the
// snapshot read lock. Never acquire in reverse order; always drop one before
// acquiring another where possible. Do not hold the routing lock across DB
// or network I/O.
pub struct CoreStateInner {
    pub db: Mutex<Database>,
    pub config: Mutex<AppConfig>,
    client_root_url_override: Option<String>,
    pub settings_update: Mutex<()>,
    settings_revision: AtomicU64,
    /// Authenticating credentials (value -> id/name) covering the primary
    /// key and enabled sub keys; see `gateway_keys` for the invalidation
    /// model. Written only under `settings_update` (key API) or from
    /// `set_config` (primary refresh).
    pub credential_snapshot: RwLock<crate::gateway_keys::CredentialSnapshot>,
    pub gateway: Mutex<Option<GatewayHandle>>,
    pub dashboard_session_token: Mutex<String>,
    dashboard_local_mode: AtomicBool,
    auto_start_sync: OnceLock<AutoStartSync>,
    dock_visibility_sync: OnceLock<DockVisibilitySync>,
    desktop_update_starter: OnceLock<DesktopUpdateStarter>,
    desktop_update_status: Mutex<DesktopUpdateStatus>,
    pub dashboard_dir: Mutex<Option<PathBuf>>,
    http_client: Mutex<Arc<crate::http_client::ForwardRouteSet>>,
    pricing: RwLock<Arc<PricingSnapshot>>,
    pub pricing_refresh: tokio::sync::Mutex<()>,
    pub routing: crate::gateway::routing::RoutingRuntime,
    pub browser: crate::browser::BrowserRuntime,
    /// Official Go usage sync gates (concurrency, dedupe, clock/jitter seams).
    /// The background loop is started from gateway startup, not construction.
    pub usage_sync: crate::usage_sync::UsageSyncRuntime,
    pub data_dir: PathBuf,
    pub cipher: Arc<dyn KeyCipher + Send + Sync>,
}

pub type CoreState = Arc<CoreStateInner>;

impl CoreStateInner {
    pub fn new(
        db: Database,
        data_dir: PathBuf,
        cipher: Arc<dyn KeyCipher + Send + Sync>,
    ) -> crate::Result<Self> {
        let client_root_url_override = client_root_url_override_from_env()?;
        Self::new_with_client_root_url_override(db, data_dir, cipher, client_root_url_override)
    }

    fn new_with_client_root_url_override(
        db: Database,
        data_dir: PathBuf,
        cipher: Arc<dyn KeyCipher + Send + Sync>,
        client_root_url_override: Option<String>,
    ) -> crate::Result<Self> {
        crate::auth::bootstrap_admin_from_env(&db)?;
        let browser_recovery =
            crate::browser::recover_staged_browser_profiles(&data_dir, |account_id| {
                Ok(db.get_account(account_id)?.is_some())
            });
        if browser_recovery.has_activity() {
            let summary = browser_recovery.summary();
            let level = if browser_recovery.issues.is_empty() {
                "info"
            } else {
                eprintln!("warning: {summary}: {}", browser_recovery.issues.join("; "));
                "warn"
            };
            let _ = db.log_gateway(level, "browser", &summary);
        }
        let (config, needs_persist) = load_config(&db)?;
        config.validate().map_err(anyhow::Error::msg)?;
        if needs_persist {
            // Persist generated defaults and drop fields removed from AppConfig.
            save_config(&db, &config)?;
        }
        let credential_snapshot =
            crate::gateway_keys::build_credential_snapshot(&db, &config.gateway_key)?;
        let pricing = match db.latest_pricing_snapshot()? {
            Some(snapshot) => {
                let previous_revision = snapshot.revision.clone();
                let snapshot = ensure_current_adjustment_policy(snapshot);
                let snapshot = ensure_seed_model_coverage(snapshot);
                if snapshot.revision != previous_revision {
                    db.insert_pricing_snapshot(&snapshot)?;
                }
                snapshot
            }
            None => {
                let snapshot = embedded_seed();
                db.insert_pricing_snapshot(&snapshot)?;
                snapshot
            }
        };
        let http_client = crate::http_client::build_route_set(&config)?;
        Ok(Self {
            db: Mutex::new(db),
            config: Mutex::new(config),
            client_root_url_override,
            settings_update: Mutex::new(()),
            // Use a per-runtime random epoch so a browser tab left open across a
            // process restart cannot accidentally match the new runtime's first
            // revision. The low 48 bits leave ample room for monotonic increments.
            settings_revision: AtomicU64::new(
                (uuid::Uuid::new_v4().as_u128() as u64) & 0x0000_FFFF_FFFF_FFFF,
            ),
            credential_snapshot: RwLock::new(credential_snapshot),
            gateway: Mutex::new(None),
            dashboard_session_token: Mutex::new(uuid::Uuid::new_v4().simple().to_string()),
            dashboard_local_mode: AtomicBool::new(false),
            auto_start_sync: OnceLock::new(),
            dock_visibility_sync: OnceLock::new(),
            desktop_update_starter: OnceLock::new(),
            desktop_update_status: Mutex::new(DesktopUpdateStatus::new()),
            dashboard_dir: Mutex::new(None),
            http_client: Mutex::new(Arc::new(http_client)),
            pricing: RwLock::new(Arc::new(pricing)),
            pricing_refresh: tokio::sync::Mutex::new(()),
            routing: crate::gateway::routing::RoutingRuntime::new(),
            browser: crate::browser::BrowserRuntime::new(),
            usage_sync: crate::usage_sync::UsageSyncRuntime::new(),
            data_dir,
            cipher,
        })
    }

    pub fn config(&self) -> AppConfig {
        self.config.lock().clone()
    }

    pub fn settings_config(&self) -> AppConfig {
        let mut config = self.config();
        if let Some(client_root_url) = &self.client_root_url_override {
            config.client_root_url.clone_from(client_root_url);
        }
        config
    }

    pub fn settings_revision(&self) -> u64 {
        self.settings_revision.load(Ordering::Acquire)
    }

    /// Advances the settings revision for mutations that bypass
    /// `set_config` (the sub key lifecycle API), keeping the shared
    /// optimistic-lock scheme meaningful across every writer.
    pub fn bump_settings_revision(&self) -> u64 {
        self.settings_revision.fetch_add(1, Ordering::AcqRel) + 1
    }

    pub fn client_root_url_from_env(&self) -> bool {
        self.client_root_url_override.is_some()
    }

    pub fn upstream_context(&self) -> (AppConfig, reqwest::Client) {
        let config = self.config.lock();
        let client = self.http_client.lock();
        (config.clone(), client.default_client().clone())
    }

    /// Clones the whole route set as one snapshot: routing metadata and both
    /// leg clients come from the same `set_config` generation, so in-flight
    /// requests fly on internally consistent routing even across hot config
    /// switches.
    pub(crate) fn forward_route_set(&self) -> Arc<crate::http_client::ForwardRouteSet> {
        self.http_client.lock().clone()
    }

    pub fn pricing_snapshot(&self) -> Arc<PricingSnapshot> {
        self.pricing.read().clone()
    }

    pub fn activate_pricing_snapshot(&self, snapshot: PricingSnapshot) -> crate::Result<()> {
        // Keep database persistence and the in-memory active pointer behind the
        // documented db -> pricing lock order, so readers never observe a
        // partially activated revision.
        let db = self.db.lock();
        let mut active = self.pricing.write();
        db.insert_pricing_snapshot(&snapshot)?;
        *active = Arc::new(snapshot);
        Ok(())
    }

    pub fn estimate_cost(
        &self,
        model: &str,
        prompt: i64,
        completion: i64,
        cached: i64,
        cache_creation: i64,
        service_tier: Option<&str>,
    ) -> PricingEstimate {
        self.pricing_snapshot().estimate(
            model,
            prompt,
            completion,
            cached,
            cache_creation,
            service_tier,
        )
    }

    pub fn active_gateway_port(&self) -> u16 {
        let configured = self.config().gateway_port;
        self.gateway
            .lock()
            .as_ref()
            .map(|handle| handle.port)
            .unwrap_or(configured)
    }

    pub fn set_dashboard_local_mode(&self, local: bool) {
        self.dashboard_local_mode.store(local, Ordering::Relaxed);
    }

    pub fn dashboard_local_mode(&self) -> bool {
        self.dashboard_local_mode.load(Ordering::Relaxed)
    }

    pub fn set_auto_start_sync(&self, sync: AutoStartSync) {
        assert!(
            self.auto_start_sync.set(sync).is_ok(),
            "auto-start sync is already configured"
        );
    }

    pub fn auto_start_supported(&self) -> bool {
        self.auto_start_sync.get().is_some()
    }

    pub fn sync_auto_start(&self, enabled: bool) -> crate::Result<()> {
        let sync = self
            .auto_start_sync
            .get()
            .ok_or_else(|| anyhow::anyhow!("auto-start is unavailable in this runtime"))?;
        sync(enabled)
    }

    pub fn set_dock_visibility_sync(&self, sync: DockVisibilitySync) {
        assert!(
            self.dock_visibility_sync.set(sync).is_ok(),
            "dock visibility sync is already configured"
        );
    }

    pub fn dock_visibility_supported(&self) -> bool {
        self.dock_visibility_sync.get().is_some()
    }

    pub fn sync_dock_visibility(&self, visible: bool) -> crate::Result<()> {
        let sync = self
            .dock_visibility_sync
            .get()
            .ok_or_else(|| anyhow::anyhow!("dock visibility is unavailable in this runtime"))?;
        sync(visible)
    }

    pub fn set_desktop_update_starter(&self, starter: DesktopUpdateStarter) {
        assert!(
            self.desktop_update_starter.set(starter).is_ok(),
            "desktop update starter is already configured"
        );
        self.desktop_update_status.lock().install_supported = true;
    }

    pub fn desktop_update_supported(&self) -> bool {
        self.desktop_update_starter.get().is_some()
    }

    pub fn desktop_update_status(&self) -> DesktopUpdateStatus {
        self.desktop_update_status.lock().clone()
    }

    pub fn start_desktop_update(
        &self,
        expected_version: String,
    ) -> Result<(), DesktopUpdateStartError> {
        let starter = self
            .desktop_update_starter
            .get()
            .cloned()
            .ok_or(DesktopUpdateStartError::Unsupported)?;
        {
            let mut status = self.desktop_update_status.lock();
            if status.phase.is_busy() {
                return Err(DesktopUpdateStartError::Busy);
            }
            status.phase = DesktopUpdatePhase::Checking;
            status.downloaded = 0;
            status.total = None;
            status.error = None;
            status.install_supported = true;
        }

        if let Err(error) = starter(expected_version) {
            self.set_desktop_update_failed(error.to_string());
            return Err(DesktopUpdateStartError::Starter(error));
        }
        Ok(())
    }

    pub fn set_desktop_update_progress(&self, downloaded: u64, total: Option<u64>) -> bool {
        let mut status = self.desktop_update_status.lock();
        if !matches!(
            status.phase,
            DesktopUpdatePhase::Checking | DesktopUpdatePhase::Downloading
        ) {
            return false;
        }
        status.phase = DesktopUpdatePhase::Downloading;
        status.downloaded = downloaded;
        status.total = total;
        status.error = None;
        true
    }

    pub fn set_desktop_update_installing(&self) -> bool {
        let mut status = self.desktop_update_status.lock();
        if !matches!(
            status.phase,
            DesktopUpdatePhase::Checking | DesktopUpdatePhase::Downloading
        ) {
            return false;
        }
        status.phase = DesktopUpdatePhase::Installing;
        status.error = None;
        true
    }

    pub fn set_desktop_update_failed(&self, error: impl Into<String>) {
        let mut status = self.desktop_update_status.lock();
        status.phase = DesktopUpdatePhase::Failed;
        status.error = Some(error.into());
    }

    pub fn set_desktop_update_idle(&self) {
        let mut status = self.desktop_update_status.lock();
        status.phase = DesktopUpdatePhase::Idle;
        status.downloaded = 0;
        status.total = None;
        status.error = None;
    }

    pub fn set_config(&self, mut config: AppConfig) -> crate::Result<()> {
        if self.client_root_url_override.is_some() {
            config.client_root_url = self.config.lock().client_root_url.clone();
        }
        config.claude_desktop_models.normalize();
        config.opencode_invite_url = normalize_opencode_invite_url(&config.opencode_invite_url)
            .map_err(anyhow::Error::msg)?;
        config.proxy_url = normalize_proxy_url(config.proxy_mode, &config.proxy_url)
            .map_err(anyhow::Error::msg)?;
        // validate() enforces the non-blank primary key on every write path.
        config.validate().map_err(anyhow::Error::msg)?;
        let http_client = crate::http_client::build_route_set(&config)?;
        {
            let db = self.db.lock();
            save_config(&db, &config)?;
        }
        let should_reset_routing = {
            let mut current_config = self.config.lock();
            let mut current_client = self.http_client.lock();
            // Sticky routing resets when the routing fields change or the
            // primary key value rotates (its previous value stops
            // authenticating). Sub key revocations reset explicitly from
            // their endpoints; renaming or adding keys never clears live
            // sessions.
            let should_reset = current_config.routing_mode != config.routing_mode
                || current_config.conversation_sticky != config.conversation_sticky
                || current_config.gateway_key != config.gateway_key;
            *current_config = config.clone();
            *current_client = Arc::new(http_client);
            should_reset
        };
        // Refresh the primary entry in the credential snapshot so auth stops
        // accepting the old value immediately. Cross-tier uniqueness (API
        // gates) guarantees no other snapshot entry holds the new value; the
        // warn-only check below is defense in depth for future unchecked
        // writers.
        //
        // Ordering note: persistence precedes the snapshot swap on purpose.
        // A failed save returns before any mutation (consistent state); the
        // only gap is a panic between the in-memory swap above and this
        // snapshot block, transiently leaving the database and in-memory
        // config on the new value while the snapshot still authenticates the
        // old one — it heals on restart or at the next key API entry point
        // (both rebuild the snapshot from the database). Swapping the
        // snapshot first would instead leave an unpersisted credential
        // authenticating after a failed save — a divergence that outlives
        // the process.
        {
            let mut snapshot = self.credential_snapshot.write();
            if let Some(existing) = snapshot.get(&config.gateway_key) {
                if existing.id != crate::gateway_keys::PRIMARY_KEY_ID {
                    eprintln!(
                        "warning: primary key value collides with sub key `{}`; \
                         the API-layer gate should have rejected this write",
                        existing.name
                    );
                }
            }
            let stale_value = snapshot
                .iter()
                .find(|(_, entry)| entry.id == crate::gateway_keys::PRIMARY_KEY_ID)
                .map(|(value, _)| value.clone());
            if let Some(value) = stale_value {
                snapshot.remove(&value);
            }
            snapshot.insert(
                config.gateway_key.clone(),
                crate::gateway_keys::CredentialEntry {
                    id: crate::gateway_keys::PRIMARY_KEY_ID.to_string(),
                    name: crate::gateway_keys::PRIMARY_KEY_NAME.to_string(),
                },
            );
        }
        self.settings_revision.fetch_add(1, Ordering::AcqRel);
        if should_reset_routing {
            self.routing.reset();
        }
        Ok(())
    }

    /// Resolves an authenticating credential by presented value; used by the
    /// auth hot path without touching the config or db locks.
    pub fn credential_entry_for_value(
        &self,
        value: &str,
    ) -> Option<crate::gateway_keys::CredentialEntry> {
        self.credential_snapshot.read().get(value).cloned()
    }

    /// Write-time name snapshot for a credential id (primary resolves to the
    /// fixed "Primary"); serves forward log attribution without a db lookup.
    pub fn client_key_name(&self, id: &str) -> Option<String> {
        self.credential_snapshot
            .read()
            .values()
            .find(|entry| entry.id == id)
            .map(|entry| entry.name.clone())
    }

    pub fn data_dir(&self) -> PathBuf {
        self.data_dir.clone()
    }

    pub fn recover_browser_profiles_for_account(
        &self,
        account_id: &str,
    ) -> crate::Result<crate::browser::BrowserProfileRecoveryReport> {
        let db = self.db.lock();
        let account_exists = db.get_account(account_id)?.is_some();
        let report = crate::browser::recover_staged_browser_profiles_for_account(
            &self.data_dir,
            account_id,
            account_exists,
        );
        drop(db);
        report
    }

    pub fn set_dashboard_dir(&self, dir: Option<PathBuf>) {
        *self.dashboard_dir.lock() = dir;
    }

    pub fn dashboard_dir(&self) -> Option<PathBuf> {
        self.dashboard_dir.lock().clone()
    }

    pub fn encrypt_key(&self, plaintext: &str) -> crate::Result<String> {
        self.cipher.encrypt(plaintext)
    }

    pub fn decrypt_key(&self, ciphertext: &str) -> crate::Result<String> {
        self.cipher.decrypt(ciphertext)
    }
}

fn client_root_url_override_from_env() -> crate::Result<Option<String>> {
    match std::env::var(CLIENT_ROOT_URL_ENV) {
        Ok(value) => normalize_client_root_url_override(Some(&value))
            .map_err(|error| anyhow::anyhow!("{CLIENT_ROOT_URL_ENV}: {error}")),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(anyhow::anyhow!(
            "{CLIENT_ROOT_URL_ENV} must contain valid Unicode"
        )),
    }
}

fn normalize_client_root_url_override(value: Option<&str>) -> Result<Option<String>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = normalize_client_root_url(value)?;
    Ok((!value.is_empty()).then_some(value))
}

/// Loads persisted config. The `bool` marks config that needs canonical rewriting.
fn load_config(db: &Database) -> crate::Result<(AppConfig, bool)> {
    let mut config = AppConfig::default();
    let mut needs_persist = false;
    if let Some(value) = db.get_setting("config")? {
        config = serde_json::from_str(&value)?;
        config.claude_desktop_models.normalize();
        needs_persist = serde_json::to_string(&config)? != value;
    }
    let invite_url =
        normalize_opencode_invite_url(&config.opencode_invite_url).map_err(anyhow::Error::msg)?;
    if invite_url != config.opencode_invite_url {
        config.opencode_invite_url = invite_url;
        needs_persist = true;
    }
    let proxy_url =
        normalize_proxy_url(config.proxy_mode, &config.proxy_url).map_err(anyhow::Error::msg)?;
    if proxy_url != config.proxy_url {
        config.proxy_url = proxy_url;
        needs_persist = true;
    }
    // v1.4.2 shipped 30/120/300 as one default tuple. Migrate that exact,
    // untouched tuple once while preserving every user-customized combination.
    if (
        config.connect_timeout_secs,
        config.non_stream_timeout_secs,
        config.stream_idle_timeout_secs,
    ) == (30, 120, 300)
    {
        config.non_stream_timeout_secs = 900;
        needs_persist = true;
    }
    if config.gateway_key.trim().is_empty() {
        // Mint before validate: a fresh, pre-multi-key, or whitespace-corrupt
        // config always ends up with a usable primary key (validate rejects
        // blank-after-trim values, so the guard here must be trim-aware).
        config.gateway_key = generate_gateway_key();
        needs_persist = true;
    }
    Ok((config, needs_persist))
}

fn save_config(db: &Database, config: &AppConfig) -> crate::Result<()> {
    db.set_setting("config", &serde_json::to_string(config)?)?;
    Ok(())
}

fn generate_gateway_key() -> String {
    format!("ocg-{}-{}", random_word(), random_word())
}

pub fn random_word() -> String {
    // Use UUID v4 for proper randomness (122 bits entropy)
    uuid::Uuid::new_v4().simple().to_string()[..8].to_string()
}

#[cfg(test)]
mod tests {
    use super::{
        CoreStateInner, DesktopUpdatePhase, DesktopUpdateStartError,
        normalize_client_root_url_override,
    };
    use crate::crypto::{KeyCipher, StaticKeyCipher};
    use crate::db::Database;
    use crate::models::{AppConfig, ProxyMode};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::{Arc, Barrier, Mutex as StdMutex};

    fn temp_data_dir(label: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock should be after the Unix epoch")
            .as_nanos();
        dir.push(format!("ocg-state-test-{label}-{nanos}"));
        fs::create_dir_all(&dir).expect("test data directory should be created");
        dir
    }

    #[test]
    fn client_root_url_override_normalizes_non_empty_values() {
        assert_eq!(normalize_client_root_url_override(None), Ok(None));
        assert_eq!(normalize_client_root_url_override(Some("   ")), Ok(None));
        assert_eq!(
            normalize_client_root_url_override(Some(" https://ocg.example.com/proxy/v1/ ")),
            Ok(Some("https://ocg.example.com/proxy".to_string()))
        );
        assert!(
            normalize_client_root_url_override(Some("https://ocg.example.com/v1/responses"))
                .is_err()
        );
    }

    #[test]
    fn client_root_url_override_never_replaces_persisted_setting() {
        let dir = temp_data_dir("client-root-override");
        let db = Database::open(dir.clone()).expect("test database should open");
        let persisted = AppConfig {
            gateway_key: "test-gateway-key".to_string(),
            client_root_url: "https://saved.example.com".to_string(),
            ..AppConfig::default()
        };
        db.set_setting(
            "config",
            &serde_json::to_string(&persisted).expect("test config should serialize"),
        )
        .expect("test config should persist");
        let cipher: Arc<dyn KeyCipher + Send + Sync> = Arc::new(StaticKeyCipher::new("state-test"));
        let state = CoreStateInner::new_with_client_root_url_override(
            db,
            dir.clone(),
            cipher,
            Some("https://environment.example.com".to_string()),
        )
        .expect("state should initialize");

        assert_eq!(
            state.settings_config().client_root_url,
            "https://environment.example.com"
        );
        let mut submitted = state.settings_config();
        submitted.connect_timeout_secs = 45;
        state
            .set_config(submitted)
            .expect("other settings should save while the override is active");
        assert_eq!(state.config().client_root_url, "https://saved.example.com");
        let stored = state
            .db
            .lock()
            .get_setting("config")
            .expect("stored config should be readable")
            .expect("stored config should exist");
        let stored: AppConfig =
            serde_json::from_str(&stored).expect("stored config should deserialize");
        assert_eq!(stored.client_root_url, "https://saved.example.com");
        assert_eq!(stored.connect_timeout_secs, 45);

        drop(state);
        fs::remove_dir_all(dir).expect("test data directory should be removed");
    }

    #[test]
    fn manual_proxy_config_is_normalized_and_persisted() {
        let dir = temp_data_dir("manual-proxy");
        let db = Database::open(dir.clone()).expect("test database should open");
        let cipher: Arc<dyn KeyCipher + Send + Sync> = Arc::new(StaticKeyCipher::new("state-test"));
        let state = CoreStateInner::new(db, dir.clone(), cipher).expect("state should initialize");
        let mut config = state.config();
        config.proxy_mode = ProxyMode::Manual;
        config.proxy_url = " http://127.0.0.1:7890/ ".to_string();

        state
            .set_config(config)
            .expect("manual proxy configuration should save");
        assert_eq!(state.config().proxy_mode, ProxyMode::Manual);
        assert_eq!(state.config().proxy_url, "http://127.0.0.1:7890");

        let stored = state
            .db
            .lock()
            .get_setting("config")
            .unwrap()
            .expect("config should be stored");
        let stored: AppConfig = serde_json::from_str(&stored).unwrap();
        assert_eq!(stored.proxy_mode, ProxyMode::Manual);
        assert_eq!(stored.proxy_url, "http://127.0.0.1:7890");

        drop(state);
        fs::remove_dir_all(dir).expect("test data directory should be removed");
    }

    #[test]
    fn auto_proxy_saves_leftover_invalid_url_without_using_it() {
        let dir = temp_data_dir("auto-proxy-leftover");
        let db = Database::open(dir.clone()).expect("test database should open");
        let cipher: Arc<dyn KeyCipher + Send + Sync> = Arc::new(StaticKeyCipher::new("state-test"));
        let state = CoreStateInner::new(db, dir.clone(), cipher).expect("state should initialize");
        let mut config = state.config();
        config.proxy_mode = ProxyMode::Auto;
        config.proxy_url = "not-a-proxy".to_string();

        state
            .set_config(config)
            .expect("auto mode should ignore leftover invalid proxy URLs");
        assert_eq!(state.config().proxy_mode, ProxyMode::Auto);
        assert_eq!(state.config().proxy_url, "not-a-proxy");

        drop(state);
        fs::remove_dir_all(dir).expect("test data directory should be removed");
    }

    #[test]
    fn startup_retries_committed_browser_profile_cleanup() {
        let dir = temp_data_dir("browser-profile-recovery");
        let db = Database::open(dir.clone()).expect("test database should open");
        let profile_root = dir.join("profiles");
        fs::create_dir_all(&profile_root).expect("legacy profile root should be created");
        let tombstone = profile_root.join(format!(
            ".ocg-profile-delete-deleted-account-{}",
            uuid::Uuid::new_v4().simple()
        ));
        fs::create_dir(&tombstone).expect("profile tombstone should be created");
        fs::write(tombstone.join("Cookies"), b"sensitive").expect("profile data should be created");
        let cipher: Arc<dyn KeyCipher + Send + Sync> = Arc::new(StaticKeyCipher::new("state-test"));

        let state = CoreStateInner::new(db, dir.clone(), cipher).expect("state should initialize");

        assert!(!tombstone.exists());
        drop(state);
        fs::remove_dir_all(dir).expect("test data directory should be removed");
    }

    #[test]
    fn startup_finishes_reset_profile_journal_without_restoring_cookies() {
        use crate::browser::{BrowserProfileOperationKind, StagedBrowserProfiles};
        use crate::models::{Account, AccountSetupStep, AccountType};

        let dir = temp_data_dir("browser-profile-reset-journal");
        let db = Database::open(dir.clone()).expect("test database should open");
        let cipher: Arc<dyn KeyCipher + Send + Sync> = Arc::new(StaticKeyCipher::new("state-test"));
        let now = chrono::Utc::now();
        let account = Account {
            id: "existing-account".into(),
            name: "existing-account".into(),
            username: None,
            password_cipher: None,
            key_cipher: cipher.encrypt("opaque-key").unwrap(),
            enabled: true,
            account_type: AccountType::Key,
            setup_step: AccountSetupStep::Ready,
            referral_code: None,
            purchase_date: String::new(),
            expires_on: String::new(),
            cooldown_until: None,
            cooldown_generic_until: None,
            cooldown_5h_until: None,
            cooldown_week_until: None,
            cooldown_month_until: None,
            cooldown_free_until: None,
            last_error: None,
            auth_error: None,
            notes: None,
            created_at: now,
            updated_at: now,
        };
        db.create_account(&account)
            .expect("test account should be created");
        let profile = dir.join("browser-profiles").join(&account.id);
        fs::create_dir_all(&profile).expect("browser profile should be created");
        fs::write(profile.join("Cookies"), b"old-cookie")
            .expect("browser cookie should be created");
        let staged = StagedBrowserProfiles::stage(
            &dir,
            &account.id,
            BrowserProfileOperationKind::ResetProfile,
        )
        .expect("profile reset should be journaled and staged");
        assert!(!profile.exists());
        drop(staged); // simulate a crash before the normal purge step

        let state = CoreStateInner::new(db, dir.clone(), cipher).expect("state should initialize");

        assert!(state.db.lock().get_account(&account.id).unwrap().is_some());
        assert!(!profile.exists(), "reset recovery must not restore cookies");
        assert_eq!(
            fs::read_dir(dir.join("browser-profile-operations"))
                .unwrap()
                .count(),
            0,
            "completed recovery should remove its journal"
        );
        drop(state);
        fs::remove_dir_all(dir).expect("test data directory should be removed");
    }

    #[test]
    fn route_set_snapshot_swaps_atomically_and_stays_self_consistent() {
        use crate::crypto::{KeyCipher, StaticKeyCipher};

        let dir = temp_data_dir("route-set-swap");
        let db = Database::open(dir.clone()).expect("test database should open");
        let cipher: Arc<dyn KeyCipher + Send + Sync> = Arc::new(StaticKeyCipher::new("state-test"));
        let state = CoreStateInner::new(db, dir.clone(), cipher).expect("state should initialize");

        let entry_snapshot = state.forward_route_set();
        assert!(
            std::ptr::eq(
                entry_snapshot.client_for("gpt-5.6-luna").0,
                state.forward_route_set().default_client()
            ),
            "non-list generations resolve to the single process-wide client"
        );

        let mut list_config = state.config();
        list_config.gateway_key = "gw".into();
        list_config.proxy_mode = crate::models::ProxyMode::List;
        list_config.proxy_url = "http://127.0.0.1:7890".into();
        list_config.proxy_list_direction = crate::models::ProxyListDirection::Whitelist;
        list_config.proxy_list_models = vec!["gpt-5.6-luna".to_string()];
        state
            .set_config(list_config)
            .expect("list config should save");

        // The active set was replaced wholesale with the new generation.
        let next_snapshot = state.forward_route_set();
        assert_eq!(next_snapshot.client_for("gpt-5.6-luna").1.as_str(), "proxy");
        assert_eq!(next_snapshot.client_for("glm-5.3").1.as_str(), "direct");

        // The in-flight snapshot keeps its own consistent routing: same model,
        // same old label, even though the config generation moved on.
        assert_eq!(entry_snapshot.client_for("gpt-5.6-luna").1.as_str(), "auto");

        drop(state);
        fs::remove_dir_all(dir).expect("test data directory should be removed");
    }

    #[test]
    fn routing_runtime_resets_when_routing_fields_or_gateway_key_change() {
        use crate::crypto::{KeyCipher, StaticKeyCipher};
        use crate::models::{Account, RoutingMode};
        use std::sync::Arc;

        fn test_account(cipher: &Arc<dyn KeyCipher + Send + Sync>, id: &str) -> Account {
            Account {
                id: id.into(),
                name: id.into(),
                username: None,
                password_cipher: None,
                key_cipher: cipher.encrypt(id).unwrap(),
                enabled: true,
                account_type: crate::models::AccountType::Key,
                setup_step: crate::models::AccountSetupStep::Ready,
                referral_code: None,
                purchase_date: String::new(),
                expires_on: String::new(),
                cooldown_until: None,
                cooldown_generic_until: None,
                cooldown_5h_until: None,
                cooldown_week_until: None,
                cooldown_month_until: None,
                cooldown_free_until: None,
                last_error: None,
                auth_error: None,
                notes: None,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            }
        }

        let dir = temp_data_dir("routing-reset");
        let db = Database::open(dir.clone()).expect("test database should open");
        let cipher: Arc<dyn KeyCipher + Send + Sync> = Arc::new(StaticKeyCipher::new("state-test"));
        let state =
            CoreStateInner::new(db, dir.clone(), cipher.clone()).expect("state should initialize");
        let accounts = vec![test_account(&cipher, "a"), test_account(&cipher, "b")];

        assert_eq!(
            state
                .routing
                .select_account(&accounts, RoutingMode::RoundRobin, false, None, &[])
                .unwrap()
                .id,
            "a"
        );
        assert_eq!(
            state
                .routing
                .select_account(&accounts, RoutingMode::RoundRobin, false, None, &[])
                .unwrap()
                .id,
            "b"
        );

        let mut invalid = state.config();
        invalid.routing_mode = RoutingMode::StickyGlobal;
        invalid.connect_timeout_secs = 0;
        assert!(state.set_config(invalid).is_err());
        assert_eq!(
            state
                .routing
                .select_account(&accounts, RoutingMode::RoundRobin, false, None, &[])
                .unwrap()
                .id,
            "a",
            "failed config validation must not reset the active round-robin cursor"
        );

        let mut next = state.config();
        next.conversation_sticky = false;
        state
            .set_config(next)
            .expect("conversation sticky change should reset routing");

        assert_eq!(
            state
                .routing
                .select_account(&accounts, RoutingMode::RoundRobin, false, None, &[])
                .unwrap()
                .id,
            "a"
        );

        drop(state);
        fs::remove_dir_all(dir).expect("test data directory should be removed");
    }

    #[test]
    fn settings_revision_advances_only_after_successful_commit() {
        let dir = temp_data_dir("settings-revision");
        let db = Database::open(dir.clone()).expect("test database should open");
        let cipher: Arc<dyn KeyCipher + Send + Sync> = Arc::new(StaticKeyCipher::new("state-test"));
        let state = CoreStateInner::new(db, dir.clone(), cipher).expect("state should initialize");
        let initial_revision = state.settings_revision();

        let mut valid = state.config();
        valid.connect_timeout_secs += 1;
        state.set_config(valid).expect("valid settings should save");
        assert_eq!(state.settings_revision(), initial_revision + 1);

        let committed_revision = state.settings_revision();
        let mut invalid = state.config();
        invalid.connect_timeout_secs = 0;
        assert!(state.set_config(invalid).is_err());
        assert_eq!(state.settings_revision(), committed_revision);

        drop(state);
        fs::remove_dir_all(dir).expect("test data directory should be removed");
    }

    #[test]
    fn desktop_update_state_machine_is_serializable_atomic_and_retriable() {
        let dir = temp_data_dir("desktop-update-state");
        let db = Database::open(dir.clone()).expect("test database should open");
        let cipher: Arc<dyn KeyCipher + Send + Sync> = Arc::new(StaticKeyCipher::new("state-test"));
        let state = Arc::new(
            CoreStateInner::new(db, dir.clone(), cipher).expect("state should initialize"),
        );

        assert_eq!(
            serde_json::to_value(state.desktop_update_status()).expect("status should serialize"),
            serde_json::json!({
                "phase": "idle",
                "downloaded": 0,
                "total": null,
                "error": null,
                "current_version": env!("CARGO_PKG_VERSION"),
                "install_supported": false,
            })
        );
        assert!(!state.set_desktop_update_progress(1, Some(2)));
        assert!(!state.set_desktop_update_installing());

        let started_versions = Arc::new(StdMutex::new(Vec::new()));
        let captured_versions = started_versions.clone();
        state.set_desktop_update_starter(Arc::new(move |expected_version| {
            captured_versions
                .lock()
                .expect("captured versions lock should work")
                .push(expected_version);
            Ok(())
        }));
        assert!(state.desktop_update_supported());
        assert!(state.desktop_update_status().install_supported);

        let barrier = Arc::new(Barrier::new(3));
        let threads = [state.clone(), state.clone()].map(|state| {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                state.start_desktop_update("9.9.9".to_string())
            })
        });
        barrier.wait();
        let results = threads.map(|thread| thread.join().expect("start thread should not panic"));
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Err(DesktopUpdateStartError::Busy)))
                .count(),
            1
        );
        assert_eq!(
            started_versions
                .lock()
                .expect("started versions lock should work")
                .as_slice(),
            ["9.9.9"]
        );
        assert_eq!(
            state.desktop_update_status().phase,
            DesktopUpdatePhase::Checking
        );

        assert!(state.set_desktop_update_progress(25, Some(100)));
        let downloading = state.desktop_update_status();
        assert_eq!(downloading.phase, DesktopUpdatePhase::Downloading);
        assert_eq!(downloading.downloaded, 25);
        assert_eq!(downloading.total, Some(100));
        assert!(state.set_desktop_update_installing());
        assert!(!state.set_desktop_update_progress(50, Some(100)));
        state.set_desktop_update_failed("install failed");
        let failed = state.desktop_update_status();
        assert_eq!(failed.phase, DesktopUpdatePhase::Failed);
        assert_eq!(failed.error.as_deref(), Some("install failed"));

        state
            .start_desktop_update("10.0.0".to_string())
            .expect("a failed update should be retriable");
        let retrying = state.desktop_update_status();
        assert_eq!(retrying.phase, DesktopUpdatePhase::Checking);
        assert_eq!(retrying.downloaded, 0);
        assert_eq!(retrying.total, None);
        assert_eq!(retrying.error, None);
        assert_eq!(
            started_versions
                .lock()
                .expect("started versions lock should work")
                .as_slice(),
            ["9.9.9", "10.0.0"]
        );

        state.set_desktop_update_idle();
        assert_eq!(
            state.desktop_update_status().phase,
            DesktopUpdatePhase::Idle
        );
        drop(state);
        fs::remove_dir_all(dir).expect("test data directory should be removed");
    }

    #[test]
    fn desktop_update_starter_failure_is_reported_in_status() {
        let dir = temp_data_dir("desktop-update-start-failure");
        let db = Database::open(dir.clone()).expect("test database should open");
        let cipher: Arc<dyn KeyCipher + Send + Sync> = Arc::new(StaticKeyCipher::new("state-test"));
        let state = CoreStateInner::new(db, dir.clone(), cipher).expect("state should initialize");
        state.set_desktop_update_starter(Arc::new(|_| anyhow::bail!("starter failed")));

        assert!(matches!(
            state.start_desktop_update("9.9.9".to_string()),
            Err(DesktopUpdateStartError::Starter(_))
        ));
        let status = state.desktop_update_status();
        assert_eq!(status.phase, DesktopUpdatePhase::Failed);
        assert_eq!(status.error.as_deref(), Some("starter failed"));

        drop(state);
        fs::remove_dir_all(dir).expect("test data directory should be removed");
    }

    #[test]
    fn legacy_config_with_embedded_key_list_drops_it_and_keeps_the_scalar() {
        let dir = temp_data_dir("gateway-key-legacy-list");
        let db = Database::open(dir.clone()).expect("test database should open");
        // The never-released PR #43 form stored a key list inside the config
        // JSON; loading it must keep the scalar and ignore the list.
        let legacy = serde_json::json!({
            "gateway_port": 9042,
            "gateway_key": "ocg-legacy-key",
            "gateway_keys": [
                {
                    "id": "old-primary",
                    "name": "Primary",
                    "key": "ocg-legacy-key",
                    "enabled": true,
                    "created_at": "2026-08-16T00:00:00Z"
                },
                {
                    "id": "old-laptop",
                    "name": "Laptop",
                    "key": "ocg-old-laptop",
                    "enabled": true,
                    "created_at": "2026-08-16T00:00:00Z"
                }
            ],
            "upstream_base_url": "https://opencode.ai/zen/go",
        });
        db.set_setting("config", &legacy.to_string())
            .expect("legacy config should persist");
        let cipher: Arc<dyn KeyCipher + Send + Sync> = Arc::new(StaticKeyCipher::new("state-test"));
        let state = CoreStateInner::new(db, dir.clone(), cipher).expect("state should initialize");

        let config = state.config();
        assert_eq!(config.gateway_key, "ocg-legacy-key");
        assert!(
            state.credential_entry_for_value("ocg-legacy-key").is_some(),
            "the legacy scalar authenticates as the primary key"
        );
        assert!(
            state.credential_entry_for_value("ocg-old-laptop").is_none(),
            "the embedded sub key list is ignored"
        );

        drop(state);
        fs::remove_dir_all(dir).expect("test data directory should be removed");
    }

    #[test]
    fn empty_config_generates_a_primary_key_value() {
        let dir = temp_data_dir("gateway-key-fresh");
        let db = Database::open(dir.clone()).expect("test database should open");
        let cipher: Arc<dyn KeyCipher + Send + Sync> = Arc::new(StaticKeyCipher::new("state-test"));
        let state = CoreStateInner::new(db, dir.clone(), cipher).expect("state should initialize");

        let config = state.config();
        assert!(!config.gateway_key.is_empty());
        assert!(
            state
                .credential_entry_for_value(&config.gateway_key)
                .is_some()
        );
        assert_eq!(
            state
                .client_key_name(crate::gateway_keys::PRIMARY_KEY_ID)
                .as_deref(),
            Some(crate::gateway_keys::PRIMARY_KEY_NAME),
        );

        drop(state);
        fs::remove_dir_all(dir).expect("test data directory should be removed");
    }

    #[test]
    fn routing_resets_only_when_routing_fields_or_primary_key_change() {
        use crate::crypto::{KeyCipher, StaticKeyCipher};
        use crate::models::{Account, RoutingMode};
        use std::sync::Arc;

        fn test_account(cipher: &Arc<dyn KeyCipher + Send + Sync>, id: &str) -> Account {
            Account {
                id: id.into(),
                name: id.into(),
                username: None,
                password_cipher: None,
                key_cipher: cipher.encrypt(id).unwrap(),
                enabled: true,
                account_type: crate::models::AccountType::Key,
                setup_step: crate::models::AccountSetupStep::Ready,
                referral_code: None,
                purchase_date: String::new(),
                expires_on: String::new(),
                cooldown_until: None,
                cooldown_generic_until: None,
                cooldown_5h_until: None,
                cooldown_week_until: None,
                cooldown_month_until: None,
                cooldown_free_until: None,
                last_error: None,
                auth_error: None,
                notes: None,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            }
        }

        let dir = temp_data_dir("routing-key-values");
        let db = Database::open(dir.clone()).expect("test database should open");
        let cipher: Arc<dyn KeyCipher + Send + Sync> = Arc::new(StaticKeyCipher::new("state-test"));
        let state =
            CoreStateInner::new(db, dir.clone(), cipher.clone()).expect("state should initialize");
        let accounts = vec![test_account(&cipher, "a"), test_account(&cipher, "b")];
        let advance = || {
            state
                .routing
                .select_account(&accounts, RoutingMode::RoundRobin, false, None, &[])
                .unwrap()
                .id
                .clone()
        };

        assert_eq!(advance(), "a");

        // An unrelated settings save keeps sticky routing state intact: with
        // the cursor after "a", the next pick stays "b"; a reset would
        // restart at "a". (Sub key lifecycle changes never go through
        // set_config; their revocation resets are endpoint-driven.)
        assert_eq!(advance(), "b");
        let mut config = state.config();
        config.connect_timeout_secs += 1;
        state.set_config(config).expect("unrelated save");
        assert_eq!(
            advance(),
            "a",
            "an unrelated settings save must not reset routing"
        );

        assert_eq!(advance(), "b");
        // Rotating the primary key value resets: the old value stops
        // authenticating.
        let mut config = state.config();
        config.gateway_key = "ocg-rotated-primary".to_string();
        state.set_config(config).expect("rotation should save");
        assert_eq!(
            advance(),
            "a",
            "the cursor restarts after the primary key value changes"
        );
        assert!(
            state
                .credential_entry_for_value("ocg-rotated-primary")
                .is_some()
        );

        drop(state);
        fs::remove_dir_all(dir).expect("test data directory should be removed");
    }

    #[test]
    fn legacy_config_gets_persisted_desktop_defaults() {
        let dir = temp_data_dir("desktop-config-migration");
        let db = Database::open(dir.clone()).expect("test database should open");
        let mut legacy = serde_json::to_value(AppConfig {
            gateway_key: "test-gateway-key".to_string(),
            ..AppConfig::default()
        })
        .expect("test config should serialize");
        {
            let legacy_object = legacy
                .as_object_mut()
                .expect("test config should be an object");
            legacy_object.remove("claude_desktop_models");
            legacy_object.remove("show_dock_icon");
            legacy_object.remove("routing_mode");
            legacy_object.remove("conversation_sticky");
            legacy_object.remove("proxy_mode");
            legacy_object.remove("proxy_url");
        }
        db.set_setting("config", &legacy.to_string())
            .expect("legacy config should persist");
        let cipher: Arc<dyn KeyCipher + Send + Sync> = Arc::new(StaticKeyCipher::new("state-test"));
        let state = CoreStateInner::new(db, dir.clone(), cipher).expect("state should initialize");

        assert_eq!(
            state.config().claude_desktop_models.resolved(),
            AppConfig::default().claude_desktop_models.resolved()
        );
        assert!(state.config().show_dock_icon);
        assert_eq!(
            state.config().routing_mode,
            crate::models::RoutingMode::StickyGlobal
        );
        assert!(state.config().conversation_sticky);
        assert_eq!(state.config().proxy_mode, ProxyMode::Auto);
        assert!(state.config().proxy_url.is_empty());
        let stored = state
            .db
            .lock()
            .get_setting("config")
            .expect("stored config should be readable")
            .expect("stored config should exist");
        assert!(stored.contains("claude_desktop_models"));
        assert!(stored.contains("show_dock_icon"));
        assert!(stored.contains("proxy_mode"));
        assert!(stored.contains("proxy_url"));

        drop(state);
        fs::remove_dir_all(dir).expect("test data directory should be removed");
    }
}
