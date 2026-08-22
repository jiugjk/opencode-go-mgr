//! Live protocol + stability probe against a local OCG data dir.
//!
//! Usage:
//!   cargo run -p ocg-core --example probe_protocols --release -- [data-dir]
//!
//! Decrypts the first enabled account key, lists upstream models, then for each
//! paid Go model (plus official IDs missing from /v1/models):
//!   1. Chat / Responses / Messages × non-stream / stream one-shot
//!   2. Official preferred endpoint: 5-turn conversation, non-stream then stream
//!
//! Free / pickle models get the one-shot matrix only.

use std::collections::BTreeMap;
use std::fmt::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use ocg_core::crypto::MachineBoundCipher;
use ocg_core::db::Database;
use ocg_core::gateway::free_models::is_free_model;
use ocg_core::state::CoreStateInner;
use serde_json::{Value, json};

const REQUEST_GAP: Duration = Duration::from_millis(400);
const PING_TIMEOUT: Duration = Duration::from_secs(120);
const TURN_TIMEOUT: Duration = Duration::from_secs(180);
const PING_MAX_TOKENS: u32 = 8;
const TURN_MAX_TOKENS: u32 = 80;

const OFFICIAL_PREFERRED: &[(&str, Endpoint)] = &[
    ("grok-4.5", Endpoint::Responses),
    ("gpt-5.6-luna", Endpoint::Responses),
    ("muse-spark-1.2", Endpoint::Responses),
    ("muse-spark-1.2-contributor", Endpoint::Responses),
    ("glm-5.3", Endpoint::Chat),
    ("glm-5.2", Endpoint::Chat),
    ("glm-5.1", Endpoint::Chat),
    ("kimi-k3", Endpoint::Chat),
    ("kimi-k2.7-code", Endpoint::Chat),
    ("kimi-k2.6", Endpoint::Chat),
    ("deepseek-v4-pro", Endpoint::Chat),
    ("deepseek-v4-flash", Endpoint::Chat),
    ("mimo-v2.5", Endpoint::Chat),
    ("mimo-v2.5-pro", Endpoint::Chat),
    ("minimax-m3", Endpoint::Messages),
    ("minimax-m2.7", Endpoint::Messages),
    ("minimax-m2.5", Endpoint::Messages),
    ("qwen3.8-max", Endpoint::Messages),
    ("qwen3.7-max", Endpoint::Messages),
    ("qwen3.7-plus", Endpoint::Messages),
    ("qwen3.6-plus", Endpoint::Messages),
    ("hy3", Endpoint::Chat),
    ("ox-alpha-free", Endpoint::Chat),
];

const CONVERSATION: &[&str] = &[
    "Reply with exactly: ALPHA",
    "Repeat your previous reply and append -1",
    "Repeat your previous reply and append -2",
    "Repeat your previous reply and append -3",
    "Repeat your previous reply and append -4",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Endpoint {
    Chat,
    Responses,
    Messages,
}

impl Endpoint {
    fn label(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Responses => "resp",
            Self::Messages => "msg",
        }
    }

    fn all() -> [Self; 3] {
        [Self::Chat, Self::Responses, Self::Messages]
    }
}

#[derive(Clone, Debug)]
struct ProbeResult {
    status: u16,
    ms: u128,
    ok: bool,
    text: String,
    note: String,
}

impl ProbeResult {
    fn cell(&self) -> String {
        if self.status == 0 {
            "ERR".into()
        } else if self.ok {
            "OK".into()
        } else {
            self.status.to_string()
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let data_dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var("USERPROFILE")
                .or_else(|_| std::env::var("HOME"))
                .expect("home");
            PathBuf::from(home).join(".ocg-mgr")
        });

    let cipher = Arc::new(MachineBoundCipher::new());
    let db = Database::open(data_dir.clone())?;
    let state = CoreStateInner::new(db, data_dir, cipher)?;
    let (config, client) = state.upstream_context();
    let base = config.upstream_base_url.trim_end_matches('/').to_string();

    let wanted = std::env::args().nth(2);
    let mut keys = Vec::new();
    for account in state
        .db
        .lock()
        .list_accounts()?
        .into_iter()
        .filter(|a| a.enabled)
    {
        if wanted
            .as_deref()
            .is_some_and(|want| !account.name.contains(want) && !account.id.starts_with(want))
        {
            continue;
        }
        match state.decrypt_key(&account.key_cipher) {
            Ok(key) => keys.push((account.name.clone(), key)),
            Err(error) => eprintln!("skip account {}: {error}", account.name),
        }
    }
    if keys.is_empty() {
        anyhow::bail!("no enabled accounts available");
    }
    keys.sort_by_key(|(name, _)| match name.as_str() {
        "115" => 0,
        "klarkxy01" => 1,
        _ => 2,
    });
    println!(
        "probe accounts={} upstream={base}",
        keys.iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>()
            .join(",")
    );
    let mut key_index = 0usize;

