//! Minimal outbound-HTTP facade with one signature on every target.
//!
//! App code (e.g. nutrition's strategies) calls [`fetch`] and never names a
//! transport. Natively it runs on reqwest; inside a WASM component it
//! becomes the `tangram:app/host.http-fetch` import — the host performs the
//! request with its own client and **enforces the app's outbound host
//! allowlist** there, so capability-confined networking is the only kind a
//! component can express.

use std::fmt::Write as _;

use anyhow::Context;

/// An outbound HTTP request.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Request {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn new(method: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            method: method.into(),
            url: url.into(),
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    pub fn get(url: impl Into<String>) -> Self {
        Self::new("GET", url)
    }

    pub fn post(url: impl Into<String>) -> Self {
        Self::new("POST", url)
    }

    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// JSON request body (sets `Content-Type: application/json`).
    pub fn json(self, value: &serde_json::Value) -> Self {
        let mut req = self.header("content-type", "application/json");
        req.body = value.to_string().into_bytes();
        req
    }
}

/// An HTTP response: status, headers, raw body.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// Deserialize the body as JSON.
    pub fn json<T: serde::de::DeserializeOwned>(&self) -> anyhow::Result<T> {
        serde_json::from_slice(&self.body).context("response body is not the expected JSON")
    }
}

/// Percent-encode a string for use inside a URL query value.
pub fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}

/// Perform one outbound HTTP request. Errors cover transport failures only;
/// non-2xx responses come back as ordinary [`Response`]s for the caller to
/// interpret.
pub async fn fetch(request: Request) -> anyhow::Result<Response> {
    imp::fetch(request).await
}

// ── native: reqwest ──────────────────────────────────────────────────────────

#[cfg(not(target_family = "wasm"))]
mod imp {
    use anyhow::Context;

    use super::{Request, Response};

    pub async fn fetch(request: Request) -> anyhow::Result<Response> {
        let method: reqwest::Method = request
            .method
            .parse()
            .with_context(|| format!("invalid HTTP method {:?}", request.method))?;
        let mut builder = reqwest::Client::new().request(method, &request.url);
        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }
        if !request.body.is_empty() {
            builder = builder.body(request.body);
        }
        let resp = builder
            .send()
            .await
            .with_context(|| format!("request to {} failed", request.url))?;
        let status = resp.status().as_u16();
        let headers = resp
            .headers()
            .iter()
            .map(|(k, v)| (k.to_string(), String::from_utf8_lossy(v.as_bytes()).into()))
            .collect();
        let body = resp
            .bytes()
            .await
            .context("reading response body")?
            .to_vec();
        Ok(Response {
            status,
            headers,
            body,
        })
    }
}

// ── wasm guest: the tangram:app/host.http-fetch import ───────────────────────

#[cfg(target_family = "wasm")]
mod imp {
    use anyhow::Context;
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as B64;

    use super::{Request, Response};

    pub async fn fetch(request: Request) -> anyhow::Result<Response> {
        let headers: serde_json::Map<String, serde_json::Value> = request
            .headers
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
            .collect();
        let request_json = serde_json::json!({
            "method": request.method,
            "url": request.url,
            "headers": headers,
            "body-b64": B64.encode(&request.body),
        })
        .to_string();

        // Synchronous from the guest's view; the host runs it async.
        let response_json = crate::guest::wit::tangram::app::host::http_fetch(&request_json)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let value: serde_json::Value = serde_json::from_str(&response_json)
            .context("host returned malformed response JSON")?;
        let status = value
            .get("status")
            .and_then(serde_json::Value::as_u64)
            .context("host response missing status")? as u16;
        let headers = value
            .get("headers")
            .and_then(serde_json::Value::as_object)
            .map(|map| {
                map.iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
                    .collect()
            })
            .unwrap_or_default();
        let body = match value.get("body-b64").and_then(serde_json::Value::as_str) {
            Some(b64) => B64
                .decode(b64)
                .context("host response body is not base64")?,
            None => Vec::new(),
        };
        Ok(Response {
            status,
            headers,
            body,
        })
    }
}
