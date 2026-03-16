use anyhow::{anyhow, Context, Result};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use chrono::Utc;
use rand::thread_rng;
use reqwest::{Client, StatusCode};
use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::pkcs8::DecodePrivateKey;
use rsa::pss::SigningKey;
use rsa::signature::{RandomizedSigner, SignatureEncoding};
use rsa::RsaPrivateKey;
use serde::Serialize;
use sha2::Sha256;
use std::sync::Arc;
use std::time::Duration;

use crate::config::KalshiConfig;
use crate::models::{
    BalanceResponse, EventsResponse, ExchangeStatusResponse, KalshiBalance, KalshiEvent,
    KalshiMarket, KalshiOrder, KalshiOrderBook, KalshiOrderRequest, KalshiPosition,
    MarketResponse, MarketsResponse, OrderBookResponse, OrderResponse, PositionsResponse,
};

// ---------------------------------------------------------------------------
// KalshiApiClient
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct KalshiApiClient {
    client: Client,
    base_url: String,
    api_key_id: Option<String>,
    /// Pre-built signing key; shared cheaply via Arc to avoid per-request key allocation.
    signing_key: Option<Arc<SigningKey<Sha256>>>,
}

impl KalshiApiClient {
    /// Create a new API client from the given configuration.
    pub fn new(cfg: &KalshiConfig) -> Result<Self> {
        let client = Client::builder()
            .use_rustls_tls()
            .timeout(Duration::from_secs(5))
            .pool_idle_timeout(Duration::from_secs(30))
            .pool_max_idle_per_host(4)
            .tcp_keepalive(Duration::from_secs(15))
            .build()
            .context("Failed to build HTTP client")?;

        let signing_key = if let Some(path) = &cfg.private_key_path {
            let pem = std::fs::read_to_string(path)
                .with_context(|| format!("Failed to read private key at '{path}'"))?;
            let rsa_key = RsaPrivateKey::from_pkcs8_pem(&pem)
                .or_else(|_| RsaPrivateKey::from_pkcs1_pem(&pem))
                .context("Failed to parse RSA private key (expected PKCS#8 or PKCS#1 PEM)")?;
            Some(Arc::new(SigningKey::<Sha256>::new(rsa_key)))
        } else {
            None
        };

        Ok(KalshiApiClient {
            client,
            base_url: cfg.api_base_url.trim_end_matches('/').to_string(),
            api_key_id: cfg.api_key_id.clone(),
            signing_key,
        })
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    /// Build the three Kalshi auth headers for a single request.
    /// Returns `(api_key, timestamp_ms, signature)`.
    ///
    /// `path` must be the bare API path **without** query parameters.
    /// Kalshi's signature spec covers only `{timestamp}{METHOD}{path}`;
    /// query parameters are sent in the URL but are not part of the signed message.
    fn sign_request(&self, method: &str, path: &str) -> Result<(String, String, String)> {
        let api_key = self
            .api_key_id
            .as_ref()
            .ok_or_else(|| anyhow!("api_key_id is not configured"))?;
        let signing_key = self
            .signing_key
            .as_ref()
            .ok_or_else(|| anyhow!("private_key_path is not configured"))?;

        let timestamp_ms = Utc::now().timestamp_millis().to_string();

        // Signed message: <timestamp_ms><METHOD><path>
        let message = format!("{}{}{}", timestamp_ms, method, path);

        let mut rng = thread_rng();
        let signature = signing_key.sign_with_rng(&mut rng, message.as_bytes());

        let sig_b64 = BASE64.encode(signature.to_bytes().as_ref());

        Ok((api_key.clone(), timestamp_ms, sig_b64))
    }

    /// Returns true if credentials are fully configured.
    pub fn has_credentials(&self) -> bool {
        self.api_key_id.is_some() && self.signing_key.is_some()
    }

    /// Generate authentication headers for WebSocket handshake.
    /// Returns (api_key, timestamp_ms, signature) for the WS endpoint.
    pub fn ws_auth_headers(&self) -> Result<(String, String, String)> {
        self.sign_request("GET", "/trade-api/ws/v2")
    }

    /// Returns the base URL configured for this client.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    async fn get<T: for<'de> serde::Deserialize<'de>>(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<T> {
        let url = format!("{}{path}", self.base_url);
        let mut req = self.client.get(&url).query(query);
        if self.has_credentials() {
            let (key, ts, sig) = self.sign_request("GET", path)?;
            req = req
                .header("KALSHI-ACCESS-KEY", key)
                .header("KALSHI-ACCESS-TIMESTAMP", ts)
                .header("KALSHI-ACCESS-SIGNATURE", sig);
        }
        let resp = req.send().await.context("HTTP GET failed")?;
        self.parse_response(resp).await
    }

    async fn post<B: Serialize, T: for<'de> serde::Deserialize<'de>>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T> {
        let url = format!("{}{path}", self.base_url);
        let mut req = self.client.post(&url).json(body);
        if self.has_credentials() {
            let (key, ts, sig) = self.sign_request("POST", path)?;
            req = req
                .header("KALSHI-ACCESS-KEY", key)
                .header("KALSHI-ACCESS-TIMESTAMP", ts)
                .header("KALSHI-ACCESS-SIGNATURE", sig);
        }
        let resp = req.send().await.context("HTTP POST failed")?;
        self.parse_response(resp).await
    }

