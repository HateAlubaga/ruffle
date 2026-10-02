//! LLM providers for Modkit's AI edits.
//!
//! The core builds the prompts and validates the answer, so a provider only
//! has to turn `(system, user)` into the model's text reply.

use ruffle_core::modkit::{AiProvider, ModkitSettings};
use serde_json::{Value, json};
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(120);
const MAX_TOKENS: u32 = 4096;
const DEFAULT_ANTHROPIC_MODEL: &str = "claude-sonnet-5";
/// Used when the settings leave the model empty.
const MODEL_ENV_VAR: &str = "RUFFLE_MODKIT_MODEL";
/// Local OpenAI-compatible gateways (OmniRoute) commonly use this one.
const GATEWAY_KEY_ENV_VAR: &str = "OMNIROUTE_API_KEY";

pub type CompletionFuture = Pin<Box<dyn Future<Output = Result<String, String>> + Send>>;

pub trait LlmProvider: Send {
    fn complete(&self, system: String, user: String) -> CompletionFuture;
}

pub fn provider_from_settings(settings: &ModkitSettings) -> Result<Box<dyn LlmProvider>, String> {
    let provider = settings.ai_provider;
    let base_url = match settings.ai_base_url.trim() {
        "" => provider.default_base_url(),
        url => url,
    }
    .trim_end_matches('/')
    .to_string();
    let api_key = resolve_api_key(provider, &settings.ai_api_key);
    let model = resolve_model(&settings.ai_model);
    let client = reqwest::Client::builder()
        .timeout(TIMEOUT)
        .build()
        .map_err(|e| format!("Couldn't create HTTP client: {e}"))?;

    match provider {
        AiProvider::OpenAiCompatible => {
            if model.is_empty() {
                return Err(format!(
                    "Set a model name in Modkit settings or in {MODEL_ENV_VAR}."
                ));
            }
            Ok(Box::new(OpenAiCompatible {
                client,
                base_url,
                api_key,
                model,
            }))
        }
        AiProvider::Anthropic => {
            let Some(api_key) = api_key else {
                return Err(format!(
                    "No API key: set one in Modkit settings or in {}.",
                    provider.key_env_var()
                ));
            };
            Ok(Box::new(Anthropic {
                client,
                base_url,
                api_key,
                model: if model.is_empty() {
                    DEFAULT_ANTHROPIC_MODEL.to_string()
                } else {
                    model
                },
            }))
        }
    }
}

fn resolve_model(configured: &str) -> String {
    pick_model(configured, std::env::var(MODEL_ENV_VAR).ok())
}

fn pick_model(configured: &str, from_env: Option<String>) -> String {
    let configured = configured.trim();
    if !configured.is_empty() {
        return configured.to_string();
    }
    from_env
        .map(|model| model.trim().to_string())
        .unwrap_or_default()
}

fn resolve_api_key(provider: AiProvider, configured: &str) -> Option<String> {
    let configured = configured.trim();
    if !configured.is_empty() {
        return Some(configured.to_string());
    }
    let mut vars = vec![provider.key_env_var()];
    if provider == AiProvider::OpenAiCompatible {
        vars.push("OPENAI_API_KEY");
        vars.push(GATEWAY_KEY_ENV_VAR);
    }
    vars.into_iter()
        .filter_map(|var| std::env::var(var).ok())
        .map(|key| key.trim().to_string())
        .find(|key| !key.is_empty())
}

/// Any server speaking the OpenAI `/chat/completions` API.
struct OpenAiCompatible {
    client: reqwest::Client,
    base_url: String,
    /// Optional: local servers usually don't need one.
    api_key: Option<String>,
    model: String,
}

impl LlmProvider for OpenAiCompatible {
    fn complete(&self, system: String, user: String) -> CompletionFuture {
        let body = json!({
            "model": self.model,
            "temperature": 0,
            "max_tokens": MAX_TOKENS,
            // Some gateways stream unless told not to.
            "stream": false,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user},
            ],
        });
        let mut request = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .json(&body);
        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }
        Box::pin(async move {
            let text = send(request).await?;
            openai_reply(&text)
        })
    }
}

/// Parses a `/chat/completions` reply. A server that ignores `"stream": false`
/// answers with server-sent events, so those are accepted as well.
fn openai_reply(body: &str) -> Result<String, String> {
    if body.trim_start().starts_with("data:") {
        return sse_text(body);
    }
    openai_text(&parse_json(body)?)
}

