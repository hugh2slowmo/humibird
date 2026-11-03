//! Provider-agnostic chat types and the adapters behind them.
//!
//! Most providers speak OpenAI's Chat Completions API — DeepSeek, OpenAI,
//! OpenRouter, Ollama and an arbitrary compatible endpoint — so one adapter covers
//! them and [`PRESETS`] says which one a profile talks to. Anthropic's Messages
//! API is its own adapter. Providers expose a normalized [`StreamEvent`] stream so
//! the agent core never sees provider deltas.

mod anthropic;
mod errors;
mod openai_compatible;
mod presets;
mod streaming;

pub use anthropic::Anthropic;
pub use errors::{ContextExceeded, context_exceeded};
pub use openai_compatible::OpenAiCompatible;
pub use presets::{
    DEFAULT_MODEL, DEFAULT_PROVIDER, PRESETS, Preset, Reasoning, Wire, model_info, models, preset,
};
pub use streaming::StreamDecoder;

use std::sync::Arc;

use futures::future::BoxFuture;
use futures::stream::BoxStream;
use http_client::{AsyncBody, HttpClient, HttpRequestExt as _, Method, Request as HttpRequest};
use serde::{Deserialize, Serialize};

pub type EventStream = BoxStream<'static, anyhow::Result<StreamEvent>>;

/// A model an adapter offers, as the picker shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModelInfo {
    /// The id sent to the provider.
    pub id: &'static str,
    /// What the picker shows: the id is for the wire, this is for the user.
    pub display_name: &'static str,
    /// How much context the model accepts, in tokens. The thread compacts before
    /// a request gets close to this.
    pub context_tokens: u32,
}

/// A chat provider.
pub trait Provider: Send + Sync {
    /// Builds a streaming request in this provider's default shape/model.
    fn request(&self, messages: Vec<Message>) -> Request;

    fn stream(&self, request: Request) -> EventStream;

    /// Asks the provider which models it serves, for one whose list is not known
    /// ahead of time. `None` when it cannot say, which leaves the catalogue the
    /// picker already has.
    fn list_models(&self) -> Option<BoxFuture<'static, anyhow::Result<Vec<DiscoveredModel>>>> {
        None
    }

    /// How much context `model` takes, as this provider describes it. A conversation
    /// measures its transcript against this, so the answer has to come from the
    /// provider it will actually talk to: one provider's `model-x` is not another's.
    fn context_limit(&self, _model: &str) -> Option<u32> {
        None
    }
}

/// A model the provider named itself. Unlike [`ModelInfo`], nothing about it is
/// known at compile time, and the window is only there when the listing reports
/// one (OpenRouter and Anthropic do; OpenAI and DeepSeek do not).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredModel {
    pub id: String,
    /// The provider's label, when it sends one worth showing.
    pub display_name: Option<String>,
    pub context_tokens: Option<u32>,
}

/// How long a provider gets to answer with its model list. A hung endpoint must
/// not leave the picker waiting on a request that will never finish.
const MODEL_LIST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Reads `GET {base_url}/models`, which both wires answer with a `data` array of
/// objects that carry an `id`. Some reports also carry the context window —
/// OpenRouter spells it `context_length`, vLLM `max_model_len`, Anthropic
/// `max_input_tokens` — and those are kept, because a window nobody catalogues is
/// the difference between compacting before a thread is full and after it fails.
/// The list is sorted and deduplicated: it is a menu, not a record of what the
/// provider sent.
pub async fn fetch_models(
    client: Arc<dyn HttpClient>,
    wire: crate::Wire,
    base_url: &str,
    api_key: Option<&str>,
) -> anyhow::Result<Vec<DiscoveredModel>> {
    let endpoint = format!("{}/models", base_url.trim_end_matches('/'));
    let builder = HttpRequest::builder()
        .method(Method::GET)
        .uri(&endpoint)
        .timeout(MODEL_LIST_TIMEOUT);
    let builder = match wire {
        crate::Wire::OpenAiCompatible => builder.when_some(api_key, |builder, key| {
            builder.header("Authorization", format!("Bearer {}", key.trim()))
        }),
        crate::Wire::Anthropic => builder
            .header("anthropic-version", "2023-06-01")
            .when_some(api_key, |builder, key| {
                builder.header("x-api-key", key.trim())
            }),
    };

    let mut response = client.send(builder.body(AsyncBody::default())?).await?;
    if !response.status().is_success() {
        anyhow::bail!("{endpoint} answered {}", response.status());
    }

    let mut body = Vec::new();
    futures::AsyncReadExt::read_to_end(response.body_mut(), &mut body).await?;
    let list: ModelList = serde_json::from_slice(&body)?;

    let mut models: Vec<DiscoveredModel> = list
        .data
        .into_iter()
        .map(|model| DiscoveredModel {
            context_tokens: model
                .max_input_tokens
                .or(model.context_length)
                .and_then(|tokens| u32::try_from(tokens).ok()),
            display_name: model.display_name.or(model.name),
            id: model.id,
        })
        .collect();
    models.sort_by(|left, right| left.id.cmp(&right.id));
    models.dedup_by(|left, right| left.id == right.id);
    Ok(models)
}