    async fn delete(&self, path: &str) -> Result<()> {
        let url = format!("{}{path}", self.base_url);
        let mut req = self.client.delete(&url);
        if self.has_credentials() {
            let (key, ts, sig) = self.sign_request("DELETE", path)?;
            req = req
                .header("KALSHI-ACCESS-KEY", key)
                .header("KALSHI-ACCESS-TIMESTAMP", ts)
                .header("KALSHI-ACCESS-SIGNATURE", sig);
        }
        let resp = req.send().await.context("HTTP DELETE failed")?;
        let status = resp.status();
        if status.is_success() {
            Ok(())
        } else {
            let body = resp.text().await.unwrap_or_default();
            Err(anyhow!("DELETE {path} failed ({status}): {body}"))
        }
    }

    async fn parse_response<T: for<'de> serde::Deserialize<'de>>(
        &self,
        resp: reqwest::Response,
    ) -> Result<T> {
        let status = resp.status();
        if status == StatusCode::TOO_MANY_REQUESTS {
            return Err(anyhow!("Rate limited by Kalshi API (429)"));
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("API error ({status}): {body}"));
        }
        let body = resp.text().await.context("Failed to read response body")?;
        serde_json::from_str::<T>(&body).map_err(|e| {
            log::error!("Failed to parse API response: {}. Body: {}", e, &body[..body.len().min(500)]);
            anyhow!("Failed to parse API response: {e}")
        })
    }

    // -----------------------------------------------------------------------
    // Public API methods
    // -----------------------------------------------------------------------

    /// Check exchange status.
    pub async fn get_exchange_status(&self) -> Result<ExchangeStatusResponse> {
        self.get("/trade-api/v2/exchange/status", &[]).await
    }

    /// List events with optional series_ticker filter.
    pub async fn list_events(&self, series_ticker: Option<&str>) -> Result<Vec<KalshiEvent>> {
        let mut query: Vec<(&str, &str)> = vec![("limit", "200")];
        if let Some(s) = series_ticker {
            query.push(("series_ticker", s));
        }
        let resp: EventsResponse = self.get("/trade-api/v2/events", &query).await?;
        Ok(resp.events)
    }

    /// List markets with optional series_ticker filter.
    pub async fn list_markets(&self, series_ticker: Option<&str>) -> Result<Vec<KalshiMarket>> {
        let mut query: Vec<(&str, &str)> = vec![("limit", "200")];
        if let Some(s) = series_ticker {
            query.push(("series_ticker", s));
        }
        let resp: MarketsResponse = self.get("/trade-api/v2/markets", &query).await?;
        Ok(resp.markets)
    }

    /// List markets for a specific series using the supported query-parameter approach.
    pub async fn list_series_markets(&self, series_ticker: &str) -> Result<Vec<KalshiMarket>> {
        let resp: MarketsResponse = self
            .get(
                "/trade-api/v2/markets",
                &[("series_ticker", series_ticker), ("limit", "50"), ("status", "open")],
            )
            .await?;
        Ok(resp.markets)
    }

    /// Get a single market by ticker.
    pub async fn get_market(&self, ticker: &str) -> Result<KalshiMarket> {
        let path = format!("/trade-api/v2/markets/{ticker}");
        let resp: MarketResponse = self.get(&path, &[]).await?;
        Ok(resp.market)
    }

    /// Get the orderbook for a market.
    pub async fn get_orderbook(&self, ticker: &str) -> Result<KalshiOrderBook> {
        let path = format!("/trade-api/v2/markets/{ticker}/orderbook");
        let resp: OrderBookResponse = self.get(&path, &[("depth", "5")]).await?;
        Ok(resp.orderbook_fp)
    }

    /// Place an order. Returns the created order.
    pub async fn place_order(&self, order: &KalshiOrderRequest) -> Result<KalshiOrder> {
        let resp: OrderResponse = self.post("/trade-api/v2/portfolio/orders", order).await?;
        Ok(resp.order)
    }

    /// Get a specific order by ID.
    pub async fn get_order(&self, order_id: &str) -> Result<KalshiOrder> {
        let path = format!("/trade-api/v2/portfolio/orders/{order_id}");
        let resp: OrderResponse = self.get(&path, &[]).await?;
        Ok(resp.order)
    }

    /// Cancel an order by ID.
    pub async fn cancel_order(&self, order_id: &str) -> Result<()> {
        let path = format!("/trade-api/v2/portfolio/orders/{order_id}");
        self.delete(&path).await
    }

    /// Get current portfolio positions.
    pub async fn get_positions(&self) -> Result<Vec<KalshiPosition>> {
        let resp: PositionsResponse = self
            .get("/trade-api/v2/portfolio/positions", &[("limit", "500")])
            .await?;
        Ok(resp.market_positions)
    }

    /// Get account balance. Returns dollars at the API boundary (see `KalshiBalance.balance_dollars`).
    pub async fn get_balance(&self) -> Result<KalshiBalance> {
        let resp: BalanceResponse = self.get("/trade-api/v2/portfolio/balance", &[]).await?;
        Ok(KalshiBalance {
            balance_dollars: resp.balance_dollars,
        })
    }
}
