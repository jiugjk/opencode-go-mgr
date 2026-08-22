use crate::models::{AppConfig, ProxyListDirection, ProxyMode};
use crate::pricing::normalize_model_name;
use std::time::Duration;

/// Closed-set route leg label recorded on every forward log row. Carries no
/// URL or credential material by construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RouteLabel {
    Auto,
    Proxy,
    Direct,
}

impl RouteLabel {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            RouteLabel::Auto => "auto",
            RouteLabel::Proxy => "proxy",
            RouteLabel::Direct => "direct",
        }
    }
}

/// One atomic routing unit: the routing metadata and both leg clients are
/// generated from the same `AppConfig` generation, so a snapshot held by an
/// in-flight request can never mix new metadata with old clients. Non-list
/// modes keep `exception_client` unset and always resolve to the default leg.
pub(crate) struct ForwardRouteSet {
    /// Registry ids normalized once at build time; empty or stale entries
    /// simply never match (total function over any persisted shape).
    list: Vec<String>,
    default_client: reqwest::Client,
    exception_client: Option<reqwest::Client>,
    default_label: RouteLabel,
    exception_label: RouteLabel,
}

impl ForwardRouteSet {
    /// Pure, lock-free route resolution for one forwarding attempt.
    pub(crate) fn client_for(&self, model: &str) -> (&reqwest::Client, RouteLabel) {
        match &self.exception_client {
            None => (&self.default_client, self.default_label),
            Some(exception) => {
                if is_listed(&self.list, model) {
                    (exception, self.exception_label)
                } else {
                    (&self.default_client, self.default_label)
                }
            }
        }
    }

    /// The default leg client used by non-model-scoped outbound callers
    /// (`upstream_context` and friends).
    pub(crate) fn default_client(&self) -> &reqwest::Client {
        &self.default_client
    }
}

/// Registry ids normalized once at build time; empty or stale entries
/// simply never match (total function over any persisted shape).
///
/// Stale entries — ids a newer registry removed — are dropped here so they
/// stay inert even if a client explicitly requests that exact id, matching the
/// load-path tolerance contract ("removed entries match nothing").
fn normalized_known_list(models: &[String]) -> Vec<String> {
    let known: std::collections::HashSet<String> = crate::gateway::protocol::supported_model_ids()
        .map(normalize_model_name)
        .collect();
    models
        .iter()
        .map(|model| normalize_model_name(model))
        .filter(|model| known.contains(model))
        .collect()
}

fn is_listed(list: &[String], model: &str) -> bool {
    list.contains(&normalize_model_name(model))
}

fn tuned(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    // Drop idle pooled connections earlier than the default so a stale connection
    // closed by the upstream/CDN isn't reused. Keep-alive probes further reduce
    // silent drops for long-lived gateways.
    builder
        .pool_idle_timeout(Duration::from_secs(30))
        .tcp_keepalive(Duration::from_secs(30))
}

fn direct_leg_builder() -> reqwest::ClientBuilder {
    tuned(reqwest::Client::builder().no_proxy())
}

fn proxy_leg_builder(url: &str) -> crate::Result<reqwest::ClientBuilder> {
    Ok(tuned(
        reqwest::Client::builder().proxy(reqwest::Proxy::all(url)?),
    ))
}

fn leg_client(
    builder: reqwest::ClientBuilder,
    config: &AppConfig,
) -> crate::Result<reqwest::Client> {
    Ok(builder
        .connect_timeout(Duration::from_secs(config.connect_timeout_secs))
        .build()?)
}

/// Applies the process-wide outbound proxy policy while leaving callers free to
/// choose their own redirect, total-timeout, and response-size policies. Under
/// list mode this builds the direction's default leg: whitelist default is
/// direct, blacklist default is the manual proxy URL.
pub(crate) fn configured_builder(config: &AppConfig) -> crate::Result<reqwest::ClientBuilder> {
    let builder = match config.proxy_mode {
        ProxyMode::Auto => tuned(reqwest::Client::builder()),
        ProxyMode::Manual => proxy_leg_builder(&config.proxy_url)?,
        ProxyMode::Direct => direct_leg_builder(),
        ProxyMode::List => match config.proxy_list_direction {
            ProxyListDirection::Whitelist => direct_leg_builder(),
            ProxyListDirection::Blacklist => proxy_leg_builder(&config.proxy_url)?,
        },
    };
    Ok(builder)
}

pub(crate) fn build(config: &AppConfig) -> crate::Result<reqwest::Client> {
    leg_client(configured_builder(config)?, config)
}

