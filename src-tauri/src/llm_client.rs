use crate::settings::PostProcessProvider;
use log::debug;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE, REFERER, USER_AGENT};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Serialize)]
struct ChatMessage {
    role: String,
    content: String,
}

#[derive(Debug, Serialize)]
struct JsonSchema {
    name: String,
    strict: bool,
    schema: Value,
}

#[derive(Debug, Serialize)]
struct ResponseFormat {
    #[serde(rename = "type")]
    format_type: String,
    json_schema: JsonSchema,
}

#[derive(Debug, Serialize)]
struct ChatCompletionRequest {
    model: String,
    messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<ResponseFormat>,
}

#[derive(Debug, Deserialize)]
struct ChatCompletionResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ChatMessageResponse,
}

#[derive(Debug, Deserialize)]
struct ChatMessageResponse {
    content: Option<String>,
}

/// Build headers for API requests based on provider type
fn build_headers(provider: &PostProcessProvider, api_key: &str) -> Result<HeaderMap, String> {
    let mut headers = HeaderMap::new();

    // Common headers
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(
        REFERER,
        HeaderValue::from_static("https://github.com/cjpais/Handy"),
    );
    headers.insert(
        USER_AGENT,
        HeaderValue::from_static("Handy/1.0 (+https://github.com/cjpais/Handy)"),
    );
    headers.insert("X-Title", HeaderValue::from_static("Handy"));

    // Provider-specific auth headers
    if !api_key.is_empty() {
        if provider.id == "anthropic" {
            headers.insert(
                "x-api-key",
                HeaderValue::from_str(api_key)
                    .map_err(|e| format!("Invalid API key header value: {}", e))?,
            );
            headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        } else {
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {}", api_key))
                    .map_err(|e| format!("Invalid authorization header value: {}", e))?,
            );
        }
    }

    Ok(headers)
}

/// Build a status-only error message for a non-success response.
///
/// The remote body is never read: a post-processing provider's error payload
/// can echo request content (including the transcribed text being
/// processed), so reading it would risk leaking potentially sensitive data
/// into the UI/logs and lets a remote server force an unbounded allocation.
/// The response (and its connection) is dropped unread; reqwest closes the
/// connection rather than reuse it, which is an acceptable trade-off for not
/// consuming an attacker-controlled body.
fn sanitized_status_error(response: &reqwest::Response, prefix: &str) -> String {
    let status = response.status();
    format!("{prefix} (status {status})")
}

/// Create an HTTP client with provider-specific headers
fn create_client(provider: &PostProcessProvider, api_key: &str) -> Result<reqwest::Client, String> {
    let headers = build_headers(provider, api_key)?;
    reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {}", e))
}

/// Send a chat completion request to an OpenAI-compatible API
/// Returns Ok(Some(content)) on success, Ok(None) if response has no content,
/// or Err on actual errors (HTTP, parsing, etc.)
pub async fn send_chat_completion(
    provider: &PostProcessProvider,
    api_key: String,
    model: &str,
    prompt: String,
) -> Result<Option<String>, String> {
    send_chat_completion_with_schema(provider, api_key, model, prompt, None, None).await
}

/// Send a chat completion request with structured output support
/// When json_schema is provided, uses structured outputs mode
/// system_prompt is used as the system message when provided
pub async fn send_chat_completion_with_schema(
    provider: &PostProcessProvider,
    api_key: String,
    model: &str,
    user_content: String,
    system_prompt: Option<String>,
    json_schema: Option<Value>,
) -> Result<Option<String>, String> {
    // Route Gemini requests to the dedicated Gemini client
    if provider.id == "gemini" {
        let sys = system_prompt.unwrap_or_default();
        match crate::gemini_client::generate_text(&api_key, model, &sys, &user_content).await {
            Ok(text) if !text.is_empty() => return Ok(Some(text)),
            Ok(_) => return Ok(None),
            Err(e) => return Err(format!("Gemini API error: {}", e)),
        }
    }

    let base_url = provider.base_url.trim_end_matches('/');
    let url = format!("{}/chat/completions", base_url);

    debug!("Sending chat completion request to: {}", url);

    let client = create_client(provider, &api_key)?;

    // Build messages vector
    let mut messages = Vec::new();

    // Add system prompt if provided
    if let Some(system) = system_prompt {
        messages.push(ChatMessage {
            role: "system".to_string(),
            content: system,
        });
    }

    // Add user message
    messages.push(ChatMessage {
        role: "user".to_string(),
        content: user_content,
    });

    // Build response_format if schema is provided
    let response_format = json_schema.map(|schema| ResponseFormat {
        format_type: "json_schema".to_string(),
        json_schema: JsonSchema {
            name: "transcription_output".to_string(),
            strict: true,
            schema,
        },
    });

    let request_body = ChatCompletionRequest {
        model: model.to_string(),
        messages,
        response_format,
    };

    let response = client
        .post(&url)
        .json(&request_body)
        .send()
        .await
        .map_err(|e| format!("HTTP request failed: {}", e))?;

    if !response.status().is_success() {
        return Err(sanitized_status_error(&response, "API request failed"));
    }

    let completion: ChatCompletionResponse = response
        .json()
        .await
        .map_err(|e| format!("Failed to parse API response: {}", e))?;

    Ok(completion
        .choices
        .first()
        .and_then(|choice| choice.message.content.clone()))
}

