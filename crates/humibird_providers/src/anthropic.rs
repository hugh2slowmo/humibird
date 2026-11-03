//! Anthropic's Messages API.
//!
//! It is a different wire from Chat Completions: `system` is a field of its own,
//! a message's content is a list of typed blocks rather than a string, and tools
//! arrive as `tool_use` blocks the model asks for and `tool_result` blocks that
//! answer them. The turn loop never sees any of that — this file maps the app's
//! [`Request`] onto that shape and its SSE stream back onto [`StreamEvent`].

use std::collections::HashMap;
use std::sync::Arc;

use futures::{AsyncBufReadExt, AsyncReadExt, StreamExt as _, io::BufReader};
use http_client::{AsyncBody, HttpClient, Method, Request as HttpRequest};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    ContextExceeded, EventStream, Message, Preset, Provider, Request, Role, StopReason,
    StreamEvent, ToolCall, Usage,
};

const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Anthropic requires `max_tokens`; a request that names none gets this.
const DEFAULT_MAX_TOKENS: u32 = 8192;

pub struct Anthropic {
    /// The provider this adapter speaks for: its display name for errors, and the
    /// catalogue that says what a model id means here.
    preset: &'static Preset,
    client: Arc<dyn HttpClient>,
    api_key: String,
    base_url: String,
    model: String,
}

impl Anthropic {
    pub fn new(
        preset: &'static Preset,
        client: Arc<dyn HttpClient>,
        api_key: impl Into<String>,
    ) -> Self {
        Self {
            preset,
            client,
            api_key: api_key.into(),
            base_url: String::new(),
            model: String::new(),
        }
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    fn endpoint(&self) -> String {
        format!("{}/messages", self.base_url.trim_end_matches('/'))
    }
}

impl Provider for Anthropic {
    fn request(&self, messages: Vec<Message>) -> Request {
        Request::new(self.model.clone(), messages)
    }

    fn list_models(
        &self,
    ) -> Option<futures::future::BoxFuture<'static, anyhow::Result<Vec<crate::DiscoveredModel>>>>
    {
        let client = self.client.clone();
        let base_url = self.base_url.clone();
        let api_key = self.api_key.clone();
        Some(Box::pin(async move {
            crate::fetch_models(
                client,
                crate::Wire::Anthropic,
                &base_url,
                Some(api_key.as_str()),
            )
            .await
        }))
    }

    fn context_limit(&self, model: &str) -> Option<u32> {
        self.preset.model(model).map(|info| info.context_tokens)
    }

