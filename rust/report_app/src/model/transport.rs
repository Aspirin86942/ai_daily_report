use crate::{config::AppConfig, ReportError};
use reqwest::{blocking::Client, redirect::Policy, retry};
use std::{io::Read, sync::OnceLock, time::Duration};

#[derive(Debug)]
pub struct HttpResponse {
    pub status: u16,
    pub retry_after: Option<String>,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, Copy)]
pub enum TransportFailure {
    Timeout,
    Connection,
    Interrupted,
    InvalidResponse,
    Configuration,
    PermissionDenied,
}

impl TransportFailure {
    pub fn code(self) -> &'static str {
        match self {
            Self::Timeout => "MODEL_TIMEOUT",
            Self::Connection => "MODEL_CONNECTION_FAILED",
            Self::Interrupted => "MODEL_RESPONSE_INTERRUPTED",
            Self::InvalidResponse => "MODEL_RESPONSE_TOO_LARGE",
            Self::Configuration => "MODEL_TRANSPORT_INVALID",
            Self::PermissionDenied => "ARCHIVE_MODEL_PERMISSION_DENIED",
        }
    }
    pub fn retryable(self) -> bool {
        matches!(self, Self::Timeout | Self::Connection | Self::Interrupted)
    }
}

pub trait ModelTransport: Sync {
    fn prepare(&self) -> Result<(), ReportError> {
        Ok(())
    }
    fn send(&self, request: &[u8], timeout: Duration) -> Result<HttpResponse, TransportFailure>;
}

pub struct HttpTransport {
    client: OnceLock<Client>,
    endpoint: String,
    api_key: String,
    http_proxy: String,
    https_proxy: String,
}

impl HttpTransport {
    pub fn new(config: &AppConfig) -> Self {
        Self {
            client: OnceLock::new(),
            endpoint: config.endpoint(),
            api_key: config.api_key().into(),
            http_proxy: config.proxy_http.clone(),
            https_proxy: config.proxy_https.clone(),
        }
    }
    pub fn with_endpoint(
        endpoint: &str,
        key: &str,
        http_proxy: &str,
        https_proxy: &str,
    ) -> Result<Self, ReportError> {
        let transport = Self {
            client: OnceLock::new(),
            endpoint: endpoint.into(),
            api_key: key.into(),
            http_proxy: http_proxy.into(),
            https_proxy: https_proxy.into(),
        };
        transport.prepare()?;
        Ok(transport)
    }
}

impl ModelTransport for HttpTransport {
    fn prepare(&self) -> Result<(), ReportError> {
        if self.client.get().is_some() {
            return Ok(());
        }
        if self.api_key.is_empty() {
            return Err(ReportError::new(
                "MODEL_CREDENTIAL_MISSING",
                "缺少模型服务凭据",
            ));
        }
        let error = || ReportError::new("MODEL_TRANSPORT_INVALID", "模型地址或代理配置无效");
        let mut builder = Client::builder()
            .redirect(Policy::none())
            .retry(retry::never())
            .no_proxy();
        if !self.http_proxy.is_empty() {
            builder = builder.proxy(reqwest::Proxy::http(&self.http_proxy).map_err(|_| error())?);
        }
        if !self.https_proxy.is_empty() {
            builder = builder.proxy(reqwest::Proxy::https(&self.https_proxy).map_err(|_| error())?);
        }
        let url = reqwest::Url::parse(&self.endpoint).map_err(|_| error())?;
        if !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(error());
        }
        let _ = self.client.set(builder.build().map_err(|_| error())?);
        Ok(())
    }
    fn send(&self, request: &[u8], timeout: Duration) -> Result<HttpResponse, TransportFailure> {
        let response = self
            .client
            .get()
            .ok_or(TransportFailure::Configuration)?
            .post(&self.endpoint)
            .bearer_auth(&self.api_key)
            .header("Content-Type", "application/json")
            .timeout(timeout)
            .body(request.to_vec())
            .send()
            .map_err(|error| {
                if error.is_timeout() {
                    TransportFailure::Timeout
                } else if error.is_connect() {
                    TransportFailure::Connection
                } else if error.is_builder() {
                    TransportFailure::Configuration
                } else {
                    TransportFailure::Interrupted
                }
            })?;
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|h| h.to_str().ok())
            .map(str::to_owned);
        let mut body = Vec::new();
        response
            .take(1024 * 1024 + 1)
            .read_to_end(&mut body)
            .map_err(|_| TransportFailure::Interrupted)?;
        if body.len() > 1024 * 1024 {
            return Err(TransportFailure::InvalidResponse);
        }
        Ok(HttpResponse {
            status,
            retry_after,
            body,
        })
    }
}
