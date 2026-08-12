//! Provider-neutral model interfaces and model provider adapters.

#![allow(dead_code)]

use crate::audit::sha256_hex;
use crate::store::{Store, StoreError};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fmt;
use std::time::Duration;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct AgentPlan {
    pub(crate) version: u32,
    pub(crate) summary: String,
    pub(crate) actions: Vec<PlanAction>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum PlanAction {
    Read { path: String },
    Write { path: String, contents: String },
    GateContains { path: String, expected: String },
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum PlanError {
    Decode(String),
    Invalid(String),
}

impl fmt::Display for PlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Decode(message) => write!(formatter, "model plan JSON error: {message}"),
            Self::Invalid(message) => write!(formatter, "invalid model plan: {message}"),
        }
    }
}

impl std::error::Error for PlanError {}

impl AgentPlan {
    pub(crate) fn parse(text: &str) -> Result<Self, PlanError> {
        let plan: Self =
            serde_json::from_str(text).map_err(|error| PlanError::Decode(error.to_string()))?;
        plan.validate()?;
        Ok(plan)
    }

    fn validate(&self) -> Result<(), PlanError> {
        if self.version != 1 {
            return Err(PlanError::Invalid(format!(
                "unsupported version {}; expected 1",
                self.version
            )));
        }
        if self.summary.trim().is_empty() {
            return Err(PlanError::Invalid("summary is empty".to_owned()));
        }
        if self.actions.is_empty() || self.actions.len() > 16 {
            return Err(PlanError::Invalid(
                "actions must contain between 1 and 16 items".to_owned(),
            ));
        }
        for action in &self.actions {
            let path = match action {
                PlanAction::Read { path }
                | PlanAction::Write { path, .. }
                | PlanAction::GateContains { path, .. } => path,
            };
            validate_relative_path(path)?;
            if let PlanAction::GateContains { expected, .. } = action
                && expected.is_empty()
            {
                return Err(PlanError::Invalid("gate expected text is empty".to_owned()));
            }
        }
        Ok(())
    }
}

fn validate_relative_path(path: &str) -> Result<(), PlanError> {
    let path_object = std::path::Path::new(path);
    if path.is_empty()
        || path.contains('\0')
        || path.starts_with('/')
        || path.starts_with('\\')
        || path.contains(':')
        || path_object
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(PlanError::Invalid(format!(
            "path `{path}` must be a safe relative path"
        )));
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Message {
    pub(crate) role: String,
    pub(crate) content: String,
}

impl Message {
    pub(crate) fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_owned(),
            content: content.into(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ModelRequest {
    pub(crate) model: String,
    pub(crate) messages: Vec<Message>,
    pub(crate) temperature: f32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ModelResponse {
    pub(crate) text: String,
    pub(crate) provider_request_id: Option<String>,
}

#[derive(Debug)]
pub(crate) enum ModelError {
    Configuration(String),
    Http(String),
    Provider { status: u16, body: String },
    Decode(String),
    Audit(StoreError),
}

impl fmt::Display for ModelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Configuration(message) => {
                write!(formatter, "model configuration error: {message}")
            }
            Self::Http(message) => write!(formatter, "model HTTP error: {message}"),
            Self::Provider { status, body } => {
                write!(formatter, "model provider returned HTTP {status}: {body}")
            }
            Self::Decode(message) => write!(formatter, "model response decode error: {message}"),
            Self::Audit(error) => write!(formatter, "model audit error: {error}"),
        }
    }
}

impl std::error::Error for ModelError {}

impl From<StoreError> for ModelError {
    fn from(error: StoreError) -> Self {
        Self::Audit(error)
    }
}

pub(crate) trait ModelProvider {
    fn complete(&self, request: &ModelRequest) -> Result<ModelResponse, ModelError>;
}

#[derive(Clone, Debug)]
pub(crate) struct FakeProvider {
    response: String,
}

impl FakeProvider {
    pub(crate) fn new(response: impl Into<String>) -> Self {
        Self {
            response: response.into(),
        }
    }
}

impl ModelProvider for FakeProvider {
    fn complete(&self, _request: &ModelRequest) -> Result<ModelResponse, ModelError> {
        Ok(ModelResponse {
            text: self.response.clone(),
            provider_request_id: Some("fake-provider".to_owned()),
        })
    }
}

pub(crate) struct OpenAiCompatibleProvider {
    endpoint: String,
    api_key: String,
    client: Client,
    max_retries: usize,
}

pub(crate) struct GeminiProvider {
    api_key: String,
    model: String,
    client: Client,
    max_retries: usize,
}

impl GeminiProvider {
    pub(crate) fn new(
        api_key: impl Into<String>,
        model: impl Into<String>,
        timeout: Duration,
        max_retries: usize,
    ) -> Result<Self, ModelError> {
        let api_key = api_key.into();
        if api_key.trim().is_empty() {
            return Err(ModelError::Configuration(
                "Gemini API key is empty".to_owned(),
            ));
        }
        let model = model.into();
        if model.trim().is_empty() {
            return Err(ModelError::Configuration(
                "Gemini model is empty".to_owned(),
            ));
        }
        let client = Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|error| ModelError::Configuration(error.to_string()))?;
        Ok(Self {
            api_key,
            model,
            client,
            max_retries,
        })
    }
}

