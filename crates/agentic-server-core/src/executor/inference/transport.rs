//! Responses transport selection. Opaque replay cannot inherit caller client state.

use std::{sync::Arc, time::Duration};

use super::{response_text_limited, send_request_with_policy};
use crate::executor::error::{ExecutorError, ExecutorResult};

#[derive(Debug, Clone, Copy)]
pub(super) enum ResponsePolicy {
    Compatible,
    Opaque,
}

/// Immutable client and error policy; no per-request credential is retained here.
#[derive(Debug, Clone)]
pub(crate) struct ResponsesTransport {
    client: Arc<reqwest::Client>,
    pub(super) response_policy: ResponsePolicy,
}

fn opaque_client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .no_proxy()
        .connect_timeout(Duration::from_secs(30))
        .read_timeout(Duration::from_secs(600))
        .pool_max_idle_per_host(1)
}

impl ResponsesTransport {
    pub(crate) fn shared(client: Arc<reqwest::Client>) -> Self {
        Self {
            client,
            response_policy: ResponsePolicy::Compatible,
        }
    }

    /// The pinned profile uses direct HTTPS with only its per-request bearer
    /// credential. No redirects, environment proxies, retries, cookies, or caller
    /// default headers can silently change the qualified routing identity.
    pub(crate) fn opaque() -> ExecutorResult<Self> {
        let client = opaque_client_builder()
            .build()
            .map_err(|_| ExecutorError::LLMTransport {
                status: http::StatusCode::INTERNAL_SERVER_ERROR,
                message: "failed to initialize opaque Responses transport",
            })?;
        Ok(Self {
            client: Arc::new(client),
            response_policy: ResponsePolicy::Opaque,
        })
    }

    pub(super) async fn send(
        &self,
        url: &str,
        body: String,
        auth: Option<&str>,
        chunk_timeout: Duration,
    ) -> ExecutorResult<reqwest::Response> {
        send_request_with_policy(&self.client, url, body, auth, None, chunk_timeout, self.response_policy).await
    }

    pub(crate) async fn fetch_json(
        &self,
        url: &str,
        body: String,
        auth: Option<&str>,
        max_bytes: usize,
    ) -> ExecutorResult<String> {
        let response = self.send(url, body, auth, Duration::ZERO).await?;
        // The opaque client's nonzero read timeout also bounds headers/body reads
        // when the caller disables the separate streaming chunk timeout.
        response_text_limited(response, Duration::ZERO, max_bytes).await
    }
}

#[cfg(test)]
mod tests;
