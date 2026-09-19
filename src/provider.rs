use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use chrono::{DateTime, Utc};
use reqwest::Client;

use crate::config::{effective_provider, AppConfig, ProviderConfig};

#[derive(Debug, Clone)]
pub struct AlertStatus {
    pub active: bool,
    pub checked_at: DateTime<Utc>,
    pub matched_uids: Vec<u32>,
}

#[derive(Debug, Clone)]
pub struct FetchResult {
    pub status: AlertStatus,
    pub next_poll_seconds: f64,
}

#[derive(Clone)]
pub struct AlertsProvider {
    config: Arc<AppConfig>,
    client: Client,
}

impl AlertsProvider {
    pub fn new(config: Arc<AppConfig>) -> Result<Self> {
        Ok(Self {
            config,
            client: Client::builder().build()?,
        })
    }

    pub fn current_config(&self) -> Result<ProviderConfig> {
        effective_provider(&self.config.provider)
    }

    pub fn token_configured(&self) -> Result<bool> {
        Ok(self.current_config()?.resolve_token().is_ok())
    }

    pub async fn fetch(&self) -> Result<FetchResult> {
        let config = self.current_config()?;
        let token = config.resolve_token()?;
        let response = self
            .client
            .get(config.status_endpoint())
            .header("Authorization", format!("Bearer {token}"))
            .header("Accept", "application/json")
            .timeout(Duration::from_secs_f64(config.request_timeout_seconds))
            .send()
            .await
            .map_err(|e| anyhow!("не вдалося отримати статус тривоги: {e}"))?;

        let http_status = response.status();
        if !http_status.is_success() {
            let body = response.text().await.unwrap_or_default();
            let body = body.chars().take(300).collect::<String>();
            let next = if http_status.as_u16() == 429 {
                config.rate_limit_backoff_seconds
            } else {
                config.poll_interval_seconds
            };
            bail!(
                "alerts.in.ua повернув HTTP {}: {} [next_poll={next}]",
                http_status.as_u16(),
                body
            );
        }

        let payload = response
            .json::<String>()
            .await
            .map_err(|e| anyhow!("alerts.in.ua повернув некоректну JSON-відповідь: {e}"))?;
        if !matches!(payload.as_str(), "A" | "P" | "N") {
            bail!("alerts.in.ua повернув невідомий статус тривоги");
        }
        let active = payload == "A" || (payload == "P" && config.partial_status_is_active());
        Ok(FetchResult {
            status: AlertStatus {
                active,
                checked_at: Utc::now(),
                matched_uids: if active {
                    vec![config.location_uid]
                } else {
                    Vec::new()
                },
            },
            next_poll_seconds: config.poll_interval_seconds,
        })
    }
}