impl ModelProvider for GeminiProvider {
    fn complete(&self, request: &ModelRequest) -> Result<ModelResponse, ModelError> {
        let prompt = request
            .messages
            .iter()
            .map(|message| format!("{}: {}", message.role, message.content))
            .collect::<Vec<_>>()
            .join("\n");
        let body = json!({"contents": [{"role": "user", "parts": [{"text": prompt}]}], "generationConfig": {"temperature": request.temperature, "maxOutputTokens": 2048}});
        let endpoint = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent",
            self.model
        );
        for attempt in 0..=self.max_retries {
            let response = self
                .client
                .post(&endpoint)
                .query(&[("key", &self.api_key)])
                .json(&body)
                .send();
            match response {
                Ok(response) => {
                    let status = response.status();
                    let response_body = response
                        .text()
                        .map_err(|error| ModelError::Http(error.to_string()))?;
                    if status.is_success() {
                        return parse_gemini_response(&response_body);
                    }
                    if !status.is_server_error() || attempt == self.max_retries {
                        return Err(ModelError::Provider {
                            status: status.as_u16(),
                            body: redact_text(&response_body),
                        });
                    }
                }
                Err(_error) if attempt == self.max_retries => {
                    return Err(ModelError::Http(
                        "Gemini request failed after retries".to_owned(),
                    ));
                }
                Err(_) => {}
            }
            std::thread::sleep(Duration::from_millis(50 * 2_u64.pow(attempt as u32)));
        }
        Err(ModelError::Http("retry loop exhausted".to_owned()))
    }
}

pub(crate) enum ConfiguredProvider {
    Fake(FakeProvider),
    Gemini(GeminiProvider),
}

impl ModelProvider for ConfiguredProvider {
    fn complete(&self, request: &ModelRequest) -> Result<ModelResponse, ModelError> {
        match self {
            Self::Fake(provider) => provider.complete(request),
            Self::Gemini(provider) => provider.complete(request),
        }
    }
}

impl OpenAiCompatibleProvider {
    pub(crate) fn new(
        endpoint: impl Into<String>,
        api_key: impl Into<String>,
        timeout: Duration,
        max_retries: usize,
    ) -> Result<Self, ModelError> {
        let endpoint = endpoint.into();
        if endpoint.is_empty() {
            return Err(ModelError::Configuration("endpoint is empty".to_owned()));
        }
        let api_key = api_key.into();
        if api_key.is_empty() {
            return Err(ModelError::Configuration("API key is empty".to_owned()));
        }
        let client = Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|error| ModelError::Configuration(error.to_string()))?;
        Ok(Self {
            endpoint,
            api_key,
            client,
            max_retries,
        })
    }
}