#[derive(Deserialize)]
struct ModelList {
    #[serde(default)]
    data: Vec<ModelEntry>,
}

/// One entry of either wire's listing. Every window field is optional and they
/// are spellings of the same idea: Anthropic sends `max_input_tokens`, OpenRouter
/// `context_length`, and vLLM `max_model_len`.
#[derive(Deserialize)]
struct ModelEntry {
    id: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(
        default,
        alias = "context_window",
        alias = "max_context_length",
        alias = "max_model_len"
    )]
    context_length: Option<u64>,
    #[serde(default)]
    max_input_tokens: Option<u64>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// Something the person attached to a turn: a file's text, or an image.
///
/// The transcript keeps these beside the message, because they are part of what
/// the model saw and a reopened thread sends them again. Text is sent inside a
/// `<file>` block, so every wire carries it without a new content type; an image
/// travels as its own part, base64 because that is how both wires take it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Attachment {
    Text {
        name: String,
        text: String,
    },
    Image {
        name: String,
        media_type: String,
        data: String,
    },
}

impl Attachment {
    /// What the composer calls it.
    pub fn name(&self) -> &str {
        match self {
            Self::Text { name, .. } | Self::Image { name, .. } => name,
        }
    }

    pub fn is_image(&self) -> bool {
        matches!(self, Self::Image { .. })
    }

    /// Reads what the transcript stores. Anything unreadable is no attachments:
    /// a row written by a version that knew more must not make a thread
    /// unopenable.
    pub fn list_from_json(json: &str) -> Vec<Self> {
        serde_json::from_str(json).unwrap_or_default()
    }

    /// The transcript's spelling of a turn's attachments. `[]` is the empty list,
    /// which is also what an older row reads as.
    pub fn list_to_json(attachments: &[Self]) -> String {
        serde_json::to_string(attachments).unwrap_or_else(|_| "[]".to_owned())
    }
}

/// The user's own text followed by every text attachment, in the wrapper the
/// model reads. Images are not text and stay out of it.
pub fn user_content(message: &Message) -> String {
    let mut content = message.content.clone();
    for attachment in &message.attachments {
        if let Attachment::Text { name, text } = attachment {
            if !content.is_empty() {
                content.push_str("\n\n");
            }
            content.push_str(&format!("<file name=\"{name}\">\n{text}\n</file>"));
        }
    }
    content
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub role: Role,
    #[serde(
        default,
        serialize_with = "serialize_content",
        deserialize_with = "deserialize_content"
    )]
    pub content: String,
    /// The calls the assistant wants to make, on an assistant message.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// Which call this message answers, on a tool message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// What came with this turn. The wire adapters fold text into the content
    /// and send images beside it; `content` itself stays what the person typed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self::text(Role::System, content)
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::text(Role::User, content)
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self::text(Role::Assistant, content)
    }

    pub fn tool(content: impl Into<String>) -> Self {
        Self::text(Role::Tool, content)
    }

    /// An assistant message that asks for tools instead of answering.
    pub fn tool_calls(calls: Vec<ToolCall>) -> Self {
        Self {
            role: Role::Assistant,
            content: String::new(),
            tool_calls: calls,
            tool_call_id: None,
            attachments: Vec::new(),
        }
    }

    /// The result of one call, fed back to the model.
    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
            attachments: Vec::new(),
        }
    }

    fn text(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            attachments: Vec::new(),
        }
    }
}

/// OpenAI-compatible APIs want `"content": null` when an assistant message is
/// only a tool call, not an empty string.
fn serialize_content<S: serde::Serializer>(
    content: &str,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    if content.is_empty() {
        serializer.serialize_none()
    } else {
        serializer.serialize_str(content)
    }
}

fn deserialize_content<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<String, D::Error> {
    Ok(Option::<String>::deserialize(deserializer)?.unwrap_or_default())
}

/// One function call, in the wire shape the API uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// The arguments as JSON text, exactly as the model produced them.
    pub arguments: String,
}

impl Serialize for ToolCall {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serde_json::json!({
            "id": self.id,
            "type": "function",
            "function": { "name": self.name, "arguments": self.arguments },
        })
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ToolCall {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct WireCall {
            id: String,
            function: WireFunction,
        }

        #[derive(Deserialize)]
        struct WireFunction {
            name: String,
            #[serde(default)]
            arguments: String,
        }

        let call = WireCall::deserialize(deserializer)?;
        Ok(Self {
            id: call.id,
            name: call.function.name,
            arguments: call.function.arguments,
        })
    }
}

/// A tool the model may call, described by a JSON Schema.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

impl ToolDefinition {
    pub fn function(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: serde_json::Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
        }
    }
}

impl Serialize for ToolDefinition {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": self.name,
                "description": self.description,
                "parameters": self.parameters,
            },
        })
        .serialize(serializer)
    }
}

/// How hard a model should think before it answers. Providers spell the levels
/// their own way; this is the app's three-point scale.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Low,
    #[default]
    Medium,
    High,
}