/// Builds the full route set from one config generation. List mode builds both
/// legs; every other mode builds exactly the process-wide client.
pub(crate) fn build_route_set(config: &AppConfig) -> crate::Result<ForwardRouteSet> {
    match config.proxy_mode {
        ProxyMode::List => {
            let (default_builder, exception_builder, default_label, exception_label) =
                match config.proxy_list_direction {
                    ProxyListDirection::Whitelist => (
                        direct_leg_builder(),
                        proxy_leg_builder(&config.proxy_url)?,
                        RouteLabel::Direct,
                        RouteLabel::Proxy,
                    ),
                    ProxyListDirection::Blacklist => (
                        proxy_leg_builder(&config.proxy_url)?,
                        direct_leg_builder(),
                        RouteLabel::Proxy,
                        RouteLabel::Direct,
                    ),
                };
            Ok(ForwardRouteSet {
                list: normalized_known_list(&config.proxy_list_models),
                default_client: leg_client(default_builder, config)?,
                exception_client: Some(leg_client(exception_builder, config)?),
                default_label,
                exception_label,
            })
        }
        mode => Ok(ForwardRouteSet {
            list: Vec::new(),
            default_client: build(config)?,
            exception_client: None,
            default_label: match mode {
                ProxyMode::Auto => RouteLabel::Auto,
                ProxyMode::Manual => RouteLabel::Proxy,
                ProxyMode::Direct => RouteLabel::Direct,
                ProxyMode::List => unreachable!("list handled above"),
            },
            exception_label: RouteLabel::Direct,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list_config(direction: ProxyListDirection, models: &[&str]) -> AppConfig {
        AppConfig {
            gateway_key: "k".to_string(),
            proxy_mode: ProxyMode::List,
            proxy_url: "http://127.0.0.1:7890".to_string(),
            proxy_list_direction: direction,
            proxy_list_models: models.iter().map(|model| model.to_string()).collect(),
            ..AppConfig::default()
        }
    }

    #[test]
    fn client_for_resolves_both_directions_and_tolerates_stale_entries() {
        // Whitelist: listed -> proxy leg, unlisted/unknown/stale -> direct leg.
        let whitelist = build_route_set(&list_config(
            ProxyListDirection::Whitelist,
            &["gpt-5.6-luna", "removed-model"],
        ))
        .unwrap();
        assert_eq!(whitelist.client_for("gpt-5.6-luna").1, RouteLabel::Proxy);
        assert_eq!(
            whitelist.client_for("GPT_5.6 LUNA").1,
            RouteLabel::Proxy,
            "matching must follow normalize_model_name"
        );
        assert_eq!(whitelist.client_for("glm-5.3").1, RouteLabel::Direct);
        assert_eq!(
            whitelist.client_for("removed-model").1,
            RouteLabel::Direct,
            "stale ids stored in old configs never match"
        );

        // Blacklist inverts both legs.
        let blacklist =
            build_route_set(&list_config(ProxyListDirection::Blacklist, &["grok-4.5"])).unwrap();
        assert_eq!(blacklist.client_for("grok-4.5").1, RouteLabel::Direct);
        assert_eq!(blacklist.client_for("glm-5.3").1, RouteLabel::Proxy);

        // Empty list: whitelist = all direct, blacklist = all proxy.
        let empty_whitelist =
            build_route_set(&list_config(ProxyListDirection::Whitelist, &[])).unwrap();
        assert_eq!(
            empty_whitelist.client_for("gpt-5.6-luna").1,
            RouteLabel::Direct
        );
        let empty_blacklist =
            build_route_set(&list_config(ProxyListDirection::Blacklist, &[])).unwrap();
        assert_eq!(
            empty_blacklist.client_for("gpt-5.6-luna").1,
            RouteLabel::Proxy
        );

        // Non-list modes always resolve to the process-wide leg.
        for (mode, label) in [
            (ProxyMode::Auto, RouteLabel::Auto),
            (ProxyMode::Manual, RouteLabel::Proxy),
            (ProxyMode::Direct, RouteLabel::Direct),
        ] {
            let config = AppConfig {
                gateway_key: "k".to_string(),
                proxy_mode: mode,
                proxy_url: "http://127.0.0.1:7890".to_string(),
                ..AppConfig::default()
            };
            let route_set = build_route_set(&config).unwrap();
            assert_eq!(route_set.client_for("gpt-5.6-luna").1, label);
        }
    }

    #[tokio::test]
    async fn list_mode_builds_the_direction_default_leg_into_configured_builder() {
        // A tiny upstream that accepts one request and answers 204.
        async fn spawn_upstream() -> std::net::SocketAddr {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                if let Ok((mut stream, _)) = listener.accept().await {
                    let mut buffer = vec![0_u8; 4096];
                    let _ = stream.read(&mut buffer).await;
                    let _ = stream
                        .write_all(
                            b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await;
                }
            });
            address
        }

        let upstream = spawn_upstream().await;
        // Blacklist default leg goes through an unreachable proxy URL and must
        // fail instead of silently connecting to the reachable upstream.
        let closed_proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let closed_proxy_url = format!("http://{}", closed_proxy.local_addr().unwrap());
        drop(closed_proxy);

        let mut blacklist = list_config(ProxyListDirection::Blacklist, &[]);
        blacklist.proxy_url = closed_proxy_url;
        let client = crate::http_client::build(&blacklist).unwrap();
        let error = client
            .get(format!("http://{upstream}"))
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .expect_err("blacklist default leg must route through the proxy URL");
        assert!(error.is_connect() || error.is_request(), "{error}");

        // Whitelist default leg connects directly: reachable upstream answers.
        let upstream = spawn_upstream().await;
        let whitelist = list_config(ProxyListDirection::Whitelist, &[]);
        let client = crate::http_client::build(&whitelist).unwrap();
        let response = client
            .get(format!("http://{upstream}"))
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .expect("whitelist default leg must connect directly")
            .status();
        assert_eq!(response.as_u16(), 204);
    }
}
