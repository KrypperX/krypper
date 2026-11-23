//! HTTP transport for model requests, including streaming helpers.

use std::io::Read;
use std::net::SocketAddr;
use ureq::Agent;

use super::errors::*;
use super::limits::*;
use super::providers;
use super::providers::*;
use super::request::*;

/// Validates a provider endpoint before any credential header is attached.
///
/// Only `https` endpoints are accepted. Plaintext `http` is tolerated for
/// loopback development servers (`127.0.0.0/8`, `::1` and `localhost`), which the
/// test suite and local proxies rely on. Userinfo (`user:pass@host`) and
/// non-http(s) schemes are always rejected.
pub(crate) fn validate_endpoint(endpoint: &str) -> Result<(), ModelError> {
    if endpoint
        .chars()
        .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err(ModelError::InvalidEndpoint);
    }
    let (scheme, rest) = endpoint
        .split_once("://")
        .ok_or(ModelError::InvalidEndpoint)?;
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    if authority.is_empty() || authority.contains('@') {
        return Err(ModelError::InvalidEndpoint);
    }
    let host = if let Some(bracketed) = authority.strip_prefix('[') {
        let (host, remainder) = bracketed
            .split_once(']')
            .ok_or(ModelError::InvalidEndpoint)?;
        if let Some(port) = remainder.strip_prefix(':') {
            if !valid_endpoint_port(port) {
                return Err(ModelError::InvalidEndpoint);
            }
        } else if !remainder.is_empty() {
            return Err(ModelError::InvalidEndpoint);
        }
        host
    } else {
        let (host, port) = match authority.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        };
        if let Some(port) = port {
            if !valid_endpoint_port(port) {
                return Err(ModelError::InvalidEndpoint);
            }
        }
        host
    };
    if host.is_empty() {
        return Err(ModelError::InvalidEndpoint);
    }
    if scheme.eq_ignore_ascii_case("https") {
        return Ok(());
    }
    if scheme.eq_ignore_ascii_case("http") && is_loopback_endpoint_host(host) {
        return Ok(());
    }
    Err(ModelError::InvalidEndpoint)
}

fn valid_endpoint_port(port: &str) -> bool {
    !port.is_empty()
        && port.bytes().all(|byte| byte.is_ascii_digit())
        && port.parse::<u16>().is_ok()
}

fn is_loopback_endpoint_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .map(|address| address.is_loopback())
            .unwrap_or(false)
}

pub(crate) fn model_agent() -> Agent {
    let config = Agent::config_builder()
        .proxy(None)
        .max_redirects(0)
        .max_response_header_size(MAX_HTTP_HEADER_BYTES)
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_global(Some(REQUEST_TIMEOUT))
        .build();
    Agent::new_with_config(config)
}

pub(crate) fn post_chat(
    agent: &Agent,
    address: SocketAddr,
    body: &[u8],
) -> Result<Vec<u8>, ModelError> {
    if body.len() > MAX_MODEL_REQUEST_BYTES {
        return Err(ModelError::RequestTooLarge);
    }
    let response = agent
        .post(format!("http://{address}/api/chat"))
        .header("Content-Type", "application/json")
        .send(body)
        .map_err(|error| match error {
            ureq::Error::StatusCode(status) => ModelError::HttpStatus(status),
            ureq::Error::ConnectionFailed => ModelError::ServiceUnavailable,
            other => ModelError::Transport(other.to_string()),
        })?;
    let mut body = Vec::new();
    response
        .into_body()
        .as_reader()
        .take(MAX_MODEL_RESPONSE_BYTES.saturating_add(1) as u64)
        .read_to_end(&mut body)
        .map_err(|error| ModelError::Transport(error.to_string()))?;
    if body.len() > MAX_MODEL_RESPONSE_BYTES {
        return Err(ModelError::ResponseTooLarge);
    }
    Ok(body)
}

pub(crate) fn post_hosted_sse<F>(
    agent: &Agent,
    provider: ModelProvider,
    endpoint: &str,
    api_key: &str,
    body: &[u8],
    on_frame: F,
) -> Result<(), ModelError>
where
    F: FnMut(crate::streaming::SseFrame) -> Result<(), ModelError>,
{
    post_hosted_sse_with_headers(agent, provider, endpoint, api_key, &[], body, on_frame)
}