    let models_url = format!("{base}/v1/models");
    let models_resp = client
        .get(&models_url)
        .header("Authorization", format!("Bearer {}", keys[0].1))
        .timeout(Duration::from_secs(60))
        .send()
        .await?;
    let models_status = models_resp.status();
    let models_body = models_resp.text().await?;
    if !models_status.is_success() {
        anyhow::bail!("GET /v1/models failed: {models_status} {models_body}");
    }
    let models_json: Value = serde_json::from_str(&models_body)?;
    let mut listed = models_json
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|m| m.get("id").and_then(Value::as_str).map(str::to_owned))
        .collect::<Vec<_>>();
    listed.sort();
    listed.dedup();
    println!("upstream models ({}): {}", listed.len(), listed.join(", "));

    let mut models = listed.clone();
    for (id, _) in OFFICIAL_PREFERRED {
        if !models.iter().any(|m| m == id) {
            models.push((*id).to_string());
        }
    }
    models.sort();
    models.dedup();

    println!();
    println!(
        "{:<22} {:<9} {:<9} {:<9} {:<9} {:<9} {:<9}",
        "model", "c", "c~", "r", "r~", "m", "m~"
    );
    println!("{}", "-".repeat(88));

    let mut report = String::new();
    let mut supported: BTreeMap<String, Vec<Endpoint>> = BTreeMap::new();

    for model in &models {
        let mut row = BTreeMap::new();
        for endpoint in Endpoint::all() {
            for stream in [false, true] {
                let result = probe_with_failover(
                    &client,
                    &base,
                    &keys,
                    &mut key_index,
                    model,
                    endpoint,
                    stream,
                    None,
                    PING_TIMEOUT,
                )
                .await;
                print_progress(model, endpoint, stream, &result);
                row.insert((endpoint, stream), result);
                tokio::time::sleep(REQUEST_GAP).await;
            }
        }

        let oneshots = [
            row.get(&(Endpoint::Chat, false)).cloned().unwrap(),
            row.get(&(Endpoint::Chat, true)).cloned().unwrap(),
            row.get(&(Endpoint::Responses, false)).cloned().unwrap(),
            row.get(&(Endpoint::Responses, true)).cloned().unwrap(),
            row.get(&(Endpoint::Messages, false)).cloned().unwrap(),
            row.get(&(Endpoint::Messages, true)).cloned().unwrap(),
        ];
        println!(
            "{:<22} {:<9} {:<9} {:<9} {:<9} {:<9} {:<9}",
            model,
            oneshots[0].cell(),
            oneshots[1].cell(),
            oneshots[2].cell(),
            oneshots[3].cell(),
            oneshots[4].cell(),
            oneshots[5].cell(),
        );

        let mut ok_endpoints = Vec::new();
        for endpoint in Endpoint::all() {
            let ns = row.get(&(endpoint, false)).unwrap();
            let st = row.get(&(endpoint, true)).unwrap();
            if ns.ok && st.ok {
                ok_endpoints.push(endpoint);
            }
        }
        supported.insert(model.clone(), ok_endpoints);

        writeln!(
            report,
            "## {model}\noneshot chat={} chat_stream={} resp={} resp_stream={} msg={} msg_stream={}",
            detail(&oneshots[0]),
            detail(&oneshots[1]),
            detail(&oneshots[2]),
            detail(&oneshots[3]),
            detail(&oneshots[4]),
            detail(&oneshots[5]),
        )
        .unwrap();

        if is_free_or_pickle(model) {
            writeln!(report, "skip 5-turn (free/pickle)\n").unwrap();
            continue;
        }

        let preferred = official_preferred(model).unwrap_or_else(|| {
            row.iter()
                .find(|((_, stream), result)| !*stream && result.ok)
                .map(|((endpoint, _), _)| *endpoint)
                .unwrap_or(Endpoint::Chat)
        });
        let preferred_ready = row.get(&(preferred, false)).is_some_and(|result| result.ok)
            || row.get(&(preferred, true)).is_some_and(|result| result.ok);
        if !preferred_ready {
            writeln!(
                report,
                "skip 5-turn (preferred {} not usable)\n",
                preferred.label()
            )
            .unwrap();
            println!("  skip 5-turn (preferred {} not usable)", preferred.label());
            continue;
        }

        for stream in [false, true] {
            let conversation = probe_conversation_with_failover(
                &client,
                &base,
                &keys,
                &mut key_index,
                model,
                preferred,
                stream,
            )
            .await;
            let ok_turns = conversation.iter().filter(|turn| turn.ok).count();
            let stable = conversation.len() == CONVERSATION.len()
                && conversation.iter().all(|turn| turn.ok)
                && conversation_looks_stable(&conversation);
            writeln!(
                report,
                "5-turn {} preferred={} ok={}/{} stable={} {}",
                if stream { "stream" } else { "sync" },
                preferred.label(),
                ok_turns,
                CONVERSATION.len(),
                stable,
                conversation
                    .iter()
                    .enumerate()
                    .map(|(i, turn)| format!("t{}:{}", i + 1, detail(turn)))
                    .collect::<Vec<_>>()
                    .join(" | ")
            )
            .unwrap();
            println!(
                "  5-turn {}/{} {}/{} stable={}",
                preferred.label(),
                if stream { "stream" } else { "sync" },
                ok_turns,
                CONVERSATION.len(),
                stable
            );
        }
        writeln!(report).unwrap();
    }

    println!();
    println!("supported = both stream and non-stream 2xx with a usable body");
    for (model, endpoints) in &supported {
        let cells = endpoints
            .iter()
            .map(|endpoint| endpoint.label())
            .collect::<Vec<_>>()
            .join(",");
        println!("  {model}: {}", if cells.is_empty() { "-" } else { &cells });
    }

    let out = std::env::temp_dir().join("ocg-protocol-probe.md");
    std::fs::write(&out, report)?;
    println!("\nfull notes: {}", out.display());
    Ok(())
}

