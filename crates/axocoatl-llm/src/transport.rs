//! Bounded HTTP and server-sent-event helpers shared by model providers.

use std::time::Duration;

use reqwest::Response;
use serde::de::DeserializeOwned;
use tokio_stream::{Stream, StreamExt};

use crate::ProviderError;

/// Maximum retained JSON response from a model API. This is well above the
/// advertised output limits while still bounding a malicious or broken peer.
pub const MAX_RESPONSE_BODY_BYTES: usize = 16 * 1024 * 1024;
/// Maximum total bytes accepted across one streaming model response.
pub const MAX_STREAM_BYTES: usize = 16 * 1024 * 1024;
/// Maximum bytes in one SSE event before JSON decoding.
pub const MAX_SSE_EVENT_BYTES: usize = 1024 * 1024;
/// Maximum number of events in one provider response, preventing tiny-frame
/// amplification from consuming unbounded per-event bookkeeping.
pub const MAX_SSE_EVENTS: usize = 65_536;
/// Maximum provider error body retained for a user-facing diagnostic.
pub const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;
/// Deadline for request headers or a complete buffered response.
pub const RESPONSE_TIMEOUT: Duration = Duration::from_secs(120);
/// Maximum silence between streaming response chunks.
pub const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(90);
/// Absolute lifetime of one streaming response. The byte cap bounds retained
/// memory, while this deadline also stops a peer that drips tiny chunks often
/// enough to evade the idle timer indefinitely.
pub const STREAM_TOTAL_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const ERROR_MARKER: &str = "\n[truncated: provider error exceeded the 64 KiB safety limit]";

/// A reqwest client with a bounded connect phase. Buffered requests add a
/// per-request total timeout; streams use a header deadline plus an idle timer.
pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .pool_idle_timeout(Duration::from_secs(90))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("provider HTTP client uses only static valid settings")
}