    fn stream(&self, mut request: Request) -> EventStream {
        if request.model.is_empty() {
            request.model = self.model.clone();
        }

        let body = match serde_json::to_string(&WireRequest::from(&request)) {
            Ok(body) => body,
            Err(error) => {
                let error = anyhow::Error::from(error);
                return Box::pin(futures::stream::once(async move {
                    Err::<StreamEvent, _>(error)
                }));
            }
        };

        let client = self.client.clone();
        let api_key = self.api_key.clone();
        let endpoint = self.endpoint();
        let name = self.preset.display_name;

        Box::pin(async_stream::try_stream! {
            let request = HttpRequest::builder()
                .method(Method::POST)
                .uri(&endpoint)
                .header("Content-Type", "application/json")
                .header("anthropic-version", ANTHROPIC_VERSION)
                .header("x-api-key", api_key.trim())
                .body(AsyncBody::from(body))?;

            let mut response = client.send(request).await?;
            if !response.status().is_success() {
                let status = response.status();
                let mut text = String::new();
                response.body_mut().read_to_string(&mut text).await?;
                let message = format!("{name} request failed with {status}: {text}");
                Err(match ContextExceeded::from_rejection(&text) {
                    Some(exceeded) => anyhow::Error::from(exceeded).context(message),
                    None => anyhow::Error::msg(message),
                })?;
            }

            let mut lines = BufReader::new(response.into_body()).lines();
            let mut decoder = Decoder::default();
            while let Some(line) = lines.next().await {
                for event in decoder.push(&line?)? {
                    yield event;
                }
            }
        })
    }
}

/// The request as Anthropic wants it.
#[derive(Serialize)]
struct WireRequest {
    model: String,
    max_tokens: u32,
    messages: Vec<WireMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    stream: bool,
}

#[derive(Serialize)]
struct WireMessage {
    role: &'static str,
    content: Vec<Value>,
}

#[derive(Serialize)]
struct WireTool {
    name: String,
    description: String,
    input_schema: Value,
}

impl From<&Request> for WireRequest {
    fn from(request: &Request) -> Self {
        let mut system: Vec<String> = Vec::new();
        let mut messages: Vec<WireMessage> = Vec::new();

        for message in &request.messages {
            match message.role {
                Role::System => {
                    if !message.content.is_empty() {
                        system.push(message.content.clone());
                    }
                }
                Role::User => {
                    let mut blocks = Vec::new();
                    let text = crate::user_content(message);
                    if !text.is_empty() {
                        blocks.push(json!({"type": "text", "text": text}));
                    }
                    for attachment in &message.attachments {
                        if let crate::Attachment::Image {
                            media_type, data, ..
                        } = attachment
                        {
                            blocks.push(json!({
                                "type": "image",
                                "source": {
                                    "type": "base64",
                                    "media_type": media_type,
                                    "data": data,
                                },
                            }));
                        }
                    }
                    // A user turn always has to exist, even when it carries no
                    // words: dropping it would leave its tool results answering
                    // nothing.
                    if blocks.is_empty() {
                        blocks.push(json!({"type": "text", "text": ""}));
                    }
                    push_blocks(&mut messages, "user", blocks);
                }
                Role::Assistant => {
                    let mut blocks = Vec::new();
                    if !message.content.is_empty() {
                        blocks.push(json!({"type": "text", "text": message.content}));
                    }
                    for call in &message.tool_calls {
                        // Anthropic wants the arguments as JSON, not as the text the
                        // model produced; a call whose arguments did not parse is
                        // still a call with no arguments.
                        let input = serde_json::from_str::<Value>(&call.arguments)
                            .unwrap_or_else(|_| json!({}));
                        blocks.push(json!({
                            "type": "tool_use",
                            "id": call.id,
                            "name": call.name,
                            "input": input,
                        }));
                    }
                    if !blocks.is_empty() {
                        push_blocks(&mut messages, "assistant", blocks);
                    }
                }
                Role::Tool => {
                    // A tool's answer is a user turn as far as Anthropic is
                    // concerned, which is also why consecutive results merge.
                    let id = message.tool_call_id.clone().unwrap_or_default();
                    push_blocks(
                        &mut messages,
                        "user",
                        vec![json!({
                            "type": "tool_result",
                            "tool_use_id": id,
                            "content": message.content,
                        })],
                    );
                }
            }
        }

        Self {
            model: request.model.clone(),
            max_tokens: request.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
            messages,
            system: (!system.is_empty()).then(|| system.join("\n\n")),
            tools: request
                .tools
                .iter()
                .map(|tool| WireTool {
                    name: tool.name.clone(),
                    description: tool.description.clone(),
                    input_schema: tool.parameters.clone(),
                })
                .collect(),
            temperature: request.temperature,
            stream: true,
        }
    }
}

/// Appends a turn, merging it into the previous one when the role matches:
/// Anthropic requires alternating roles, and a tool result is a user turn.
fn push_blocks(messages: &mut Vec<WireMessage>, role: &'static str, blocks: Vec<Value>) {
    match messages.last_mut() {
        Some(last) if last.role == role => last.content.extend(blocks),
        _ => messages.push(WireMessage {
            role,
            content: blocks,
        }),
    }
}

/// Turns the SSE lines back into the app's events.
#[derive(Default)]
struct Decoder {
    /// Tool calls being assembled, by the content block they belong to.
    tools: HashMap<usize, PendingTool>,
    /// Anthropic reports the input count once, at `message_start`, and the output
    /// count at `message_delta`; the app reports both together.
    input_tokens: u32,
    cached_tokens: u32,
}

#[derive(Default)]
struct PendingTool {
    id: String,
    name: String,
    arguments: String,
}

impl Decoder {
    /// Consumes one SSE line. `event:` lines name the payload that follows, but
    /// the payload carries its own `type`, so they need no handling.
    fn push(&mut self, line: &str) -> anyhow::Result<Vec<StreamEvent>> {
        let Some(data) = line.strip_prefix("data:") else {
            return Ok(Vec::new());
        };
        let data = data.trim();
        if data.is_empty() {
            return Ok(Vec::new());
        }

        let mut events = Vec::new();
        match serde_json::from_str::<Event>(data)? {
            Event::MessageStart { message } => {
                let usage = message.usage;
                // The cached and cache-written tokens are part of what the model
                // saw, so they count toward the context the thread measures.
                self.input_tokens = usage.input_tokens.unwrap_or(0)
                    + usage.cache_read_input_tokens.unwrap_or(0)
                    + usage.cache_creation_input_tokens.unwrap_or(0);
                self.cached_tokens = usage.cache_read_input_tokens.unwrap_or(0);
            }
            Event::ContentBlockStart {
                index,
                content_block,
            } => {
                if let Block::ToolUse { id, name } = content_block {
                    self.tools.insert(
                        index,
                        PendingTool {
                            id,
                            name,
                            arguments: String::new(),
                        },
                    );
                }
            }
            Event::ContentBlockDelta { index, delta } => match delta {
                ContentDelta::TextDelta { text } => {
                    if !text.is_empty() {
                        events.push(StreamEvent::Text(text));
                    }
                }
                ContentDelta::ThinkingDelta { thinking } => {
                    if !thinking.is_empty() {
                        events.push(StreamEvent::Reasoning(thinking));
                    }
                }
                ContentDelta::InputJsonDelta { partial_json } => {
                    if let Some(tool) = self.tools.get_mut(&index) {
                        tool.arguments.push_str(&partial_json);
                    }
                }
                ContentDelta::Ignored => {}
            },
            Event::ContentBlockStop { index } => {
                if let Some(tool) = self.tools.remove(&index) {
                    events.push(StreamEvent::ToolCall(ToolCall {
                        id: tool.id,
                        name: tool.name,
                        arguments: if tool.arguments.trim().is_empty() {
                            "{}".to_owned()
                        } else {
                            tool.arguments
                        },
                    }));
                }
            }
            Event::MessageDelta { delta, usage } => {
                let completion = usage.output_tokens.unwrap_or(0);
                events.push(StreamEvent::Usage(Usage {
                    prompt_tokens: self.input_tokens,
                    completion_tokens: completion,
                    total_tokens: self.input_tokens.saturating_add(completion),
                    cache_hit_tokens: self.cached_tokens,
                }));
                if let Some(reason) = delta.stop_reason {
                    events.push(StreamEvent::Finished {
                        reason: stop_reason(&reason),
                    });
                }
            }
            Event::MessageStop | Event::Ping => {}
            Event::Error { error } => {
                anyhow::bail!("{}: {}", error.kind, error.message);
            }
        }
        Ok(events)
    }
}

fn stop_reason(reason: &str) -> StopReason {
    match reason {
        "end_turn" | "stop_sequence" => StopReason::Stop,
        "max_tokens" => StopReason::Length,
        "tool_use" => StopReason::ToolCalls,
        "refusal" => StopReason::ContentFilter,
        other => StopReason::Other(other.to_owned()),
    }
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum Event {
    #[serde(rename = "message_start")]
    MessageStart { message: StartMessage },
    #[serde(rename = "content_block_start")]
    ContentBlockStart { index: usize, content_block: Block },
    #[serde(rename = "content_block_delta")]
    ContentBlockDelta { index: usize, delta: ContentDelta },
    #[serde(rename = "content_block_stop")]
    ContentBlockStop { index: usize },
    #[serde(rename = "message_delta")]
    MessageDelta {
        delta: MessageDeltaBody,
        usage: WireUsage,
    },
    #[serde(rename = "message_stop")]
    MessageStop,
    #[serde(rename = "ping")]
    Ping,
    #[serde(rename = "error")]
    Error { error: ApiError },
}

#[derive(Deserialize)]
struct StartMessage {
    #[serde(default)]
    usage: WireUsage,
}

#[derive(Deserialize, Default)]
struct WireUsage {
    #[serde(default)]
    input_tokens: Option<u32>,
    #[serde(default)]
    output_tokens: Option<u32>,
    #[serde(default)]
    cache_read_input_tokens: Option<u32>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u32>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Block {
    ToolUse {
        id: String,
        name: String,
    },
    /// Text and thinking blocks announce themselves here, but their content
    /// arrives in deltas, and redacted thinking is nothing the app shows.
    #[serde(other)]
    Ignored,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentDelta {
    TextDelta {
        text: String,
    },
    ThinkingDelta {
        thinking: String,
    },
    InputJsonDelta {
        partial_json: String,
    },
    /// A thinking signature, which only matters when thinking is sent back; the
    /// app never replays it.
    #[serde(other)]
    Ignored,
}

#[derive(Deserialize)]
struct MessageDeltaBody {
    #[serde(default)]
    stop_reason: Option<String>,
}

#[derive(Deserialize)]
struct ApiError {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    message: String,
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use futures::future::BoxFuture;
    use http_client::{Url, http};

    use super::*;
    use crate::ToolDefinition;

    struct FakeClient {
        status: u16,
        body: String,
        sent: Mutex<Option<http::Request<AsyncBody>>>,
    }

    impl HttpClient for FakeClient {
        fn user_agent(&self) -> Option<&http::HeaderValue> {
            None
        }

        fn proxy(&self) -> Option<&Url> {
            None
        }

        fn send(
            &self,
            request: http::Request<AsyncBody>,
        ) -> BoxFuture<'static, anyhow::Result<http_client::Response<AsyncBody>>> {
            let body = self.body.clone();
            let status = self.status;
            *self.sent.lock().expect("sent lock") = Some(request);
            Box::pin(async move {
                Ok(http::Response::builder()
                    .status(status)
                    .body(AsyncBody::from(body))?)
            })
        }
    }

    fn provider(status: u16, body: &str) -> Anthropic {
        Anthropic::new(
            crate::preset("anthropic").expect("the anthropic preset"),
            Arc::new(FakeClient {
                status,
                body: body.to_owned(),
                sent: Mutex::new(None),
            }),
            "sk-ant-test",
        )
        .with_base_url("https://api.anthropic.com/v1")
        .with_model("claude-sonnet-4-5")
    }

    fn events(provider: &Anthropic, messages: Vec<Message>) -> Vec<anyhow::Result<StreamEvent>> {
        let request = provider.request(messages);
        futures::executor::block_on(provider.stream(request).collect())
    }

    /// One SSE frame, as Anthropic sends it: the `event:` line names the payload
    /// and the payload repeats the name in its `type`.
    fn frame(event: &str, data: &str) -> String {
        format!("event: {event}\ndata: {data}\n\n")
    }

    #[test]
    fn streams_text_thinking_and_the_end_of_the_turn() {
        let body = [
            frame(
                "message_start",
                r#"{"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":10,"cache_read_input_tokens":4}}}"#,
            ),
            frame(
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            ),
            frame(
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}}"#,
            ),
            frame(
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo"}}"#,
            ),
            frame(
                "content_block_start",
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"thinking","thinking":""}}"#,
            ),
            frame(
                "content_block_delta",
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"thinking_delta","thinking":"hmm"}}"#,
            ),
            frame("ping", r#"{"type":"ping"}"#),
            frame(
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}"#,
            ),
            frame("message_stop", r#"{"type":"message_stop"}"#),
        ]
        .concat();
        let provider = provider(200, &body);

        let events: Vec<StreamEvent> = events(&provider, vec![Message::user("hi")])
            .into_iter()
            .collect::<Result<_, _>>()
            .expect("the stream decodes");

        assert_eq!(
            events,
            vec![
                StreamEvent::Text("Hel".to_owned()),
                StreamEvent::Text("lo".to_owned()),
                StreamEvent::Reasoning("hmm".to_owned()),
                StreamEvent::Usage(Usage {
                    prompt_tokens: 14,
                    completion_tokens: 3,
                    total_tokens: 17,
                    cache_hit_tokens: 4,
                }),
                StreamEvent::Finished {
                    reason: StopReason::Stop
                },
            ]
        );
    }

    #[test]
    fn a_tool_call_is_assembled_from_fragments_and_ends_the_turn() {
        let body = [
            frame(
                "message_start",
                r#"{"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":8}}}"#,
            ),
            frame(
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call_1","name":"read_file"}}"#,
            ),
            frame(
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#,
            ),
            frame(
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"\"a.txt\"}"}}"#,
            ),
            frame(
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            frame(
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":5}}"#,
            ),
            frame("message_stop", r#"{"type":"message_stop"}"#),
        ]
        .concat();
        let provider = provider(200, &body);

        let events: Vec<StreamEvent> = events(&provider, vec![Message::user("hi")])
            .into_iter()
            .collect::<Result<_, _>>()
            .expect("the stream decodes");

        assert_eq!(
            events[0],
            StreamEvent::ToolCall(ToolCall {
                id: "call_1".to_owned(),
                name: "read_file".to_owned(),
                arguments: "{\"path\":\"a.txt\"}".to_owned(),
            })
        );
        assert_eq!(
            events.last(),
            Some(&StreamEvent::Finished {
                reason: StopReason::ToolCalls
            })
        );
    }

    #[test]
    fn a_streamed_error_stops_the_turn() {
        let body = frame(
            "error",
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
        );
        let provider = provider(200, &body);

        let result = futures::executor::block_on(provider.stream(provider.request(vec![])).next());
        let error = result
            .expect("one item")
            .expect_err("the stream carries the failure");

        assert!(error.to_string().contains("Overloaded"), "{error}");
    }

    #[test]
    fn a_text_and_an_image_arrive_as_blocks() {
        let mut request = Request::new("claude-sonnet-4-5", vec![Message::user("look")]);
        request.messages[0].attachments = vec![
            crate::Attachment::Text {
                name: "notes.md".to_owned(),
                text: "the build is flaky".to_owned(),
            },
            crate::Attachment::Image {
                name: "shot.png".to_owned(),
                media_type: "image/png".to_owned(),
                data: "iVBORw0KGgo=".to_owned(),
            },
        ];

        let sent = serde_json::to_value(WireRequest::from(&request)).expect("the body is JSON");

        let blocks = sent["messages"][0]["content"]
            .as_array()
            .expect("content is a list of blocks");
        assert_eq!(blocks[0]["type"], "text");
        assert!(
            blocks[0]["text"]
                .as_str()
                .is_some_and(|text| text.contains("<file name=\"notes.md\">")),
            "the file's name and body are in the text block: {}",
            blocks[0]
        );
        assert_eq!(blocks[1]["type"], "image");
        assert_eq!(blocks[1]["source"]["type"], "base64");
        assert_eq!(blocks[1]["source"]["media_type"], "image/png");
        assert_eq!(blocks[1]["source"]["data"], "iVBORw0KGgo=");
    }

    #[test]
    fn the_request_becomes_anthropic_shaped_json() {
        let provider = provider(200, "");
        let mut request = provider.request(vec![
            Message::system("be brief"),
            Message::user("read the entry point"),
            Message::tool_calls(vec![ToolCall {
                id: "call_1".to_owned(),
                name: "read_file".to_owned(),
                arguments: "{\"path\":\"src/main.rs\"}".to_owned(),
            }]),
            Message::tool_result("call_1", "fn main() {}"),
        ]);
        request.tools = vec![ToolDefinition::function(
            "read_file",
            "Read a file.",
            json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        )];

        let sent = serde_json::to_value(WireRequest::from(&request)).expect("the body is JSON");

        assert_eq!(sent["system"], "be brief");
        assert_eq!(sent["max_tokens"], DEFAULT_MAX_TOKENS);
        assert_eq!(sent["stream"], true);
        assert_eq!(sent["tools"][0]["name"], "read_file");
        assert_eq!(
            sent["tools"][0]["input_schema"]["properties"]["path"]["type"],
            "string"
        );
        assert_eq!(sent["messages"][0]["role"], "user");
        assert_eq!(sent["messages"][0]["content"][0]["type"], "text");
        assert_eq!(sent["messages"][1]["role"], "assistant");
        assert_eq!(sent["messages"][1]["content"][0]["type"], "tool_use");
        assert_eq!(
            sent["messages"][1]["content"][0]["input"]["path"],
            "src/main.rs"
        );
        // A tool's answer is a user turn, and it follows the assistant turn.
        assert_eq!(sent["messages"][2]["role"], "user");
        assert_eq!(sent["messages"][2]["content"][0]["type"], "tool_result");
        assert_eq!(sent["messages"][2]["content"][0]["tool_use_id"], "call_1");
    }

    #[test]
    fn the_key_and_the_api_version_go_out_as_headers() {
        let client = Arc::new(FakeClient {
            status: 200,
            body: frame("message_stop", r#"{"type":"message_stop"}"#),
            sent: Mutex::new(None),
        });
        let provider = Anthropic::new(
            crate::preset("anthropic").expect("the anthropic preset"),
            client.clone(),
            "sk-ant-test",
        )
        .with_base_url("https://api.anthropic.com/v1")
        .with_model("claude-sonnet-4-5");

        futures::executor::block_on(
            provider
                .stream(provider.request(vec![]))
                .collect::<Vec<_>>(),
        );

        let sent = client.sent.lock().expect("sent lock");
        let sent = sent.as_ref().expect("the request was sent");
        assert_eq!(
            sent.uri().to_string(),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            sent.headers()
                .get("x-api-key")
                .and_then(|value| value.to_str().ok()),
            Some("sk-ant-test")
        );
        assert_eq!(
            sent.headers()
                .get("anthropic-version")
                .and_then(|value| value.to_str().ok()),
            Some(ANTHROPIC_VERSION)
        );
    }

    #[test]
    fn consecutive_user_turns_merge_into_one() {
        let client = Arc::new(FakeClient {
            status: 200,
            body: String::new(),
            sent: Mutex::new(None),
        });
        let provider = Anthropic::new(
            crate::preset("anthropic").expect("the anthropic preset"),
            client,
            "sk-ant-test",
        )
        .with_base_url("https://api.anthropic.com/v1")
        .with_model("claude-sonnet-4-5");
        // Two tool results in a row are one user turn on this wire.
        let request = provider.request(vec![
            Message::tool_result("call_1", "one"),
            Message::tool_result("call_2", "two"),
        ]);

        let wire = WireRequest::from(&request);

        assert_eq!(wire.messages.len(), 1);
        assert_eq!(wire.messages[0].content.len(), 2);
    }

    #[test]
    fn the_model_list_goes_out_with_anthropic_headers() {
        let client = Arc::new(FakeClient {
            status: 200,
            body: r#"{"data":[
                {"id":"claude-b","display_name":"Claude B","max_input_tokens":200000,"max_tokens":8192},
                {"id":"claude-a","display_name":"Claude A","max_input_tokens":1000000,"max_tokens":64000}
            ]}"#
                .to_owned(),
            sent: Mutex::new(None),
        });
        let provider = Anthropic::new(
            crate::preset("anthropic").expect("the anthropic preset"),
            client.clone(),
            "sk-ant-test",
        )
        .with_base_url("https://api.anthropic.com/v1")
        .with_model("claude-sonnet-4-5");

        let models = futures::executor::block_on(
            provider
                .list_models()
                .expect("Anthropic can list its models"),
        )
        .expect("the list decodes");

        assert_eq!(
            models,
            vec![
                crate::DiscoveredModel {
                    id: "claude-a".to_owned(),
                    display_name: Some("Claude A".to_owned()),
                    context_tokens: Some(1_000_000),
                },
                crate::DiscoveredModel {
                    id: "claude-b".to_owned(),
                    display_name: Some("Claude B".to_owned()),
                    // `max_input_tokens`, not the output `max_tokens`.
                    context_tokens: Some(200_000),
                },
            ]
        );
        let sent = client.sent.lock().expect("sent lock");
        let sent = sent.as_ref().expect("a request was made");
        assert_eq!(
            sent.uri().to_string(),
            "https://api.anthropic.com/v1/models"
        );
        assert_eq!(
            sent.headers()
                .get("x-api-key")
                .and_then(|value| value.to_str().ok()),
            Some("sk-ant-test")
        );
        assert_eq!(
            sent.headers()
                .get("anthropic-version")
                .and_then(|value| value.to_str().ok()),
            Some(ANTHROPIC_VERSION)
        );
    }

    #[test]
    fn a_refusal_about_the_context_window_carries_the_cause() {
        let provider = provider(
            400,
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 210000 tokens > 200000 maximum"}}"#,
        );

        let result = futures::executor::block_on(provider.stream(provider.request(vec![])).next());
        let error = result
            .expect("one item")
            .expect_err("the request should fail");

        assert!(
            crate::context_exceeded(&error).is_some(),
            "the turn has to be able to tell this one apart: {error:#}"
        );
        assert!(error.to_string().contains("Anthropic"), "{error}");
    }
}
