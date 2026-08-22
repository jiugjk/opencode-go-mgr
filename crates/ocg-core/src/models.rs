use std::fmt;

use chrono::{DateTime, Datelike, Local, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Default model for dashboard account ping and CLI `ping`.
pub const DEFAULT_ACCOUNT_TEST_MODEL: &str = "mimo-v2.5";

/// Maximum persisted freeform account note length, counted in Unicode scalars.
pub const MAX_ACCOUNT_NOTES_CHARS: usize = 4000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    pub name: String,
    pub username: Option<String>,
    pub password_cipher: Option<String>,
    pub key_cipher: String,
    pub enabled: bool,
    #[serde(default)]
    pub account_type: AccountType,
    #[serde(default)]
    pub setup_step: AccountSetupStep,
    pub referral_code: Option<String>,
    #[serde(alias = "recharge_date")]
    pub purchase_date: String,
    #[serde(default)]
    pub expires_on: String,
    /// Derived: when the account becomes usable after every active cooldown expires.
    /// Kept for backwards compatibility; `None` means currently available.
    pub cooldown_until: Option<DateTime<Utc>>,
    #[serde(default)]
    pub cooldown_generic_until: Option<DateTime<Utc>>,
    #[serde(default)]
    pub cooldown_5h_until: Option<DateTime<Utc>>,
    #[serde(default)]
    pub cooldown_week_until: Option<DateTime<Utc>>,
    #[serde(default)]
    pub cooldown_month_until: Option<DateTime<Utc>>,
    #[serde(default)]
    pub cooldown_free_until: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    /// A persisted upstream 401 marker. Accounts with an auth error remain
    /// enabled for management purposes, but are excluded from gateway routing
    /// until their key is replaced or a direct ping proves the key works again.
    #[serde(default)]
    pub auth_error: Option<String>,
    /// Optional freeform note. Empty or omitted is valid.
    #[serde(default)]
    pub notes: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AccountType {
    #[default]
    Key,
    Managed,
}

impl AccountType {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Key => "key",
            Self::Managed => "managed",
        }
    }
}

impl TryFrom<&str> for AccountType {
    type Error = String;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "key" => Ok(Self::Key),
            "managed" => Ok(Self::Managed),
            _ => Err(format!("unknown account type `{value}`")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AccountSetupStep {
    GoogleAccount,
    OpencodeRegistration,
    Payment,
    KeyVerification,
    #[default]
    Ready,
}

impl AccountSetupStep {
    pub fn next(self) -> Option<Self> {
        match self {
            Self::GoogleAccount => Some(Self::OpencodeRegistration),
            Self::OpencodeRegistration => Some(Self::Payment),
            Self::Payment => Some(Self::KeyVerification),
            Self::KeyVerification | Self::Ready => None,
        }
    }

    pub fn is_ready(self) -> bool {
        self == Self::Ready
    }

    /// Wizard progress index for unfinished steps. `ready` is not part of the wizard.
    pub const fn wizard_index(self) -> Option<u8> {
        match self {
            Self::GoogleAccount => Some(0),
            Self::OpencodeRegistration => Some(1),
            Self::Payment => Some(2),
            Self::KeyVerification => Some(3),
            Self::Ready => None,
        }
    }

    /// Forward exactly one step, or rewind to any earlier unfinished step.
    pub fn can_transition_to(self, to: Self) -> bool {
        let Some(from_i) = self.wizard_index() else {
            return false;
        };
        let Some(to_i) = to.wizard_index() else {
            return false;
        };
        to_i == from_i + 1 || to_i < from_i
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::GoogleAccount => "google_account",
            Self::OpencodeRegistration => "opencode_registration",
            Self::Payment => "payment",
            Self::KeyVerification => "key_verification",
            Self::Ready => "ready",
        }
    }
}

impl TryFrom<&str> for AccountSetupStep {
    type Error = String;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "google_account" => Ok(Self::GoogleAccount),
            "opencode_registration" => Ok(Self::OpencodeRegistration),
            "payment" => Ok(Self::Payment),
            "key_verification" => Ok(Self::KeyVerification),
            "ready" => Ok(Self::Ready),
            _ => Err(format!("unknown account setup step `{value}`")),
        }
    }
}

impl Account {
    /// Latest end among every cooldown window (UI / any-channel busy).
    pub fn cooldown_ends_at(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        [
            self.cooldown_generic_until,
            self.cooldown_5h_until,
            self.cooldown_week_until,
            self.cooldown_month_until,
            self.cooldown_free_until,
        ]
        .into_iter()
        .flatten()
        .filter(|until| *until > now)
        .max()
    }

    pub fn is_cooling_at(&self, now: DateTime<Utc>) -> bool {
        self.cooldown_ends_at(now).is_some()
    }

