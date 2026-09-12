//! The client side of the proxy: one blocking ureq agent that forwards requests as
//! sent and hands back a streaming response.

use std::io::Read;
use std::time::Duration;

use anyhow::{Result, bail};
use ureq::config::AutoHeaderValue;
use ureq::http::{self, HeaderName, HeaderValue};
use ureq::tls::{RootCerts, TlsConfig};
use ureq::{Agent, Body, SendBody};

pub struct Upstream {
    agent: Agent,
    /// Scheme, authority and optional path prefix, without a trailing slash.
    pub base: String,
}

pub enum Outbound<'a> {
    Bytes(&'a [u8]),
    Stream(&'a mut dyn Read),
}

pub enum SendError {
    /// The request could not be expressed, such as a header value HTTP rejects.
    Request(String),
    /// The upstream could not be reached or failed before a response arrived.
    Upstream(String),
}

impl Upstream {
    pub fn new(base: &str) -> Result<Self> {
        let base = base.trim_end_matches('/').to_owned();
        // The URL is printed and logged, so it must not carry credentials.
        let authority = base
            .split_once("://")
            .map_or(base.as_str(), |(_, rest)| rest)
            .split('/')
            .next()
            .unwrap_or_default();
        if authority.contains('@') {
            bail!("the upstream URL cannot contain credentials; the client's headers carry them");
        }
        let uri: http::Uri = match base.parse() {
            Ok(uri) => uri,
            Err(error) => bail!("invalid upstream URL {base}: {error}"),
        };
        if !matches!(uri.scheme_str(), Some("http" | "https")) || uri.host().is_none() {
            bail!("the upstream URL must start with http:// or https://: {base}");
        }
        if uri.query().is_some() {
            bail!("the upstream URL cannot have a query: {base}");
        }
        let config = Agent::config_builder()
            .max_redirects(0)
            .http_status_as_error(false)
            .timeout_connect(Some(Duration::from_secs(30)))
            // Add nothing the client did not send.
            .user_agent(AutoHeaderValue::None)
            .accept(AutoHeaderValue::None)
            .accept_encoding(AutoHeaderValue::None)
            .tls_config(
                TlsConfig::builder()
                    .root_certs(RootCerts::PlatformVerifier)
                    .build(),
            )
            .build();
        Ok(Self {
            agent: Agent::new_with_config(config),
            base,
        })
    }

    /// The upstream host and port, for refusing to forward to ourselves.
    pub fn authority(&self) -> Option<(String, u16)> {
        let uri: http::Uri = self.base.parse().ok()?;
        let default = if uri.scheme_str() == Some("https") {
            443
        } else {
            80
        };
        Some((
            uri.host()?.to_ascii_lowercase(),
            uri.port_u16().unwrap_or(default),
        ))
    }

    pub fn send(
        &self,
        method: &str,
        target: &str,
        headers: &[(String, Vec<u8>)],
        body: Outbound,
    ) -> Result<http::Response<Body>, SendError> {
        let mut builder = http::Request::builder()
            .method(method)
            .uri(format!("{}{target}", self.base));
        for (name, value) in headers {
            if super::http::hop_by_hop(name)
                || ["host", "content-length", "expect"]
                    .iter()
                    .any(|n| name.eq_ignore_ascii_case(n))
            {
                continue;
            }
            let (Ok(name), Ok(value)) = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_bytes(value),
            ) else {
                return Err(SendError::Request(format!("invalid header {name}")));
            };
            builder = builder.header(name, value);
        }
        let result = match body {
            Outbound::Bytes(bytes) => {
                let needs_body = matches!(method, "POST" | "PUT" | "PATCH");
                if bytes.is_empty() && !needs_body {
                    builder.body(SendBody::none()).map(|r| self.agent.run(r))
                } else {
                    builder.body(bytes).map(|r| self.agent.run(r))
                }
            }
            Outbound::Stream(reader) => builder
                .body(SendBody::from_reader(reader))
                .map(|r| self.agent.run(r)),
        };
        match result {
            Err(error) => Err(SendError::Request(error.to_string())),
            Ok(Err(error)) => Err(SendError::Upstream(error.to_string())),
            Ok(Ok(response)) => Ok(response),
        }
    }
}