/// What the window asks a model to do about thinking. `None` on a [`Request`]
/// means "say nothing", which leaves the model's own default in place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Thinking {
    pub enabled: bool,
    pub effort: Effort,
}

/// A streaming chat completion request.
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct Request {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDefinition>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
    /// The thinking switch, in the app's terms. Every wire spells it differently,
    /// so the adapter maps it and this field never reaches the JSON as it is.
    #[serde(skip)]
    pub thinking: Option<Thinking>,
}

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct StreamOptions {
    pub include_usage: bool,
}

impl Request {
    pub fn new(model: impl Into<String>, messages: Vec<Message>) -> Self {
        Self {
            model: model.into(),
            messages,
            tools: Vec::new(),
            stream: true,
            max_tokens: None,
            temperature: None,
            stream_options: Some(StreamOptions {
                include_usage: true,
            }),
            thinking: None,
        }
    }
}

/// Normalized streaming output. Provider-specific wire details stay in the
/// adapter.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    Text(String),
    /// A reasoning/thinking delta, for models that stream one.
    Reasoning(String),
    /// One complete call, emitted once its arguments have all arrived.
    ToolCall(ToolCall),
    Usage(Usage),
    Finished {
        reason: StopReason,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    /// Tokens served from DeepSeek's prompt cache.
    pub cache_hit_tokens: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    Stop,
    Length,
    ToolCalls,
    ContentFilter,
    Other(String),
}

impl StopReason {
    pub fn from_wire(reason: &str) -> Self {
        match reason {
            "stop" => Self::Stop,
            "length" => Self::Length,
            "tool_calls" => Self::ToolCalls,
            "content_filter" => Self::ContentFilter,
            other => Self::Other(other.to_owned()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_streaming_and_openai_shaped() {
        let request = Request::new(
            "deepseek-flash",
            vec![Message::system("be brief"), Message::user("hi")],
        );

        let json = serde_json::to_value(&request).expect("request serializes");

        assert_eq!(json["model"], "deepseek-flash");
        assert_eq!(json["stream"], true);
        assert_eq!(json["stream_options"]["include_usage"], true);
        assert_eq!(json["messages"][0]["role"], "system");
        assert_eq!(json["messages"][1]["role"], "user");
        assert!(json.get("max_tokens").is_none());
        assert!(json.get("temperature").is_none());
    }

    #[test]
    fn tools_and_tool_messages_are_openai_shaped() {
        let mut request = Request::new(
            "deepseek-flash",
            vec![
                Message::user("read the entry point"),
                Message::tool_calls(vec![ToolCall {
                    id: "call_1".to_owned(),
                    name: "read_file".to_owned(),
                    arguments: "{\"path\":\"src/main.rs\"}".to_owned(),
                }]),
                Message::tool_result("call_1", "fn main() {}"),
            ],
        );
        request.tools = vec![ToolDefinition::function(
            "read_file",
            "Read a file.",
            serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        )];

        let json = serde_json::to_value(&request).expect("request serializes");

        assert_eq!(json["tools"][0]["type"], "function");
        assert_eq!(json["tools"][0]["function"]["name"], "read_file");
        assert_eq!(
            json["tools"][0]["function"]["parameters"]["properties"]["path"]["type"],
            "string"
        );

        let assistant = &json["messages"][1];
        assert_eq!(assistant["role"], "assistant");
        assert_eq!(assistant["content"], serde_json::Value::Null);
        assert_eq!(assistant["tool_calls"][0]["id"], "call_1");
        assert_eq!(assistant["tool_calls"][0]["type"], "function");
        assert_eq!(assistant["tool_calls"][0]["function"]["name"], "read_file");

        let tool = &json["messages"][2];
        assert_eq!(tool["role"], "tool");
        assert_eq!(tool["tool_call_id"], "call_1");
        assert_eq!(tool["content"], "fn main() {}");
    }

    #[test]
    fn plain_messages_leave_tool_fields_out() {
        let request = Request::new("deepseek-flash", vec![Message::user("hi")]);

        let json = serde_json::to_value(&request).expect("request serializes");

        assert!(json.get("tools").is_none());
        assert_eq!(json["messages"][0]["content"], "hi");
        assert!(json["messages"][0].get("tool_calls").is_none());
        assert!(json["messages"][0].get("tool_call_id").is_none());
    }

    #[test]
    fn tool_calls_round_trip_from_the_wire() {
        let call: ToolCall = serde_json::from_value(serde_json::json!({
            "id": "call_1",
            "type": "function",
            "function": {"name": "read_file", "arguments": "{\"path\":\"a\"}"},
        }))
        .expect("the wire shape deserializes");

        assert_eq!(call.name, "read_file");
        assert_eq!(call.arguments, "{\"path\":\"a\"}");
    }

    #[test]
    fn stop_reasons_map_from_the_wire() {
        assert_eq!(StopReason::from_wire("stop"), StopReason::Stop);
        assert_eq!(StopReason::from_wire("length"), StopReason::Length);
        assert_eq!(StopReason::from_wire("tool_calls"), StopReason::ToolCalls);
        assert_eq!(
            StopReason::from_wire("nonsense"),
            StopReason::Other("nonsense".to_owned())
        );
    }
}