    /// Go routing ignores free promo cooldown; free routing ignores Go usage windows.
    /// Free 429s are IP-shared: the selector treats any active `cooldown_free_until`
    /// as exhausting the whole free channel.
    pub fn cooldown_ends_at_for(
        &self,
        channel: UpstreamChannel,
        now: DateTime<Utc>,
    ) -> Option<DateTime<Utc>> {
        let windows: &[Option<DateTime<Utc>>] = match channel {
            UpstreamChannel::Go => &[
                self.cooldown_generic_until,
                self.cooldown_5h_until,
                self.cooldown_week_until,
                self.cooldown_month_until,
            ],
            UpstreamChannel::Free => &[self.cooldown_generic_until, self.cooldown_free_until],
        };
        windows
            .iter()
            .copied()
            .flatten()
            .filter(|until| *until > now)
            .max()
    }

    pub fn is_cooling_for(&self, channel: UpstreamChannel, now: DateTime<Utc>) -> bool {
        self.cooldown_ends_at_for(channel, now).is_some()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountInput {
    pub name: String,
    pub username: Option<String>,
    pub password: Option<String>,
    pub key: String,
    pub referral_code: Option<String>,
    #[serde(alias = "recharge_date")]
    pub purchase_date: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AccountUpdate {
    pub name: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub key: Option<String>,
    pub enabled: Option<bool>,
    pub referral_code: Option<String>,
    #[serde(alias = "recharge_date")]
    pub purchase_date: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PurchaseDateError;

impl fmt::Display for PurchaseDateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("purchase date must use the YYYY-MM-DD format")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccountNotesError;

impl fmt::Display for AccountNotesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "notes must be at most {MAX_ACCOUNT_NOTES_CHARS} characters"
        )
    }
}

impl std::error::Error for AccountNotesError {}

/// Trims a freeform account note. Empty input becomes `None`; overlong input is rejected.
pub fn normalize_account_notes(value: &str) -> Result<Option<String>, AccountNotesError> {
    let trimmed = value.trim();
    if trimmed.chars().count() > MAX_ACCOUNT_NOTES_CHARS {
        return Err(AccountNotesError);
    }
    if trimmed.is_empty() {
        Ok(None)
    } else {
        Ok(Some(trimmed.to_string()))
    }
}

impl std::error::Error for PurchaseDateError {}

/// Returns the current calendar date in the process's local timezone.
pub fn local_today() -> String {
    format_date(Local::now().date_naive())
}

/// Validates a purchase date and returns its canonical `YYYY-MM-DD` representation.
pub fn normalize_purchase_date(value: &str) -> Result<String, PurchaseDateError> {
    let parsed = NaiveDate::parse_from_str(value, "%Y-%m-%d").map_err(|_| PurchaseDateError)?;
    let normalized = format_date(parsed);
    if normalized != value {
        return Err(PurchaseDateError);
    }
    Ok(normalized)
}

/// Calculates the natural-month expiry date, clamping to the target month's last day.
pub fn purchase_expires_on(value: &str) -> Result<String, PurchaseDateError> {
    let normalized = normalize_purchase_date(value)?;
    let purchase =
        NaiveDate::parse_from_str(&normalized, "%Y-%m-%d").map_err(|_| PurchaseDateError)?;
    let (target_year, target_month) = next_month(purchase.year(), purchase.month())?;
    let (following_year, following_month) = next_month(target_year, target_month)?;
    let target_last_day = NaiveDate::from_ymd_opt(following_year, following_month, 1)
        .and_then(|date| date.pred_opt())
        .ok_or(PurchaseDateError)?
        .day();
    let expires = NaiveDate::from_ymd_opt(
        target_year,
        target_month,
        purchase.day().min(target_last_day),
    )
    .ok_or(PurchaseDateError)?;
    Ok(format_date(expires))
}

fn next_month(year: i32, month: u32) -> Result<(i32, u32), PurchaseDateError> {
    if month == 12 {
        Ok((year.checked_add(1).ok_or(PurchaseDateError)?, 1))
    } else {
        Ok((year, month + 1))
    }
}

fn format_date(date: NaiveDate) -> String {
    date.format("%Y-%m-%d").to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum FreeModelRouting {
    /// Reject free model ids; never map Go requests onto free twins.
    Deny,
    /// Only explicit free model ids use the Zen free channel.
    Explicit,
    /// Prefer mapped free twins when context fits; fall back to Go (default).
    #[default]
    Prefer,
}

/// Upstream product channel for account selection and cooldown windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamChannel {
    Go,
    Free,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum RoutingMode {
    StrictPriority,
    #[default]
    StickyGlobal,
    RoundRobin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ProxyMode {
    /// Use the platform/environment proxy configuration when available.
    #[default]
    Auto,
    /// Route every supported outbound HTTP(S) request through one explicit proxy.
    Manual,
    /// Ignore platform/environment proxy configuration and connect directly.
    Direct,
    /// Route per model against `proxy_list_models`: listed models use the
    /// direction's exception leg, everything else (including non-model-scoped
    /// outbound traffic) uses the direction's default leg.
    List,
}

/// Which leg the listed models take in list proxy mode. The other leg is the
/// direction's default for unlisted models and non-model-scoped traffic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ProxyListDirection {
    /// Listed models go through `proxy_url`; everything else connects directly.
    #[default]
    Whitelist,
    /// Listed models connect directly; everything else goes through `proxy_url`.
    Blacklist,
}

pub const DEFAULT_OPENCODE_INVITE_URL: &str = "https://opencode.ai/go?ref=55G3ETNT1Q";

/// Shared rejection message for a blank primary gateway key; used by
/// `AppConfig::validate` and both settings-update entry points.
pub const PRIMARY_KEY_REQUIRED_MESSAGE: &str = "key is required";

/// Sentinel filter value selecting forward logs without a client key
/// (written before multi-key support or not yet backfilled).
pub const UNATTRIBUTED_KEY_FILTER: &str = "__unattributed__";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub gateway_port: u16,
    pub gateway_key: String,
    pub upstream_base_url: String,
    pub proxy_mode: ProxyMode,
    pub proxy_url: String,
    #[serde(default)]
    pub proxy_list_direction: ProxyListDirection,
    #[serde(default)]
    pub proxy_list_models: Vec<String>,
    pub opencode_invite_url: String,
    pub client_root_url: String,
    pub auto_start: bool,
    pub show_dock_icon: bool,
    pub connect_timeout_secs: u64,
    pub non_stream_timeout_secs: u64,
    pub stream_idle_timeout_secs: u64,
    pub routing_mode: RoutingMode,
    pub conversation_sticky: bool,
    #[serde(default)]
    pub free_model_routing: FreeModelRouting,
    pub claude_desktop_models: ClaudeDesktopModels,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            gateway_port: 9042,
            gateway_key: String::new(),
            upstream_base_url: "https://opencode.ai/zen/go".to_string(),
            proxy_mode: ProxyMode::Auto,
            proxy_url: String::new(),
            proxy_list_direction: ProxyListDirection::Whitelist,
            proxy_list_models: Vec::new(),
            opencode_invite_url: DEFAULT_OPENCODE_INVITE_URL.to_string(),
            client_root_url: String::new(),
            auto_start: false,
            show_dock_icon: true,
            connect_timeout_secs: 30,
            non_stream_timeout_secs: 900,
            stream_idle_timeout_secs: 300,
            routing_mode: RoutingMode::StickyGlobal,
            conversation_sticky: true,
            free_model_routing: FreeModelRouting::Prefer,
            claude_desktop_models: ClaudeDesktopModels::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeDesktopModels {
    pub sonnet: String,
    pub opus: String,
    pub haiku: String,
}

impl Default for ClaudeDesktopModels {
    fn default() -> Self {
        Self {
            sonnet: "minimax-m3".to_string(),
            opus: String::new(),
            haiku: String::new(),
        }
    }
}

impl ClaudeDesktopModels {
    pub fn normalize(&mut self) {
        self.sonnet = self.sonnet.trim().to_string();
        self.opus = self.opus.trim().to_string();
        self.haiku = self.haiku.trim().to_string();
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.first_configured().is_none() {
            return Err("at least one Claude Desktop model is required".to_string());
        }
        for (role, model) in [
            ("sonnet", self.sonnet.as_str()),
            ("opus", self.opus.as_str()),
            ("haiku", self.haiku.as_str()),
        ] {
            if model.is_empty() {
                continue;
            }
            if crate::gateway::free_models::is_free_model(model) {
                return Err(format!(
                    "Claude Desktop {role} model `{model}` cannot be a Zen free model"
                ));
            }
            if !crate::gateway::protocol::supported_model_ids().any(|supported| supported == model)
            {
                return Err(format!("unsupported Claude Desktop {role} model `{model}`"));
            }
        }
        Ok(())
    }

    pub fn resolved(&self) -> Self {
        let fallback = self.first_configured().unwrap_or_default();
        Self {
            sonnet: if self.sonnet.is_empty() {
                fallback.to_string()
            } else {
                self.sonnet.clone()
            },
            opus: if self.opus.is_empty() {
                fallback.to_string()
            } else {
                self.opus.clone()
            },
            haiku: if self.haiku.is_empty() {
                fallback.to_string()
            } else {
                self.haiku.clone()
            },
        }
    }

    pub(crate) fn model_for_alias(&self, alias: &str) -> Option<&str> {
        let configured = match alias {
            CLAUDE_DESKTOP_SONNET_ALIAS => self.sonnet.as_str(),
            CLAUDE_DESKTOP_OPUS_ALIAS => self.opus.as_str(),
            CLAUDE_DESKTOP_HAIKU_ALIAS => self.haiku.as_str(),
            _ => return None,
        };
        (!configured.is_empty())
            .then_some(configured)
            .or_else(|| self.first_configured())
    }

    fn first_configured(&self) -> Option<&str> {
        [
            self.sonnet.as_str(),
            self.opus.as_str(),
            self.haiku.as_str(),
        ]
        .into_iter()
        .find(|model| !model.is_empty())
    }
}

pub const CLAUDE_DESKTOP_SONNET_ALIAS: &str = "claude-sonnet-4-6";
pub const CLAUDE_DESKTOP_OPUS_ALIAS: &str = "claude-opus-4-6";
pub const CLAUDE_DESKTOP_HAIKU_ALIAS: &str = "claude-haiku-4-5-20251001";

/// Validates and canonicalizes the optional URL shown to downstream clients.
pub fn normalize_client_root_url(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(String::new());
    }
    let lower = value.to_ascii_lowercase();
    if !lower.starts_with("http://") && !lower.starts_with("https://") {
        return Err("client root URL must be an absolute http:// or https:// URL".to_string());
    }

    let mut url =
        reqwest::Url::parse(value).map_err(|error| format!("invalid client root URL: {error}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("client root URL must use http or https".to_string());
    }
    if url.host_str().is_none() {
        return Err("client root URL must include a host".to_string());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("client root URL must not include credentials".to_string());
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err("client root URL must not include a query or fragment".to_string());
    }

    let mut path = url.path().trim_end_matches('/').to_string();
    let segments = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    if let Some(index) = segments
        .iter()
        .position(|segment| segment.eq_ignore_ascii_case("v1"))
    {
        if index + 1 != segments.len() {
            return Err("client root URL must not include an endpoint after /v1".to_string());
        }
        path.truncate(path.len() - "/v1".len());
        path.truncate(path.trim_end_matches('/').len());
    }

    url.set_path(if path.is_empty() { "/" } else { &path });
    Ok(url.as_str().trim_end_matches('/').to_string())
}

impl AppConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.gateway_key.trim().is_empty() {
            return Err(PRIMARY_KEY_REQUIRED_MESSAGE.to_string());
        }
        self.validate_timeouts()?;
        normalize_proxy_url(self.proxy_mode, &self.proxy_url)?;
        normalize_opencode_invite_url(&self.opencode_invite_url)?;
        // routing_mode is validated by serde enum decoding; unknown values never reach here.
        self.claude_desktop_models.validate()
    }