fn official_preferred(model: &str) -> Option<Endpoint> {
    OFFICIAL_PREFERRED
        .iter()
        .find(|(id, _)| *id == model)
        .map(|(_, endpoint)| *endpoint)
}

fn is_free_or_pickle(model: &str) -> bool {
    is_free_model(model)
}

fn detail(result: &ProbeResult) -> String {
    if result.ok {
        format!("OK {}ms", result.ms)
    } else if result.status == 0 {
        format!("ERR {}", clip(&result.note, 80))
    } else {
        format!("{} {}", result.status, clip(&result.note, 80))
    }
}

fn clip(value: &str, n: usize) -> String {
    let flat = value.replace('\n', " ");
    flat.chars().take(n).collect()
}

fn print_progress(model: &str, endpoint: Endpoint, stream: bool, result: &ProbeResult) {
    eprintln!(
        "  … {model} {}{} => {}",
        endpoint.label(),
        if stream { "~" } else { "" },
        detail(result)
    );
}

fn conversation_looks_stable(turns: &[ProbeResult]) -> bool {
    let Some(first) = turns.first().and_then(|turn| extract_marker(&turn.text)) else {
        return false;
    };
    turns.iter().all(|turn| {
        extract_marker(&turn.text)
            .map(|text| text.starts_with(&first))
            .unwrap_or(false)
    })
}

fn extract_marker(text: &str) -> Option<String> {
    let upper = text.to_ascii_uppercase();
    let start = upper.find("ALPHA")?;
    Some(
        text[start..]
            .chars()
            .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '-')
            .take(24)
            .collect(),
    )
}

#[allow(clippy::too_many_arguments)]
async fn probe_with_failover(
    client: &reqwest::Client,
    base: &str,
    keys: &[(String, String)],
    key_index: &mut usize,
    model: &str,
    endpoint: Endpoint,
    stream: bool,
    history: Option<&[(String, String)]>,
    timeout: Duration,
) -> ProbeResult {
    for attempt in 0..keys.len() {
        let index = (*key_index + attempt) % keys.len();
        let body = if let Some(history) = history {
            conversation_body(
                model,
                endpoint,
                stream,
                history,
                CONVERSATION
                    .get(history.len())
                    .copied()
                    .unwrap_or(CONVERSATION[CONVERSATION.len() - 1]),
            )
        } else {
            oneshot_body(model, endpoint, stream)
        };
        let result = send(
            client,
            base,
            &keys[index].1,
            endpoint,
            body,
            stream,
            timeout,
        )
        .await;
        if is_usage_limit(&result) {
            eprintln!("  account {} hit Go usage limit, rotating", keys[index].0);
            *key_index = (index + 1) % keys.len();
            continue;
        }
        *key_index = index;
        return result;
    }
    ProbeResult {
        status: 429,
        ms: 0,
        ok: false,
        text: String::new(),
        note: "all accounts hit Go usage limit".into(),
    }
}