impl ModelProvider for OpenAiCompatibleProvider {
    fn complete(&self, request: &ModelRequest) -> Result<ModelResponse, ModelError> {
        let body = json!({
            "model": request.model,
            "messages": request.messages,
            "temperature": request.temperature,
        });

        for attempt in 0..=self.max_retries {
            let response = self
                .client
                .post(&self.endpoint)
                .bearer_auth(&self.api_key)
                .json(&body)
                .send();
            match response {
                Ok(response) => {
                    let status = response.status();
                    let response_body = response
                        .text()
                        .map_err(|error| ModelError::Http(error.to_string()))?;
                    if status.is_success() {
                        return parse_openai_response(&response_body);
                    }
                    if !status.is_server_error() || attempt == self.max_retries {
                        return Err(ModelError::Provider {
                            status: status.as_u16(),
                            body: redact_text(&response_body),
                        });
                    }
                }
                Err(error) => {
                    if attempt == self.max_retries {
                        return Err(ModelError::Http(error.to_string()));
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(50 * 2_u64.pow(attempt as u32)));
        }
        Err(ModelError::Http("retry loop exhausted".to_owned()))
    }
}

pub(crate) fn complete_with_audit<P: ModelProvider>(
    store: &Store,
    run_id: &str,
    step_index: usize,
    provider: &P,
    request: &ModelRequest,
) -> Result<ModelResponse, ModelError> {
    let request_json =
        serde_json::to_value(request).map_err(|error| ModelError::Decode(error.to_string()))?;
    let redacted_request = redact_value(request_json);
    let request_payload = serde_json::to_string(&redacted_request)
        .map_err(|error| ModelError::Decode(error.to_string()))?;
    store.record_event(
        run_id,
        "llm.request",
        Some(step_index),
        &format!("request={request_payload}"),
    )?;

    match provider.complete(request) {
        Ok(response) => {
            store.record_event(
                run_id,
                "llm.response",
                Some(step_index),
                &format!(
                    "request_id={} text_sha256={}",
                    response.provider_request_id.as_deref().unwrap_or("unknown"),
                    sha256_hex(response.text.as_bytes())
                ),
            )?;
            Ok(response)
        }
        Err(error) => {
            let _ = store.record_event(
                run_id,
                "llm.error",
                Some(step_index),
                &redact_text(&error.to_string()),
            );
            Err(error)
        }
    }
}

fn parse_openai_response(body: &str) -> Result<ModelResponse, ModelError> {
    let value: Value =
        serde_json::from_str(body).map_err(|error| ModelError::Decode(error.to_string()))?;
    let text = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"))
        .and_then(|message| message.get("content"))
        .and_then(Value::as_str)
        .ok_or_else(|| ModelError::Decode("missing choices[0].message.content".to_owned()))?;
    Ok(ModelResponse {
        text: text.to_owned(),
        provider_request_id: value.get("id").and_then(Value::as_str).map(str::to_owned),
    })
}

fn parse_gemini_response(body: &str) -> Result<ModelResponse, ModelError> {
    let value: Value =
        serde_json::from_str(body).map_err(|error| ModelError::Decode(error.to_string()))?;
    let text = value
        .get("candidates")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
        .and_then(|item| item.get("content"))
        .and_then(|content| content.get("parts"))
        .and_then(Value::as_array)
        .and_then(|parts| parts.first())
        .and_then(|part| part.get("text"))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            ModelError::Decode("missing candidates[0].content.parts[0].text".to_owned())
        })?;
    Ok(ModelResponse {
        text: text.to_owned(),
        provider_request_id: value
            .get("responseId")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

fn redact_value(mut value: Value) -> Value {
    redact_value_in_place(&mut value);
    value
}

fn redact_value_in_place(value: &mut Value) {
    match value {
        Value::Object(object) => {
            for (key, child) in object.iter_mut() {
                let lower = key.to_ascii_lowercase();
                if lower.contains("key")
                    || lower.contains("token")
                    || lower.contains("secret")
                    || lower.contains("authorization")
                {
                    *child = Value::String("[REDACTED]".to_owned());
                } else {
                    redact_value_in_place(child);
                }
            }
        }
        Value::Array(array) => {
            for child in array {
                redact_value_in_place(child);
            }
        }
        _ => {}
    }
}

fn redact_text(value: &str) -> String {
    value
        .replace("Bearer ", "Bearer [REDACTED] ")
        .replace("api_key=", "api_key=[REDACTED]")
        .replace("token=", "token=[REDACTED]")
}

#[cfg(test)]
mod tests {
    use super::{AgentPlan, FakeProvider, Message, ModelRequest, PlanAction, complete_with_audit};
    use crate::run::{StepDefinition, new_run_id};
    use crate::store::Store;
    use std::path::PathBuf;

    fn temporary_store() -> PathBuf {
        std::env::temp_dir().join(format!("agentrt-model-{}.db", new_run_id()))
    }

    #[test]
    fn fake_provider_is_deterministic_and_audited() {
        let path = temporary_store();
        let run_id = new_run_id();
        let definitions = StepDefinition::sequence(1);
        let store = Store::open(&path).expect("store opens");
        store
            .create_run(&run_id, &definitions)
            .expect("run creates");
        store.mark_running(&run_id).expect("run starts");
        let request = ModelRequest {
            model: "fake-model".to_owned(),
            messages: vec![Message::user("hello")],
            temperature: 0.0,
        };
        let provider = FakeProvider::new("stable response");
        let first =
            complete_with_audit(&store, &run_id, 0, &provider, &request).expect("first completion");
        let second = complete_with_audit(&store, &run_id, 0, &provider, &request)
            .expect("second completion");
        assert_eq!(first.text, "stable response");
        assert_eq!(first.text, second.text);
        let events = store.load_events(&run_id).expect("events load");
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "llm.request")
                .count(),
            2
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "llm.response")
                .count(),
            2
        );
        drop(store);
        std::fs::remove_file(path).expect("store removes");
    }

    #[test]
    fn structured_plan_parses_typed_actions() {
        let plan = AgentPlan::parse(
            r#"{
                "version": 1,
                "summary": "repair the fixture",
                "actions": [
                    {"kind": "read", "path": "fixture.txt"},
                    {"kind": "write", "path": "fixture.txt", "contents": "fixed"},
                    {"kind": "gate_contains", "path": "fixture.txt", "expected": "fixed"}
                ]
            }"#,
        )
        .expect("plan parses");
        assert_eq!(plan.version, 1);
        assert_eq!(plan.actions.len(), 3);
        assert!(matches!(plan.actions[0], PlanAction::Read { .. }));
    }

    #[test]
    fn structured_plan_rejects_unsafe_paths() {
        let result = AgentPlan::parse(
            r#"{
                "version": 1,
                "summary": "escape",
                "actions": [{"kind": "read", "path": "../secret.txt"}]
            }"#,
        );
        assert!(matches!(result, Err(super::PlanError::Invalid(_))));
    }
}