    pub fn validate_timeouts(&self) -> Result<(), String> {
        for (name, value, max) in [
            ("connect_timeout_secs", self.connect_timeout_secs, 300),
            (
                "non_stream_timeout_secs",
                self.non_stream_timeout_secs,
                3600,
            ),
            (
                "stream_idle_timeout_secs",
                self.stream_idle_timeout_secs,
                3600,
            ),
        ] {
            if !(1..=max).contains(&value) {
                return Err(format!("{name} must be between 1 and {max}"));
            }
        }
        Ok(())
    }
}

/// Validates and canonicalizes the optional global outbound HTTP proxy URL.
///
/// Manual and list modes both require a usable URL (the list legs route
/// through it); unused leftover values must not block Auto/Direct saves.
pub fn normalize_proxy_url(mode: ProxyMode, value: &str) -> Result<String, String> {
    let value = value.trim();
    let url_required = matches!(mode, ProxyMode::Manual | ProxyMode::List);
    if value.is_empty() {
        return if url_required {
            Err(match mode {
                ProxyMode::List => "list proxy mode requires a proxy URL".to_string(),
                _ => "manual proxy mode requires a proxy URL".to_string(),
            })
        } else {
            Ok(String::new())
        };
    }

    match canonicalize_proxy_url(value) {
        Ok(normalized) => Ok(normalized),
        Err(error) if url_required => Err(error),
        // Unused leftover values must not block Auto/Direct saves.
        Err(_) => Ok(value.to_string()),
    }
}