fn is_usage_limit(result: &ProbeResult) -> bool {
    result.status == 429 && result.note.contains("GoUsageLimitError")
}

async fn probe_conversation_with_failover(
    client: &reqwest::Client,
    base: &str,
    keys: &[(String, String)],
    key_index: &mut usize,
    model: &str,
    endpoint: Endpoint,
    stream: bool,
) -> Vec<ProbeResult> {
    let mut history: Vec<(String, String)> = Vec::new();
    let mut results = Vec::new();
    for _user in CONVERSATION {
        let result = probe_with_failover(
            client,
            base,
            keys,
            key_index,
            model,
            endpoint,
            stream,
            Some(&history),
            TURN_TIMEOUT,
        )
        .await;
        if result.ok && !result.text.trim().is_empty() {
            history.push((CONVERSATION[history.len()].to_string(), result.text.clone()));
        }
        results.push(result);
        tokio::time::sleep(REQUEST_GAP).await;
    }
    results
}

fn oneshot_body(model: &str, endpoint: Endpoint, stream: bool) -> Value {
    match endpoint {
        Endpoint::Chat => json!({
            "model": model,
            "messages": [{"role": "user", "content": "Reply with exactly: PING"}],
            "max_tokens": PING_MAX_TOKENS,
            "stream": stream
        }),
        Endpoint::Responses => json!({
            "model": model,
            "input": "Reply with exactly: PING",
            "store": false,
            "max_output_tokens": PING_MAX_TOKENS,
            "stream": stream
        }),
        Endpoint::Messages => json!({
            "model": model,
            "messages": [{"role": "user", "content": "Reply with exactly: PING"}],
            "max_tokens": PING_MAX_TOKENS,
            "stream": stream
        }),
    }
}

fn conversation_body(
    model: &str,
    endpoint: Endpoint,
    stream: bool,
    history: &[(String, String)],
    user: &str,
) -> Value {
    match endpoint {
        Endpoint::Chat => {
            let mut messages = Vec::new();
            for (prev_user, prev_assistant) in history {
                messages.push(json!({"role": "user", "content": prev_user}));
                messages.push(json!({"role": "assistant", "content": prev_assistant}));
            }
            messages.push(json!({"role": "user", "content": user}));
            json!({
                "model": model,
                "messages": messages,
                "max_tokens": TURN_MAX_TOKENS,
                "stream": stream
            })
        }
        Endpoint::Responses => {
            let mut input = Vec::new();
            for (prev_user, prev_assistant) in history {
                input.push(json!({
                    "role": "user",
                    "content": [{"type": "input_text", "text": prev_user}]
                }));
                input.push(json!({
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": prev_assistant}]
                }));
            }
            input.push(json!({
                "role": "user",
                "content": [{"type": "input_text", "text": user}]
            }));
            json!({
                "model": model,
                "input": input,
                "store": false,
                "max_output_tokens": TURN_MAX_TOKENS,
                "stream": stream
            })
        }
        Endpoint::Messages => {
            let mut messages = Vec::new();
            for (prev_user, prev_assistant) in history {
                messages.push(json!({"role": "user", "content": prev_user}));
                messages.push(json!({"role": "assistant", "content": prev_assistant}));
            }
            messages.push(json!({"role": "user", "content": user}));
            json!({
                "model": model,
                "messages": messages,
                "max_tokens": TURN_MAX_TOKENS,
                "stream": stream
            })
        }
    }
}

async fn send(
    client: &reqwest::Client,
    base: &str,
    key: &str,
    endpoint: Endpoint,
    body: Value,
    stream: bool,
    timeout: Duration,
) -> ProbeResult {
    let url = match endpoint {
        Endpoint::Chat => format!("{base}/v1/chat/completions"),
        Endpoint::Responses => format!("{base}/v1/responses"),
        Endpoint::Messages => format!("{base}/v1/messages"),
    };
    let started = Instant::now();
    let mut req = client
        .post(&url)
        .timeout(timeout)
        .header("Content-Type", "application/json")
        .json(&body);
    req = match endpoint {
        Endpoint::Messages => req
            .header("x-api-key", key)
            .header("anthropic-version", "2023-06-01"),
        _ => req.header("Authorization", format!("Bearer {key}")),
    };

    match req.send().await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            if stream {
                finish_stream(resp, status, started, endpoint).await
            } else {
                finish_json(resp, status, started, endpoint).await
            }
        }
        Err(error) => ProbeResult {
            status: 0,
            ms: started.elapsed().as_millis(),
            ok: false,
            text: String::new(),
            note: error.to_string(),
        },
    }
}