/// Fetch available models from an OpenAI-compatible API
/// Returns a list of model IDs
pub async fn fetch_models(
    provider: &PostProcessProvider,
    api_key: String,
) -> Result<Vec<String>, String> {
    // Gemini uses a different API format for listing models
    if provider.id == "gemini" {
        return fetch_gemini_models(&api_key).await;
    }

    let base_url = provider.base_url.trim_end_matches('/');
    let url = format!("{}/models", base_url);

    debug!("Fetching models from: {}", url);

    let client = create_client(provider, &api_key)?;

    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("Failed to fetch models: {}", e))?;

    if !response.status().is_success() {
        return Err(sanitized_status_error(
            &response,
            "Model list request failed",
        ));
    }

    let parsed: serde_json::Value = response
        .json()
        .await
        .map_err(|e| format!("Failed to parse response: {}", e))?;

    let mut models = Vec::new();

    // Handle OpenAI format: { data: [ { id: "..." }, ... ] }
    if let Some(data) = parsed.get("data").and_then(|d| d.as_array()) {
        for entry in data {
            if let Some(id) = entry.get("id").and_then(|i| i.as_str()) {
                models.push(id.to_string());
            } else if let Some(name) = entry.get("name").and_then(|n| n.as_str()) {
                models.push(name.to_string());
            }
        }
    }
    // Handle array format: [ "model1", "model2", ... ]
    else if let Some(array) = parsed.as_array() {
        for entry in array {
            if let Some(model) = entry.as_str() {
                models.push(model.to_string());
            }
        }
    }

    Ok(models)
}

async fn fetch_gemini_models(api_key: &str) -> Result<Vec<String>, String> {
    let url = "https://generativelanguage.googleapis.com/v1beta/models";

    let client = reqwest::Client::new();
    let response = client
        .get(url)
        .header("x-goog-api-key", api_key)
        .send()
        .await
        .map_err(|e| format!("Failed to fetch Gemini models: {}", e))?;

    if !response.status().is_success() {
        return Err(sanitized_status_error(
            &response,
            "Gemini model list request failed",
        ));
    }

    let parsed: serde_json::Value = response
        .json()
        .await
        .map_err(|e| format!("Failed to parse Gemini response: {}", e))?;

    let mut models = Vec::new();
    if let Some(data) = parsed.get("models").and_then(|d| d.as_array()) {
        for entry in data {
            if let Some(name) = entry.get("name").and_then(|n| n.as_str()) {
                // Gemini returns "models/gemini-2.5-flash" - strip the prefix
                let model_id = name.strip_prefix("models/").unwrap_or(name);
                if model_id.contains("gemini") {
                    models.push(model_id.to_string());
                }
            }
        }
    }

    Ok(models)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::Duration;

    /// A value that must never appear in a returned/logged error: providers
    /// can echo request content (potentially transcribed user speech) in
    /// their error bodies.
    const SENTINEL: &str = "super-secret-transcription-sentinel-should-never-leak";

    /// Minimal one-shot local HTTP server, mirroring the pattern already used
    /// by the ElevenLabs/Gemini transcription client tests.
    fn spawn_mock(response: String) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut socket, _)) = listener.accept() {
                let _ = socket.set_read_timeout(Some(Duration::from_millis(100)));
                let mut buf = [0u8; 4096];
                loop {
                    match socket.read(&mut buf) {
                        Ok(0) => break,
                        Ok(_) => {}
                        Err(_) => break, // timeout: request fully read
                    }
                }
                let _ = socket.write_all(response.as_bytes());
                let _ = socket.flush();
            }
        });
        format!("http://{addr}")
    }

    fn http_response(status_line: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            status_line,
            body.as_bytes().len(),
            body
        )
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    fn test_provider(base_url: String) -> PostProcessProvider {
        PostProcessProvider {
            id: "test-provider".into(),
            label: "Test".into(),
            base_url,
            allow_base_url_edit: true,
            models_endpoint: None,
            supports_structured_output: false,
        }
    }

    #[test]
    fn sanitized_status_error_never_reads_the_body() {
        let base_url = spawn_mock(http_response("429 Too Many Requests", SENTINEL));
        let message = runtime().block_on(async {
            let response = reqwest::Client::new().get(&base_url).send().await.unwrap();
            sanitized_status_error(&response, "request failed")
        });

        assert!(!message.contains(SENTINEL), "leaked remote body: {message}");
        assert!(message.contains("429"));
    }

    #[test]
    fn sanitized_status_error_does_not_block_on_an_unbounded_body() {
        // A body far larger than any reasonable buffer, and never fully sent
        // (no Connection: close, server never finishes writing): if the
        // status-only path ever read the body again, this would hang/alloc
        // instead of returning immediately.
        let huge = "x".repeat(50 * 1024 * 1024);
        let base_url = spawn_mock(format!(
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: {}\r\n\r\n{}",
            huge.len() * 2,
            huge
        ));
        let message = runtime().block_on(async {
            let response = reqwest::Client::new().get(&base_url).send().await.unwrap();
            sanitized_status_error(&response, "request failed")
        });
        assert!(message.contains("500"));
    }

    #[test]
    fn chat_completion_error_never_leaks_the_remote_body() {
        let base_url = spawn_mock(http_response(
            "401 Unauthorized",
            &format!(r#"{{"error":"{SENTINEL}"}}"#),
        ));
        let provider = test_provider(base_url);

        let err = runtime()
            .block_on(send_chat_completion(
                &provider,
                "key".into(),
                "model",
                "prompt".into(),
            ))
            .unwrap_err();

        assert!(!err.contains(SENTINEL), "error leaked remote body: {err}");
        assert!(err.contains("401"));
    }

    #[test]
    fn fetch_models_error_never_leaks_the_remote_body() {
        let base_url = spawn_mock(http_response(
            "500 Internal Server Error",
            &format!(r#"{{"error":"{SENTINEL}"}}"#),
        ));
        let provider = test_provider(base_url);

        let err = runtime()
            .block_on(fetch_models(&provider, "key".into()))
            .unwrap_err();

        assert!(!err.contains(SENTINEL), "error leaked remote body: {err}");
    }
}