/// Like [`post_hosted_sse`], with additional fixed request headers.
///
/// `extra_headers` must hold only non-secret, provider-defined header names
/// and values that were validated by the caller (for example
/// `anthropic-workspace-id`). Authentication headers are never taken from it.
pub(crate) fn post_hosted_sse_with_headers<F>(
    agent: &Agent,
    provider: ModelProvider,
    endpoint: &str,
    api_key: &str,
    extra_headers: &[(&str, &str)],
    body: &[u8],
    on_frame: F,
) -> Result<(), ModelError>
where
    F: FnMut(crate::streaming::SseFrame) -> Result<(), ModelError>,
{
    validate_endpoint(endpoint)?;
    if body.len() > MAX_MODEL_REQUEST_BYTES {
        return Err(ModelError::RequestTooLarge);
    }
    let authorization = format!("Bearer {api_key}");
    let request = agent
        .post(endpoint)
        .header("Content-Type", "application/json")
        .header("Accept", "text/event-stream");
    let request = if provider == ModelProvider::Anthropic {
        request
            .header("x-api-key", api_key)
            .header("anthropic-version", providers::ANTHROPIC_API_VERSION)
    } else {
        request.header("Authorization", &authorization)
    };
    let request = extra_headers
        .iter()
        .fold(request, |request, (name, value)| {
            request.header(*name, *value)
        });
    let response = request.send(body).map_err(|error| match error {
        ureq::Error::StatusCode(status) => ModelError::ProviderHttpStatus(provider, status),
        _ => ModelError::ProviderTransport(provider),
    })?;
    let mut response_body = response.into_body();
    let mut reader = response_body.as_reader();
    crate::streaming::consume_sse(&mut reader, MAX_MODEL_RESPONSE_BYTES, on_frame).map_err(
        |error| match error {
            crate::streaming::SseReadError::Io(_) => ModelError::ProviderTransport(provider),
            crate::streaming::SseReadError::Decode(
                crate::streaming::SseDecodeError::FrameTooLarge,
            ) => ModelError::ProviderResponseTooLarge(provider),
            crate::streaming::SseReadError::Decode(_) => ModelError::ProviderInvalidJson(provider),
            crate::streaming::SseReadError::Consumer(error) => error,
        },
    )
}

#[allow(dead_code)]
pub(crate) fn post_hosted_text_stream<F>(
    agent: &Agent,
    provider: ModelProvider,
    endpoint: &str,
    api_key: &str,
    body: &[u8],
    mut on_text: F,
) -> Result<(), ModelError>
where
    F: FnMut(String) -> Result<(), ModelError>,
{
    post_hosted_sse(agent, provider, endpoint, api_key, body, |frame| {
        if let Some(text) = crate::streaming::provider_text_delta(&frame) {
            on_text(text)?;
        }
        Ok(())
    })
}

pub(crate) fn post_responses_text_stream<F>(
    agent: &Agent,
    provider: ModelProvider,
    endpoint: &str,
    api_key: &str,
    request: &[u8],
    on_text: F,
) -> Result<(), ModelError>
where
    F: FnMut(String) -> Result<(), ModelError>,
{
    let streaming_request = enable_responses_streaming(request)?;
    post_hosted_text_stream(
        agent,
        provider,
        endpoint,
        api_key,
        &streaming_request,
        on_text,
    )
}

pub(crate) fn post_hosted(
    agent: &Agent,
    provider: ModelProvider,
    endpoint: &str,
    api_key: &str,
    api_version: Option<&str>,
    body: &[u8],
) -> Result<Vec<u8>, ModelError> {
    post_hosted_with_headers(agent, provider, endpoint, api_key, api_version, &[], body)
}

/// Like [`post_hosted`], with additional fixed request headers.
///
/// `extra_headers` must hold only non-secret, provider-defined header names
/// and values that were validated by the caller (for example
/// `anthropic-workspace-id`). Authentication headers are never taken from it.
pub(crate) fn post_hosted_with_headers(
    agent: &Agent,
    provider: ModelProvider,
    endpoint: &str,
    api_key: &str,
    api_version: Option<&str>,
    extra_headers: &[(&str, &str)],
    body: &[u8],
) -> Result<Vec<u8>, ModelError> {
    validate_endpoint(endpoint)?;
    if body.len() > MAX_MODEL_REQUEST_BYTES {
        return Err(ModelError::RequestTooLarge);
    }
    let request = agent
        .post(endpoint)
        .header("Content-Type", "application/json");
    let request = if provider == ModelProvider::Gemini {
        request.header("x-goog-api-key", api_key)
    } else {
        let authorization = format!("Bearer {api_key}");
        request.header("Authorization", &authorization)
    };
    let request = match api_version {
        Some(version) => request.header("anthropic-version", version),
        None => request,
    };
    let request = extra_headers
        .iter()
        .fold(request, |request, (name, value)| {
            request.header(*name, *value)
        });
    let response = request.send(body).map_err(|error| match error {
        ureq::Error::StatusCode(status) => ModelError::ProviderHttpStatus(provider, status),
        _ => ModelError::ProviderTransport(provider),
    })?;
    let mut response_body = Vec::new();
    response
        .into_body()
        .as_reader()
        .take(MAX_MODEL_RESPONSE_BYTES.saturating_add(1) as u64)
        .read_to_end(&mut response_body)
        .map_err(|_| ModelError::ProviderTransport(provider))?;
    if response_body.len() > MAX_MODEL_RESPONSE_BYTES {
        return Err(ModelError::ProviderResponseTooLarge(provider));
    }
    Ok(response_body)
}
