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
    let model = settings.ai_model.trim().to_string();
    let client = reqwest::Client::builder()
        .timeout(TIMEOUT)
        .build()
        .map_err(|e| format!("Couldn't create HTTP client: {e}"))?;

    match provider {
        AiProvider::OpenAiCompatible => {
            if model.is_empty() {
                return Err("Set a model name in Modkit settings.".to_string());
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

fn resolve_api_key(provider: AiProvider, configured: &str) -> Option<String> {
    let configured = configured.trim();
    if !configured.is_empty() {
        return Some(configured.to_string());
    }
    let mut vars = vec![provider.key_env_var()];
    if provider == AiProvider::OpenAiCompatible {
        vars.push("OPENAI_API_KEY");
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
            let json = send(request).await?;
            openai_text(&json)
        })
    }
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
            let json = send(request).await?;
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

async fn send(request: reqwest::RequestBuilder) -> Result<Value, String> {
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
    serde_json::from_str(&text).map_err(|e| format!("Invalid JSON ({e}): {}", truncate(&text)))
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
    fn openai_needs_model() {
        let settings = ModkitSettings::default();
        assert!(provider_from_settings(&settings).is_err());
        let settings = ModkitSettings {
            ai_model: "m".into(),
            ..Default::default()
        };
        assert!(provider_from_settings(&settings).is_ok());
    }
}