fn canonicalize_proxy_url(value: &str) -> Result<String, String> {
    if value.len() > 2048 {
        return Err("proxy URL is too long".to_string());
    }

    let parsed =
        reqwest::Url::parse(value).map_err(|error| format!("invalid proxy URL: {error}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("proxy URL must use http or https".to_string());
    }
    if parsed.host_str().is_none() {
        return Err("proxy URL must include a host".to_string());
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("proxy URL must not include credentials".to_string());
    }
    if !matches!(parsed.path(), "" | "/") || parsed.query().is_some() || parsed.fragment().is_some()
    {
        return Err("proxy URL must not include a path, query, or fragment".to_string());
    }

    Ok(parsed.as_str().trim_end_matches('/').to_string())
}

pub fn normalize_opencode_invite_url(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(String::new());
    }
    if value.len() > 2048 {
        return Err("OpenCode invite URL is too long".to_string());
    }
    let parsed = reqwest::Url::parse(value)
        .map_err(|error| format!("invalid OpenCode invite URL: {error}"))?;
    if parsed.scheme() != "https" {
        return Err("OpenCode invite URL must use https".to_string());
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("OpenCode invite URL must not contain credentials".to_string());
    }
    match parsed.host_str() {
        Some("opencode.ai" | "console.opencode.ai") => {}
        _ => {
            return Err(
                "OpenCode invite URL host must be opencode.ai or console.opencode.ai".to_string(),
            );
        }
    }
    Ok(parsed.to_string())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayLog {
    pub id: i64,
    pub level: String,
    pub category: String,
    pub message: String,
    pub created_at: DateTime<Utc>,
    pub request_id: Option<String>,
    pub attempt: Option<i64>,
    pub error_source: Option<String>,
    pub error_stage: Option<String>,
    pub duration_ms: Option<i64>,
    pub diagnostic: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForwardLog {
    pub id: i64,
    pub timestamp: DateTime<Utc>,
    pub model: String,
    pub account_id: String,
    pub account_name: String,
    #[serde(default)]
    pub client_key_id: Option<String>,
    #[serde(default)]
    pub client_key_name: Option<String>,
    pub status: String,
    pub http_status: Option<i32>,
    /// Route leg label for this attempt: `auto`, `proxy`, or `direct`.
    /// Empty for rows written before the column existed ("not recorded").
    #[serde(default)]
    pub route: String,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cached_tokens: i64,
    pub cache_creation_tokens: i64,
    pub cost: Option<f64>,
    pub pricing_revision_id: Option<String>,
    pub quota_multiplier: Option<f64>,
    pub local_adjustment_multiplier: Option<f64>,
    pub service_tier: Option<String>,
    pub cost_state: String,
    pub error_message: Option<String>,
    pub request_id: Option<String>,
    pub attempt: Option<i64>,
    pub error_source: Option<String>,
    pub error_stage: Option<String>,
    pub duration_ms: Option<i64>,
    pub diagnostic: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct ForwardMetrics {
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cached_tokens: i64,
    pub cache_creation_tokens: i64,
    pub cost: f64,
    pub pricing_revision_id: Option<String>,
    pub quota_multiplier: Option<f64>,
    pub local_adjustment_multiplier: Option<f64>,
    pub service_tier: Option<String>,
    pub cost_state: &'static str,
}

impl Default for ForwardMetrics {
    fn default() -> Self {
        Self {
            prompt_tokens: 0,
            completion_tokens: 0,
            cached_tokens: 0,
            cache_creation_tokens: 0,
            cost: 0.0,
            pricing_revision_id: None,
            quota_multiplier: None,
            local_adjustment_multiplier: None,
            service_tier: None,
            cost_state: "not_applicable",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForwardLogSummary {
    pub total_requests: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cached_tokens: i64,
    pub cost: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForwardLogPage {
    pub items: Vec<ForwardLog>,
    pub summary: ForwardLogSummary,
}

/// One distinct client key observed in forward logs (see
/// `Database::list_forward_log_keys`). Covers enabled, disabled, and
/// soft-deleted keys plus dangling ids left by a downgrade.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForwardLogClientKey {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageWindow {
    pub account_id: String,
    pub window_5h: f64,
    pub window_week: f64,
    pub window_month: f64,
    /// 当 5h 固定窗口仍有效时，表示该窗口的清零时刻；`None` 表示窗口尚未开始（无成功请求）。
    #[serde(default)]
    pub resets_in_5h: Option<DateTime<Utc>>,
    /// 当周固定窗口的清零时刻；`None` 表示窗口尚未开始。
    #[serde(default)]
    pub resets_in_week: Option<DateTime<Utc>>,
    /// 月窗口的到期时刻，固定为 `purchase_expires_on(purchase_date) 00:00`；`None` 表示账号无购买日期。
    #[serde(default)]
    pub resets_in_month: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageWindowKind {
    FiveHours,
    Week,
    Month,
    /// Zen free-model promo quota (independent of Go usage windows).
    Free,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayStatus {
    pub running: bool,
    pub port: u16,
    /// Primary key value; kept for legacy consumers.
    pub key: String,
    pub upstream_base_url: String,
    pub last_error: Option<String>,
}

/// One database-owned sub gateway key (schema v20 `sub_gateway_keys`).
/// `key` holds the plaintext value and is cleared on soft delete so deleted
/// credentials never resurface in management APIs while the record stays
/// resolvable for log attribution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubGatewayKey {
    pub id: String,
    pub name: String,
    pub key: String,
    pub enabled: bool,
    pub deleted_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

impl SubGatewayKey {
    pub fn is_active(&self) -> bool {
        self.deleted_at.is_none()
    }

    pub fn authenticates(&self) -> bool {
        self.enabled && self.is_active() && !self.key.is_empty()
    }
}

/// A sub key as exposed by the lightweight connection endpoint. Plaintext is
/// behind the dashboard session layer, same as the primary key value.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionSubKey {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub value: String,
}

/// Aggregated connection view for the dashboard connection center: primary
/// key value, non-deleted sub keys with values, settings revision, and the
/// fields needed to derive client-facing URLs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionInfo {
    pub gateway_port: u16,
    pub client_root_url: String,
    pub upstream_base_url: String,
    pub primary_key: String,
    pub sub_keys: Vec<ConnectionSubKey>,
    pub revision: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardSummary {
    pub total_accounts: usize,
    pub available_accounts: usize,
    pub gateway_running: bool,
    pub today_cost: f64,
    pub week_cost: f64,
    pub month_cost: f64,
}

/// One row of "daily cost per model" aggregation for the dashboard chart.
/// `date` is `YYYY-MM-DD` (UTC). The frontend groups rows by date into a
/// stacked bar for each day.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DailyModelCost {
    pub date: String,
    pub model: String,
    pub cost: f64,
}

#[cfg(test)]
mod tests {
    use super::{
        AccountInput, AppConfig, CLAUDE_DESKTOP_HAIKU_ALIAS, CLAUDE_DESKTOP_OPUS_ALIAS,
        CLAUDE_DESKTOP_SONNET_ALIAS, ClaudeDesktopModels, DEFAULT_OPENCODE_INVITE_URL,
        FreeModelRouting, MAX_ACCOUNT_NOTES_CHARS, ProxyListDirection, ProxyMode, RoutingMode,
        normalize_account_notes, normalize_opencode_invite_url, normalize_proxy_url,
        normalize_purchase_date, purchase_expires_on,
    };

    #[test]
    fn claude_desktop_models_map_aliases_and_inherit_by_role_priority() {
        let models = ClaudeDesktopModels {
            sonnet: String::new(),
            opus: "glm-5.2".to_string(),
            haiku: "mimo-v2.5".to_string(),
        };

        assert_eq!(
            models.model_for_alias(CLAUDE_DESKTOP_SONNET_ALIAS),
            Some("glm-5.2")
        );
        assert_eq!(
            models.model_for_alias(CLAUDE_DESKTOP_OPUS_ALIAS),
            Some("glm-5.2")
        );
        assert_eq!(
            models.model_for_alias(CLAUDE_DESKTOP_HAIKU_ALIAS),
            Some("mimo-v2.5")
        );
        assert_eq!(models.model_for_alias("claude-unknown"), None);
    }

    #[test]
    fn claude_desktop_models_reject_unknown_and_all_empty_values() {
        let empty = ClaudeDesktopModels {
            sonnet: String::new(),
            opus: String::new(),
            haiku: String::new(),
        };
        assert!(empty.validate().is_err());

        let unknown = ClaudeDesktopModels {
            sonnet: "not-a-supported-model".to_string(),
            ..ClaudeDesktopModels::default()
        };
        assert!(unknown.validate().is_err());
        assert!(ClaudeDesktopModels::default().validate().is_ok());
    }

    #[test]
    fn account_notes_trim_empty_and_reject_overlong() {
        assert_eq!(normalize_account_notes("").unwrap(), None);
        assert_eq!(normalize_account_notes("   ").unwrap(), None);
        assert_eq!(
            normalize_account_notes("  keep this  ").unwrap().as_deref(),
            Some("keep this")
        );
        let overlong = "n".repeat(MAX_ACCOUNT_NOTES_CHARS + 1);
        assert!(normalize_account_notes(&overlong).is_err());
        let max = "你".repeat(MAX_ACCOUNT_NOTES_CHARS);
        assert_eq!(
            normalize_account_notes(&max).unwrap().as_deref(),
            Some(max.as_str())
        );
    }

    #[test]
    fn purchase_dates_require_canonical_calendar_dates() {
        assert_eq!(
            normalize_purchase_date("2026-07-15").expect("valid date should normalize"),
            "2026-07-15"
        );
        for invalid in ["2026-7-15", " 2026-07-15", "2026-07-15 ", "2026-02-29", ""] {
            assert!(
                normalize_purchase_date(invalid).is_err(),
                "{invalid:?} should be rejected"
            );
        }
    }

    #[test]
    fn purchase_expiry_uses_the_next_natural_month() {
        for (purchase, expected) in [
            ("2026-01-15", "2026-02-15"),
            ("2026-01-31", "2026-02-28"),
            ("2024-01-31", "2024-02-29"),
            ("2024-02-29", "2024-03-29"),
            ("2026-12-31", "2027-01-31"),
        ] {
            assert_eq!(
                purchase_expires_on(purchase).expect("valid date should have an expiry"),
                expected
            );
        }
    }

    #[test]
    fn account_input_accepts_legacy_recharge_date_but_serializes_the_new_name() {
        let input: AccountInput = serde_json::from_value(serde_json::json!({
            "name": "legacy",
            "key": "key",
            "recharge_date": "2026-07-15"
        }))
        .expect("legacy input should deserialize");
        assert_eq!(input.purchase_date.as_deref(), Some("2026-07-15"));

        let json = serde_json::to_value(input).expect("input should serialize");
        assert_eq!(json["purchase_date"], "2026-07-15");
        assert!(json.get("recharge_date").is_none());
    }

    #[test]
    fn routing_mode_defaults_and_rejects_unknown_values() {
        let missing: AppConfig = serde_json::from_value(serde_json::json!({
            "gateway_key": "k"
        }))
        .expect("missing routing fields should default");
        assert_eq!(missing.routing_mode, RoutingMode::StickyGlobal);
        assert!(missing.conversation_sticky);
        assert_eq!(missing.free_model_routing, FreeModelRouting::Prefer);
        assert_eq!(missing.proxy_mode, ProxyMode::Auto);
        assert!(missing.proxy_url.is_empty());

        for mode in [
            RoutingMode::StrictPriority,
            RoutingMode::StickyGlobal,
            RoutingMode::RoundRobin,
        ] {
            let config = AppConfig {
                routing_mode: mode,
                conversation_sticky: true,
                gateway_key: "k".into(),
                ..AppConfig::default()
            };
            config.validate().expect("valid routing config");
            let encoded = serde_json::to_value(&config).expect("serialize");
            let decoded: AppConfig =
                serde_json::from_value(encoded).expect("round-trip routing config");
            assert_eq!(decoded.routing_mode, mode);
            assert!(decoded.conversation_sticky);
        }

        assert!(
            serde_json::from_value::<AppConfig>(serde_json::json!({
                "gateway_key": "k",
                "routing_mode": "weighted"
            }))
            .is_err()
        );
    }

    #[test]
    fn legacy_config_json_with_gateway_keys_list_keeps_the_scalar_key() {
        // Config JSON written by the never-released PR #43 form embeds a
        // `gateway_keys` list; current builds ignore it and keep the legacy
        // scalar, so downgraded databases stay readable either way.
        let legacy: AppConfig = serde_json::from_value(serde_json::json!({
            "gateway_key": "ocg-legacy-key",
            "gateway_keys": [
                {
                    "id": "key-1",
                    "name": "Primary",
                    "key": "ocg-legacy-key",
                    "enabled": true,
                    "created_at": "2026-08-16T00:00:00Z"
                }
            ],
            "upstream_base_url": "https://opencode.ai/zen/go"
        }))
        .expect("legacy config with an embedded key list should deserialize");
        assert_eq!(legacy.gateway_key, "ocg-legacy-key");
        legacy
            .validate()
            .expect("the scalar key satisfies validation");

        let encoded = serde_json::to_value(&AppConfig {
            gateway_key: "ocg-keep".into(),
            ..AppConfig::default()
        })
        .expect("config should serialize");
        assert!(encoded.get("gateway_keys").is_none());
    }

    #[test]
    fn blank_primary_key_is_rejected_by_validate() {
        for blank in ["", "   ", "\t"] {
            let config = AppConfig {
                gateway_key: blank.to_string(),
                ..AppConfig::default()
            };
            assert_eq!(
                config.validate().unwrap_err(),
                "key is required",
                "{blank:?} must be rejected"
            );
        }
        AppConfig {
            gateway_key: "  padded  ".into(),
            ..AppConfig::default()
        }
        .validate()
        .expect("a non-blank key passes");
    }

    #[test]
    fn proxy_url_requires_a_supported_origin_without_credentials() {
        assert_eq!(
            normalize_proxy_url(ProxyMode::Manual, " http://127.0.0.1:7890/ ").unwrap(),
            "http://127.0.0.1:7890"
        );
        assert_eq!(
            normalize_proxy_url(ProxyMode::Auto, "").unwrap(),
            String::new()
        );
        assert_eq!(
            normalize_proxy_url(ProxyMode::Auto, " http://127.0.0.1:7890/ ").unwrap(),
            "http://127.0.0.1:7890"
        );
        assert_eq!(
            normalize_proxy_url(ProxyMode::Direct, "socks5://127.0.0.1:1080").unwrap(),
            "socks5://127.0.0.1:1080"
        );
        assert!(normalize_proxy_url(ProxyMode::Manual, "").is_err());
        for invalid in [
            "socks5://127.0.0.1:1080",
            "http://user:secret@127.0.0.1:7890",
            "http://127.0.0.1:7890/proxy",
            "http://127.0.0.1:7890?x=1",
        ] {
            assert!(
                normalize_proxy_url(ProxyMode::Manual, invalid).is_err(),
                "{invalid:?} should be rejected"
            );
        }

        AppConfig {
            gateway_key: "k".to_string(),
            proxy_mode: ProxyMode::Auto,
            proxy_url: "not-a-proxy".to_string(),
            ..AppConfig::default()
        }
        .validate()
        .expect("auto mode must not reject leftover invalid proxy URLs");
    }

    #[test]
    fn list_proxy_mode_requires_a_valid_proxy_url_but_not_a_valid_list() {
        let mut config = AppConfig {
            gateway_key: "k".to_string(),
            proxy_mode: ProxyMode::List,
            proxy_url: String::new(),
            ..AppConfig::default()
        };
        assert_eq!(
            config.validate().unwrap_err(),
            "list proxy mode requires a proxy URL"
        );
        config.proxy_url = "http://127.0.0.1:7890".to_string();
        // validate() must stay self-contained: an empty list or unknown ids are
        // write-gate concerns and must never block the load path.
        config.proxy_list_models = Vec::new();
        config
            .validate()
            .expect("empty list must not block validate");
        config.proxy_list_models = vec!["not-a-known-model".to_string()];
        config
            .validate()
            .expect("unknown list ids must not block validate");

        assert!(normalize_proxy_url(ProxyMode::List, "socks5://127.0.0.1:1080").is_err());
    }

    #[test]
    fn non_list_modes_keep_list_fields_untouched() {
        let config = AppConfig {
            gateway_key: "k".to_string(),
            proxy_list_direction: ProxyListDirection::Blacklist,
            proxy_list_models: vec!["gpt-5.6-luna".to_string(), "grok-4.5".to_string()],
            ..AppConfig::default()
        };
        config
            .validate()
            .expect("auto mode with list leftovers passes");
        assert_eq!(config.proxy_list_direction, ProxyListDirection::Blacklist);
        assert_eq!(config.proxy_list_models.len(), 2);
    }

    #[test]
    fn proxy_mode_and_direction_serde_round_trip() {
        assert_eq!(
            serde_json::to_value(ProxyMode::List).unwrap(),
            serde_json::json!("list")
        );
        assert_eq!(
            serde_json::to_value(ProxyListDirection::Blacklist).unwrap(),
            serde_json::json!("blacklist")
        );
        assert_eq!(
            serde_json::from_value::<ProxyListDirection>(serde_json::json!("whitelist")).unwrap(),
            ProxyListDirection::Whitelist
        );
    }

    #[test]
    fn legacy_config_without_list_fields_loads_with_defaults() {
        let legacy = serde_json::json!({
            "gateway_port": 9042,
            "gateway_key": "ocg-keep",
            "upstream_base_url": "https://opencode.ai/zen/go",
            "proxy_mode": "manual",
            "proxy_url": "http://127.0.0.1:7890",
            "opencode_invite_url": DEFAULT_OPENCODE_INVITE_URL,
            "client_root_url": "",
            "auto_start": false,
            "show_dock_icon": true,
            "connect_timeout_secs": 30,
            "non_stream_timeout_secs": 900,
            "stream_idle_timeout_secs": 300,
            "routing_mode": "strict-priority",
            "conversation_sticky": false,
            "free_model_routing": "explicit",
            "claude_desktop_models": {
                "sonnet": "minimax-m3",
                "opus": "",
                "haiku": ""
            }
        });
        let config: AppConfig = serde_json::from_value(legacy).expect("legacy config loads");
        assert_eq!(config.proxy_list_direction, ProxyListDirection::Whitelist);
        assert!(config.proxy_list_models.is_empty());
        for mode in [ProxyMode::Auto, ProxyMode::Manual, ProxyMode::Direct] {
            let mut legacy_config = config.clone();
            legacy_config.proxy_mode = mode;
            legacy_config
                .validate()
                .expect("legacy three-mode behavior is unchanged");
        }
    }

    #[test]
    fn list_mode_deserialization_fails_loudly_without_serde_other() {
        // D8: no #[serde(other)] fallback — an older binary must fail to start
        // on a "list" config instead of silently routing restricted models
        // directly.
        let encoded = serde_json::json!("list");
        // This build knows the variant, so it decodes; the fail-loud contract
        // is about older builds lacking it, asserted via raw JSON round trip.
        assert_eq!(
            serde_json::from_value::<ProxyMode>(encoded).unwrap(),
            ProxyMode::List
        );
        assert!(serde_json::from_value::<ProxyMode>(serde_json::json!("unknown-mode")).is_err());
    }

    #[test]
    fn persisted_list_with_stale_ids_loads_and_never_matches() {
        // Registry-shrink tolerance: the load path only needs a URL; stale ids
        // and empty lists resolve to "no match" inside the route set (covered
        // by http_client tests), never to a startup failure.
        let config: AppConfig = serde_json::from_value(serde_json::json!({
            "gateway_key": "k",
            "proxy_mode": "list",
            "proxy_url": "http://127.0.0.1:7890",
            "proxy_list_direction": "whitelist",
            "proxy_list_models": ["gpt-5.6-luna", "removed-model"],
            "claude_desktop_models": { "sonnet": "minimax-m3", "opus": "", "haiku": "" }
        }))
        .expect("stale list entries must load");
        config
            .validate()
            .expect("validate only checks the self-contained URL invariant");
        assert!(
            config
                .proxy_list_models
                .contains(&"removed-model".to_string())
        );
    }

    #[test]
    fn default_opencode_invite_url_is_allowlisted() {
        assert_eq!(
            normalize_opencode_invite_url(DEFAULT_OPENCODE_INVITE_URL).unwrap(),
            DEFAULT_OPENCODE_INVITE_URL
        );
        assert_eq!(
            AppConfig::default().opencode_invite_url,
            DEFAULT_OPENCODE_INVITE_URL
        );
    }

    #[test]
    fn opencode_invite_url_is_https_and_host_allowlisted() {
        assert_eq!(normalize_opencode_invite_url("  ").unwrap(), "");
        assert_eq!(
            normalize_opencode_invite_url("https://opencode.ai/invite/test").unwrap(),
            "https://opencode.ai/invite/test"
        );
        assert!(normalize_opencode_invite_url("https://console.opencode.ai/invite?id=1").is_ok());
        for invalid in [
            "http://opencode.ai/invite/test",
            "https://opencode.ai.evil.test/invite",
            "https://user:pass@opencode.ai/invite",
            "https://example.com/invite",
            "not-a-url",
        ] {
            assert!(
                normalize_opencode_invite_url(invalid).is_err(),
                "accepted unsafe invite URL {invalid:?}"
            );
        }
        assert!(
            normalize_opencode_invite_url(&format!("https://opencode.ai/{}", "x".repeat(2049)))
                .is_err()
        );
    }
}
