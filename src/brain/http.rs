//! HTTP transport for model requests, including streaming helpers.

use std::io::Read;
use std::net::{IpAddr, SocketAddr};
use ureq::http::Uri;
use ureq::Agent;

use super::errors::*;
use super::limits::*;
use super::providers;
use super::providers::*;
use super::request::*;

fn validate_hosted_endpoint(endpoint: &str) -> Result<(), ModelError> {
    let uri: Uri = endpoint
        .parse()
        .map_err(|_| ModelError::InvalidProviderEndpoint)?;
    let authority = uri.authority().ok_or(ModelError::InvalidProviderEndpoint)?;
    let host = authority.host();
    if host.is_empty() || authority.as_str().contains('@') {
        return Err(ModelError::InvalidProviderEndpoint);
    }
    match uri.scheme_str() {
        Some("https") => Ok(()),
        Some("http") => {
            let bare_host = host.trim_start_matches('[').trim_end_matches(']');
            let loopback = bare_host.eq_ignore_ascii_case("localhost")
                || bare_host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback());
            if loopback {
                Ok(())
            } else {
                Err(ModelError::InvalidProviderEndpoint)
            }
        }
        _ => Err(ModelError::InvalidProviderEndpoint),
    }
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

#[allow(dead_code)]
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
    if body.len() > MAX_MODEL_REQUEST_BYTES {
        return Err(ModelError::RequestTooLarge);
    }
    validate_hosted_endpoint(endpoint)?;
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

#[allow(dead_code)]
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
    if body.len() > MAX_MODEL_REQUEST_BYTES {
        return Err(ModelError::RequestTooLarge);
    }
    validate_hosted_endpoint(endpoint)?;
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

#[cfg(test)]
mod endpoint_tests {
    use super::validate_hosted_endpoint;
    #[test]
    fn accepts_https_and_loopback() {
        for url in [
            "https://api.example.com/v1",
            "http://127.0.0.1:1234/v1",
            "http://[::1]:1234/v1",
            "http://localhost:1234/v1",
        ] {
            assert!(validate_hosted_endpoint(url).is_ok(), "{url}");
        }
    }
    #[test]
    fn rejects_insecure_endpoints() {
        for url in [
            "http://example.com/v1",
            "http://192.168.1.2/v1",
            "https://user:pw@example.com/v1",
            "http://user:pw@localhost/v1",
            "ftp://example.com/v1",
            "file:///etc/passwd",
            "/relative/path",
        ] {
            assert!(validate_hosted_endpoint(url).is_err(), "{url}");
        }
    }
}