fn utf8_prefix(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

/// Bound a diagnostic and redact exact configured credentials before it can be
/// logged, persisted, or displayed. Empty secrets are deliberately ignored.
pub fn bounded_redacted(value: &str, limit: usize, secrets: &[&str]) -> String {
    let mut redacted = value.to_string();
    for secret in secrets.iter().copied().filter(|secret| !secret.is_empty()) {
        redacted = redacted.replace(secret, "[REDACTED]");
    }
    if redacted.len() <= limit {
        return redacted;
    }

    let content_limit = limit.saturating_sub(ERROR_MARKER.len());
    let mut output = String::with_capacity(limit);
    output.push_str(utf8_prefix(&redacted, content_limit));
    output.push_str(utf8_prefix(
        ERROR_MARKER,
        limit.saturating_sub(output.len()),
    ));
    output
}

pub fn network_error(error: &reqwest::Error, secrets: &[&str]) -> ProviderError {
    ProviderError::Network(bounded_redacted(&error.to_string(), 8 * 1024, secrets))
}

/// Pull one stream item subject to both a fixed response-wide deadline and a
/// per-item idle deadline. `total_deadline` must be created once before the
/// provider decode loop and reused for every call.
pub async fn next_stream_item<S>(
    stream: &mut S,
    total_deadline: tokio::time::Instant,
    idle_timeout: Duration,
    provider: &str,
) -> Result<Option<S::Item>, ProviderError>
where
    S: Stream + Unpin,
{
    tokio::time::timeout_at(
        total_deadline,
        tokio::time::timeout(idle_timeout, stream.next()),
    )
    .await
    .map_err(|_| ProviderError::Stream(format!("{provider} stream total timeout")))?
    .map_err(|_| ProviderError::Stream(format!("{provider} stream idle timeout")))
}

/// Validate an operator-supplied OpenAI-compatible endpoint without ever
/// reflecting it in an error. Credentials belong in provider credential
/// fields, never URL userinfo/query/fragment where HTTP errors may expose them.
pub fn validated_endpoint(
    base_url: &str,
    suffix: &str,
    provider: &str,
) -> Result<String, ProviderError> {
    let parsed = reqwest::Url::parse(base_url).map_err(|_| ProviderError::InvalidRequest {
        provider: provider.to_string(),
        message: "provider base URL is not a valid absolute URL".to_string(),
    })?;
    let valid_scheme = matches!(parsed.scheme(), "http" | "https");
    let contains_credential_material = !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some();
    if !valid_scheme || parsed.host_str().is_none() || contains_credential_material {
        return Err(ProviderError::InvalidRequest {
            provider: provider.to_string(),
            message: "provider base URL must be HTTP(S) without userinfo, query, or fragment"
                .to_string(),
        });
    }
    Ok(format!(
        "{}/{}",
        base_url.trim_end_matches('/'),
        suffix.trim_start_matches('/')
    ))
}

async fn read_bounded(
    mut response: Response,
    provider: &str,
    limit: usize,
) -> Result<Vec<u8>, ProviderError> {
    let status = response.status().as_u16();
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(ProviderError::ApiError {
            provider: provider.to_string(),
            status,
            message: format!("response body exceeds the {limit}-byte safety limit"),
        });
    }

    let mut body =
        Vec::with_capacity(response.content_length().unwrap_or(0).min(limit as u64) as usize);
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| network_error(&error, &[]))?
    {
        if chunk.len() > limit.saturating_sub(body.len()) {
            return Err(ProviderError::ApiError {
                provider: provider.to_string(),
                status,
                message: format!("response body exceeds the {limit}-byte safety limit"),
            });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Decode one successful buffered response without allowing `Response::json`
/// to collect an unbounded body first.
pub async fn read_json<T: DeserializeOwned>(
    response: Response,
    provider: &str,
) -> Result<T, ProviderError> {
    let status = response.status().as_u16();
    let body = read_bounded(response, provider, MAX_RESPONSE_BODY_BYTES).await?;
    serde_json::from_slice(&body).map_err(|error| ProviderError::ApiError {
        provider: provider.to_string(),
        status,
        message: format!("invalid bounded JSON response: {error}"),
    })
}

/// Retain a bounded, UTF-8-safe provider error preview. Dropping the response
/// after the cap also stops draining an unexpectedly large body.
pub async fn read_error_text(response: Response, secrets: &[&str]) -> String {
    read_error_text_with_limits(response, secrets, STREAM_IDLE_TIMEOUT, RESPONSE_TIMEOUT).await
}

/// Testable form of [`read_error_text`] with an explicit per-chunk idle limit.
pub async fn read_error_text_with_idle(
    response: Response,
    secrets: &[&str],
    idle_timeout: Duration,
) -> String {
    read_error_text_with_limits(response, secrets, idle_timeout, RESPONSE_TIMEOUT).await
}

/// Testable form with explicit idle and whole-body deadlines.
pub async fn read_error_text_with_limits(
    mut response: Response,
    secrets: &[&str],
    idle_timeout: Duration,
    total_timeout: Duration,
) -> String {
    let mut body = Vec::with_capacity(MAX_ERROR_BODY_BYTES);
    let mut truncated = response
        .content_length()
        .is_some_and(|length| length > MAX_ERROR_BODY_BYTES as u64);
    let total_deadline = tokio::time::Instant::now() + total_timeout;

    loop {
        match tokio::time::timeout_at(
            total_deadline,
            tokio::time::timeout(idle_timeout, response.chunk()),
        )
        .await
        {
            Err(_) => {
                return "provider error response exceeded its total body deadline".to_string();
            }
            Ok(Err(_)) => {
                return "provider error response timed out while reading its bounded body"
                    .to_string();
            }
            Ok(Ok(result)) => match result {
                Ok(Some(chunk)) => {
                    let remaining = MAX_ERROR_BODY_BYTES - body.len();
                    if remaining == 0 {
                        truncated = true;
                        break;
                    }
                    if chunk.len() > remaining {
                        body.extend_from_slice(&chunk[..remaining]);
                        truncated = true;
                        break;
                    }
                    body.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                Err(error) => {
                    return bounded_redacted(
                        &format!("failed to read provider error response: {error}"),
                        MAX_ERROR_BODY_BYTES,
                        secrets,
                    );
                }
            },
        }
    }

    // If the body was cut at the cap, remove enough tail bytes that a secret
    // crossing that boundary cannot survive as a visible prefix.
    if truncated {
        let boundary_guard = secrets.iter().map(|secret| secret.len()).max().unwrap_or(0);
        let guarded_len = body.len().saturating_sub(boundary_guard);
        body.truncate(guarded_len);
    }
    let mut text = String::from_utf8_lossy(&body).into_owned();
    if truncated {
        text.push_str(ERROR_MARKER);
    }
    bounded_redacted(&text, MAX_ERROR_BODY_BYTES, secrets)
}

#[derive(Debug, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// Incremental SSE decoder with both per-event and whole-response bounds.
/// It accepts LF and CRLF framing and preserves split UTF-8 sequences until a
/// complete event is available.
pub struct SseDecoder {
    buffer: Vec<u8>,
    received: usize,
    max_total: usize,
    max_event: usize,
    emitted_events: usize,
    scan_from: usize,
}

impl SseDecoder {
    pub fn provider_default() -> Self {
        Self::new(MAX_STREAM_BYTES, MAX_SSE_EVENT_BYTES)
    }

    pub fn new(max_total: usize, max_event: usize) -> Self {
        Self {
            buffer: Vec::new(),
            received: 0,
            max_total,
            max_event,
            emitted_events: 0,
            scan_from: 0,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<SseEvent>, ProviderError> {
        if bytes.len() > self.max_total.saturating_sub(self.received) {
            return Err(ProviderError::Stream(format!(
                "provider stream exceeded the {}-byte safety limit",
                self.max_total
            )));
        }
        self.received += bytes.len();
        self.buffer.extend_from_slice(bytes);

        let mut events = Vec::new();
        let mut start = 0usize;
        let mut search_from = self.scan_from.min(self.buffer.len());
        while let Some((end, separator_len)) = find_event_boundary(&self.buffer, search_from) {
            if end.saturating_sub(start) > self.max_event {
                return Err(ProviderError::Stream(format!(
                    "provider SSE event exceeded the {}-byte safety limit",
                    self.max_event
                )));
            }
            if let Some(event) = parse_event(&self.buffer[start..end])? {
                self.record_event()?;
                events.push(event);
            }
            start = end + separator_len;
            search_from = start;
        }

        if start > 0 {
            self.buffer.drain(..start);
        }
        self.scan_from = self.buffer.len().saturating_sub(3);
        if self.buffer.len() > self.max_event {
            return Err(ProviderError::Stream(format!(
                "provider SSE event exceeded the {}-byte safety limit",
                self.max_event
            )));
        }
        Ok(events)
    }

    /// Dispatch a final unterminated event at clean EOF, matching SSE clients,
    /// while still rejecting incomplete UTF-8 or an oversized tail.
    pub fn finish(&mut self) -> Result<Vec<SseEvent>, ProviderError> {
        if self.buffer.iter().all(u8::is_ascii_whitespace) {
            self.buffer.clear();
            return Ok(Vec::new());
        }
        if self.buffer.len() > self.max_event {
            return Err(ProviderError::Stream(format!(
                "provider SSE event exceeded the {}-byte safety limit",
                self.max_event
            )));
        }
        let event = parse_event(&self.buffer)?;
        self.buffer.clear();
        if event.is_some() {
            self.record_event()?;
        }
        Ok(event.into_iter().collect())
    }

    fn record_event(&mut self) -> Result<(), ProviderError> {
        self.emitted_events = self.emitted_events.saturating_add(1);
        if self.emitted_events > MAX_SSE_EVENTS {
            return Err(ProviderError::Stream(format!(
                "provider stream exceeded the {MAX_SSE_EVENTS}-event safety limit"
            )));
        }
        Ok(())
    }
}

fn find_event_boundary(bytes: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut index = from;
    while index < bytes.len() {
        if let Some(first_len) = line_ending_len(bytes, index) {
            let second = index + first_len;
            if let Some(second_len) = line_ending_len(bytes, second) {
                return Some((index, first_len + second_len));
            }
            index += first_len;
        } else {
            index += 1;
        }
    }
    None
}

fn line_ending_len(bytes: &[u8], index: usize) -> Option<usize> {
    match bytes.get(index) {
        Some(b'\r') if bytes.get(index + 1) == Some(&b'\n') => Some(2),
        Some(b'\r' | b'\n') => Some(1),
        _ => None,
    }
}

fn parse_event(bytes: &[u8]) -> Result<Option<SseEvent>, ProviderError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| ProviderError::Stream("provider SSE event was not valid UTF-8".to_string()))?;
    let mut event = None;
    let mut data = String::new();
    let mut saw_data = false;

    for line in text.split(['\r', '\n']) {
        if line.starts_with(':') || line.is_empty() {
            continue;
        }
        let (field, value) = line
            .split_once(':')
            .map(|(field, value)| (field, value.strip_prefix(' ').unwrap_or(value)))
            .unwrap_or((line, ""));
        match field {
            "event" => event = Some(value.to_string()),
            "data" => {
                if saw_data {
                    data.push('\n');
                }
                data.push_str(value);
                saw_data = true;
            }
            _ => {}
        }
    }

    Ok(saw_data.then_some(SseEvent { event, data }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoder_handles_split_utf8_crlf_and_multiline_data() {
        let mut decoder = SseDecoder::new(1024, 512);
        let bytes = "data: hé\r\ndata: there\r\n\r\n".as_bytes();
        let split = bytes.iter().position(|byte| *byte == 0xa9).unwrap();
        assert!(decoder.push(&bytes[..split]).unwrap().is_empty());
        let events = decoder.push(&bytes[split..]).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "hé\nthere");
    }

    #[test]
    fn decoder_rejects_event_and_total_overflow() {
        let mut event_limited = SseDecoder::new(128, 8);
        let error = event_limited.push(b"data: 123456789\n\n").unwrap_err();
        assert!(error.to_string().contains("SSE event exceeded"));

        let mut total_limited = SseDecoder::new(8, 8);
        let error = total_limited.push(b"123456789").unwrap_err();
        assert!(error.to_string().contains("stream exceeded"));
    }

    #[test]
    fn decoder_dispatches_bounded_tail_at_eof() {
        let mut decoder = SseDecoder::new(128, 64);
        assert!(decoder.push(b"data: [DONE]").unwrap().is_empty());
        assert_eq!(decoder.finish().unwrap()[0].data, "[DONE]");
    }

    #[test]
    fn decoder_accepts_all_sse_line_endings_and_byte_drip() {
        for separator in ["\n\n", "\r\r", "\r\n\r\n", "\n\r\n", "\r\n\n"] {
            let mut decoder = SseDecoder::new(128, 64);
            let framed = format!("data: ok{separator}");
            let mut events = Vec::new();
            for byte in framed.as_bytes() {
                events.extend(decoder.push(std::slice::from_ref(byte)).unwrap());
            }
            assert_eq!(
                events,
                vec![SseEvent {
                    event: None,
                    data: "ok".to_string()
                }]
            );
        }

        let mut decoder = SseDecoder::new(128 * 1024, 128 * 1024);
        for byte in vec![b'x'; 64 * 1024] {
            assert!(decoder.push(&[byte]).unwrap().is_empty());
            assert!(decoder.scan_from >= decoder.buffer.len().saturating_sub(3));
        }
    }

    #[test]
    fn decoder_rejects_event_count_amplification() {
        let mut decoder = SseDecoder::new(1024, 64);
        decoder.emitted_events = MAX_SSE_EVENTS;
        let error = decoder.push(b"data: x\n\n").unwrap_err();
        assert!(error.to_string().contains("event safety limit"));
    }

    #[test]
    fn diagnostics_are_utf8_safe_and_redact_before_truncating() {
        let source = format!("prefix secret {}", "é".repeat(100));
        let bounded = bounded_redacted(&source, 64, &["secret"]);
        assert!(!bounded.contains("secret"));
        assert!(bounded.is_char_boundary(bounded.len()));
        assert!(bounded.len() <= 64);
        assert!(bounded.contains("truncated"));
    }

    #[test]
    fn repeated_short_secret_is_rebounded_after_redaction() {
        let source = "!".repeat(MAX_ERROR_BODY_BYTES);
        let bounded = bounded_redacted(&source, MAX_ERROR_BODY_BYTES, &["!"]);
        assert!(!bounded.contains('!'));
        assert!(bounded.len() <= MAX_ERROR_BODY_BYTES);
        assert!(bounded.contains("truncated"));
    }

    #[test]
    fn endpoint_rejects_embedded_credentials_without_reflecting_them() {
        let error = validated_endpoint(
            "https://user:launch-secret@example.com/v1?token=launch-secret",
            "chat/completions",
            "test",
        )
        .unwrap_err();
        assert!(!error.to_string().contains("launch-secret"));
        assert_eq!(
            validated_endpoint("http://localhost:11434/", "/v1/chat/completions", "test").unwrap(),
            "http://localhost:11434/v1/chat/completions"
        );
    }

    #[tokio::test]
    async fn error_body_idle_and_success_request_deadlines_are_enforced() {
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            socket
                .write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 10\r\nConnection: close\r\n\r\nx")
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_secs(1)).await;
        });
        let response = http_client()
            .get(format!("http://{address}/"))
            .send()
            .await
            .unwrap();
        let message = read_error_text_with_idle(response, &[], Duration::from_millis(20)).await;
        assert!(message.contains("timed out"));
        server.abort();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            socket
                .write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 100\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            for _ in 0..100 {
                if socket.write_all(b"x").await.is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        let response = http_client()
            .get(format!("http://{address}/"))
            .send()
            .await
            .unwrap();
        let message = read_error_text_with_limits(
            response,
            &[],
            Duration::from_millis(200),
            Duration::from_millis(30),
        )
        .await;
        assert!(message.contains("total body deadline"));
        server.abort();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 10\r\nConnection: close\r\n\r\n{\"a\":")
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_secs(1)).await;
        });
        let started = tokio::time::Instant::now();
        let response = http_client()
            .get(format!("http://{address}/"))
            .timeout(Duration::from_millis(20))
            .send()
            .await
            .unwrap();
        let error = read_json::<serde_json::Value>(response, "test")
            .await
            .unwrap_err();
        assert!(matches!(error, ProviderError::Network(_)));
        assert!(started.elapsed() < Duration::from_millis(500));
        server.abort();
    }

    #[tokio::test]
    async fn provider_client_does_not_follow_redirects_with_credentials() {
        use tokio::io::AsyncWriteExt;

        let recipient = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let recipient_address = recipient.local_addr().unwrap();
        let redirector = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let redirector_address = redirector.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = redirector.accept().await.unwrap();
            let response = format!(
                "HTTP/1.1 302 Found\r\nLocation: http://{recipient_address}/capture\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        let response = http_client()
            .post(format!("http://{redirector_address}/model"))
            .header("x-goog-api-key", "credential")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::FOUND);
        assert!(
            tokio::time::timeout(Duration::from_millis(30), recipient.accept())
                .await
                .is_err()
        );
        server.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn total_stream_deadline_stops_an_indefinite_peer() {
        let mut stream = tokio_stream::pending::<()>();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let error = next_stream_item(&mut stream, deadline, Duration::from_secs(60), "test")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("total timeout"));
    }
}