async fn finish_json(
    resp: reqwest::Response,
    status: u16,
    started: Instant,
    endpoint: Endpoint,
) -> ProbeResult {
    let text = resp.text().await.unwrap_or_default();
    let parsed: Result<Value, _> = serde_json::from_str(&text);
    let extracted = parsed
        .as_ref()
        .ok()
        .map(|value| extract_text(endpoint, value))
        .unwrap_or_default();
    let ok = status < 300 && parsed.as_ref().is_ok_and(|value| json_ok(endpoint, value));
    ProbeResult {
        status,
        ms: started.elapsed().as_millis(),
        ok,
        text: extracted,
        note: if ok { "ok".into() } else { clip(&text, 160) },
    }
}

async fn finish_stream(
    resp: reqwest::Response,
    status: u16,
    started: Instant,
    endpoint: Endpoint,
) -> ProbeResult {
    if status >= 300 {
        let text = resp.text().await.unwrap_or_default();
        return ProbeResult {
            status,
            ms: started.elapsed().as_millis(),
            ok: false,
            text: String::new(),
            note: clip(&text, 160),
        };
    }

    let mut raw = String::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(bytes) => raw.push_str(&String::from_utf8_lossy(&bytes)),
            Err(error) => {
                return ProbeResult {
                    status: 0,
                    ms: started.elapsed().as_millis(),
                    ok: false,
                    text: String::new(),
                    note: error.to_string(),
                };
            }
        }
    }

    let (text, saw_terminal) = parse_sse(endpoint, &raw);
    let ok = !text.trim().is_empty() || saw_terminal;
    ProbeResult {
        status,
        ms: started.elapsed().as_millis(),
        ok,
        text,
        note: if ok { "ok".into() } else { clip(&raw, 160) },
    }
}

fn json_ok(endpoint: Endpoint, value: &Value) -> bool {
    match endpoint {
        Endpoint::Chat => {
            value.get("object").and_then(Value::as_str) == Some("chat.completion")
                || value.pointer("/choices/0/message").is_some()
        }
        Endpoint::Responses => {
            value.get("object").and_then(Value::as_str) == Some("response")
                || value.get("output").is_some()
        }
        Endpoint::Messages => {
            value.get("type").and_then(Value::as_str) == Some("message")
                || value.get("content").and_then(Value::as_array).is_some()
        }
    }
}

fn extract_text(endpoint: Endpoint, value: &Value) -> String {
    match endpoint {
        Endpoint::Chat => value
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        Endpoint::Responses => {
            if let Some(text) = value.get("output_text").and_then(Value::as_str) {
                return text.to_string();
            }
            value
                .get("output")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .flat_map(|item| item.get("content").and_then(Value::as_array))
                .flatten()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("")
        }
        Endpoint::Messages => value
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
    }
}

fn parse_sse(endpoint: Endpoint, raw: &str) -> (String, bool) {
    let mut text = String::new();
    let mut terminal = false;
    for block in raw.split("\n\n") {
        let data = block
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(str::trim)
            .collect::<Vec<_>>()
            .join("");
        if data.is_empty() || data == "[DONE]" {
            if data == "[DONE]" {
                terminal = true;
            }
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&data) else {
            continue;
        };
        match endpoint {
            Endpoint::Chat => {
                if let Some(delta) = value
                    .pointer("/choices/0/delta/content")
                    .and_then(Value::as_str)
                {
                    text.push_str(delta);
                }
                if value
                    .pointer("/choices/0/finish_reason")
                    .and_then(Value::as_str)
                    .is_some()
                {
                    terminal = true;
                }
            }
            Endpoint::Responses => {
                let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
                if kind.ends_with("output_text.delta") {
                    if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                        text.push_str(delta);
                    }
                }
                if kind == "response.completed" || kind == "response.output_text.done" {
                    terminal = true;
                    if text.is_empty() {
                        text.push_str(&extract_text(Endpoint::Responses, &value));
                    }
                }
            }
            Endpoint::Messages => {
                let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
                if kind == "content_block_delta" {
                    if let Some(delta) = value.pointer("/delta/text").and_then(Value::as_str) {
                        text.push_str(delta);
                    }
                }
                if kind == "message_stop" {
                    terminal = true;
                }
            }
        }
    }
    (text, terminal)
}