fn sse_text(body: &str) -> Result<String, String> {
    let mut text = String::new();
    for line in body.lines() {
        let Some(data) = line.strip_prefix("data:").map(str::trim) else {
            continue;
        };
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let chunk = parse_json(data)?;
        if let Some(error) = chunk.get("error") {
            return Err(format!("Server error: {}", truncate(&error.to_string())));
        }
        if let Some(delta) = chunk
            .pointer("/choices/0/delta/content")
            .and_then(Value::as_str)
        {
            text.push_str(delta);
        }
    }
    if text.is_empty() {
        return Err("The model returned no text.".to_string());
    }
    Ok(text)
}

fn openai_text(json: &Value) -> Result<String, String> {
    json.pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("Unexpected response: {}", truncate(&json.to_string())))
}

struct Anthropic {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
}

impl LlmProvider for Anthropic {
    fn complete(&self, system: String, user: String) -> CompletionFuture {
        let body = json!({
            "model": self.model,
            "max_tokens": MAX_TOKENS,
            "temperature": 0,
            "system": system,
            "messages": [{"role": "user", "content": user}],
        });
        let request = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&body);
        Box::pin(async move {
            let json = parse_json(&send(request).await?)?;
            anthropic_text(&json)
        })
    }
}

fn anthropic_text(json: &Value) -> Result<String, String> {
    let blocks = json
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("Unexpected response: {}", truncate(&json.to_string())))?;
    let text: String = blocks
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect();
    if text.is_empty() {
        return Err("The model returned no text.".to_string());
    }
    Ok(text)
}

async fn send(request: reqwest::RequestBuilder) -> Result<String, String> {
    let response = request
        .send()
        .await
        .map_err(|e| format!("Request failed: {e}"))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| format!("Couldn't read response: {e}"))?;
    if !status.is_success() {
        return Err(format!("HTTP {status}: {}", truncate(&text)));
    }
    Ok(text)
}

fn parse_json(text: &str) -> Result<Value, String> {
    serde_json::from_str(text).map_err(|e| format!("Invalid JSON ({e}): {}", truncate(text)))
}

fn truncate(text: &str) -> String {
    const MAX: usize = 500;
    match text.char_indices().nth(MAX) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_openai_reply() {
        let json = json!({"choices": [{"message": {"role": "assistant", "content": "[]"}}]});
        assert_eq!(openai_text(&json).as_deref(), Ok("[]"));
        assert!(openai_text(&json!({"error": "x"})).is_err());
    }

    #[test]
    fn plain_and_streamed_replies_agree() {
        let plain = r#"{"choices":[{"message":{"content":"[{\"id\":1}]"}}]}"#;
        let streamed = concat!(
            r#"data: {"choices":[{"delta":{"role":"assistant"}}]}"#,
            "\n\n",
            r#"data: {"choices":[{"delta":{"content":"[{\"id\""}}]}"#,
            "\n\n",
            r#"data: {"choices":[{"delta":{"content":":1}]"}}]}"#,
            "\n\n",
            "data: [DONE]\n\n",
        );
        assert_eq!(openai_reply(plain).as_deref(), Ok(r#"[{"id":1}]"#));
        assert_eq!(openai_reply(streamed).as_deref(), Ok(r#"[{"id":1}]"#));
    }

    #[test]
    fn streamed_errors_and_empty_replies_fail() {
        assert!(openai_reply(r#"data: {"error":{"message":"nope"}}"#).is_err());
        assert!(openai_reply("data: [DONE]\n\n").is_err());
        assert!(openai_reply("not json").is_err());
    }

    #[test]
    fn parses_anthropic_reply() {
        let json = json!({"content": [
            {"type": "thinking", "thinking": "hmm"},
            {"type": "text", "text": "[{\"id\":1,"},
            {"type": "text", "text": "\"set\":{}}]"},
        ]});
        assert_eq!(
            anthropic_text(&json).as_deref(),
            Ok("[{\"id\":1,\"set\":{}}]")
        );
        assert!(anthropic_text(&json!({"content": []})).is_err());
    }

    #[test]
    fn configured_key_wins() {
        assert_eq!(
            resolve_api_key(AiProvider::Anthropic, "  abc ").as_deref(),
            Some("abc")
        );
    }

    #[test]
    fn model_comes_from_settings_then_environment() {
        assert_eq!(pick_model(" m ", Some("env".into())), "m");
        assert_eq!(pick_model("", Some(" env ".into())), "env");
        assert_eq!(pick_model("  ", None), "");
    }

    #[test]
    fn openai_needs_model() {
        let settings = ModkitSettings {
            ai_model: "m".into(),
            ..Default::default()
        };
        assert!(provider_from_settings(&settings).is_ok());
    }
}
