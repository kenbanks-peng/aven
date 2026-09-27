//! Drives core sync client sessions over HTTP.
//!
//! Requests follow no redirects, use no proxy and never decode content;
//! response bodies are read up to one byte past the request's limit, so the
//! engine sees and refuses anything larger. Diagnostics deliberately discard
//! Reqwest URLs and bodies.
use std::error::Error as _;
use std::future::Future;

use anyhow::Result;
use aven_core::sync::client::{HttpHeader, HttpResponse, Link, PreparedRequest, Session, Step};

/// One Reqwest client that sends session requests.
#[derive(Clone)]
pub struct HttpDriver {
    pub(crate) http: reqwest::Client,
}

impl HttpDriver {
    pub fn new() -> Result<Self> {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .no_gzip()
            .no_brotli()
            .no_zstd()
            .no_deflate()
            .build()
            .map_err(|_| anyhow::anyhow!("error sync-transport"))?;
        Ok(Self { http })
    }

    /// Runs `operation` in a session until it finishes.
    pub async fn run<'a, T, F, Fut>(&self, operation: F) -> Result<T>
    where
        F: FnOnce(Link) -> Fut,
        Fut: Future<Output = Result<T>> + Send + 'a,
    {
        self.drive(Session::new(operation)).await
    }

    /// Answers every step of `session` until it finishes.
    async fn drive<T>(&self, mut session: Session<'_, T>) -> Result<T> {
        loop {
            match session.next().await? {
                Step::Done(value) => return Ok(value),
                Step::Wait(delay) => tokio::time::sleep(delay).await,
                Step::Request(request) => self.answer(&mut session, request).await?,
            }
        }
    }

    /// Sends one request of `session` and hands it the outcome.
    pub async fn answer<T>(
        &self,
        session: &mut Session<'_, T>,
        request: PreparedRequest,
    ) -> Result<()> {
        let context = request.context;
        match self.send(request).await {
            Ok(response) => session.accept_response(context, response),
            Err(SendFailure::Network) => session.register_transport_failure(context),
            Err(SendFailure::SecureTransport) => session.register_secure_transport_failure(context),
        }
    }

    async fn send(&self, request: PreparedRequest) -> Result<HttpResponse, SendFailure> {
        let secure = request.url.starts_with("https://");
        let method = reqwest::Method::from_bytes(request.method.as_bytes())
            .map_err(|_| SendFailure::Network)?;
        let mut builder = self
            .http
            .request(method, request.url.as_str())
            .timeout(request.timeout)
            .body(request.body);
        for header in &request.headers {
            let mut value = reqwest::header::HeaderValue::from_str(&header.value)
                .map_err(|_| SendFailure::Network)?;
            if header.name.eq_ignore_ascii_case("authorization") {
                value.set_sensitive(true);
            }
            builder = builder.header(header.name.as_str(), value);
        }
        let mut response = builder
            .send()
            .await
            .map_err(|error| SendFailure::from_reqwest(secure, &error))?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .filter_map(|(name, value)| {
                Some(HttpHeader {
                    name: name.as_str().to_string(),
                    value: value.to_str().ok()?.to_string(),
                })
            })
            .collect();
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| SendFailure::from_reqwest(secure, &error))?
        {
            let room = (request.response_limit + 1).saturating_sub(body.len());
            body.extend_from_slice(&chunk[..chunk.len().min(room)]);
            if body.len() > request.response_limit {
                break;
            }
        }
        Ok(HttpResponse {
            status,
            headers,
            body,
        })
    }
}

#[derive(Clone, Copy)]
enum SendFailure {
    Network,
    SecureTransport,
}

impl SendFailure {
    fn from_reqwest(secure: bool, error: &reqwest::Error) -> Self {
        if secure && contains_rustls_error(error) {
            Self::SecureTransport
        } else {
            Self::Network
        }
    }
}

fn contains_rustls_error(error: &(dyn std::error::Error + 'static)) -> bool {
    if error.is::<rustls::Error>() {
        return true;
    }
    if let Some(inner) = error
        .downcast_ref::<std::io::Error>()
        .and_then(std::io::Error::get_ref)
        && contains_rustls_error(inner)
    {
        return true;
    }
    error.source().is_some_and(contains_rustls_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_rustls_errors_nested_in_io_errors() {
        let tls = std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            rustls::Error::General("certificate failure".into()),
        );
        let outer = std::io::Error::new(std::io::ErrorKind::Other, tls);
        assert!(contains_rustls_error(&outer));
        assert!(!contains_rustls_error(&std::io::Error::from(
            std::io::ErrorKind::ConnectionRefused
        )));
    }
}
