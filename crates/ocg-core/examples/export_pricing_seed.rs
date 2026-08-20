//! Refreshes the embedded pricing seed from the official OpenCode Go page.
//!
//! `cargo run -p ocg-core --example export_pricing_seed` fetches
//! <https://opencode.ai/docs/go/>, parses it with the exact runtime parser,
//! and writes `src/pricing-seed.json`, which the crate embeds via
//! `include_str!`. Release builds run this before compiling so shipped
//! binaries always carry the latest official table; the committed JSON is
//! the fallback for offline builds. Fails closed: a fetch or parse error
//! aborts without touching the existing seed.

use ocg_core::models::AppConfig;
use ocg_core::pricing::fetch_official_snapshot;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Default config keeps proxy_mode=auto so the export honors the same
    // environment proxy policy as every other outbound request.
    let config = AppConfig::default();
    let snapshot = fetch_official_snapshot(&config).await?;
    let revision = snapshot.revision.clone();
    let document_updated_at = snapshot.document_updated_at.clone();
    let model_count = snapshot.models.len();
    let json = serde_json::to_string_pretty(&snapshot)?;
    let path = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR")?)
        .join("src")
        .join("pricing-seed.json");
    std::fs::write(&path, format!("{json}\n"))?;
    println!(
        "exported {model_count} models (revision {revision}, document updated {document_updated_at}) to {}",
        path.display()
    );
    Ok(())
}
