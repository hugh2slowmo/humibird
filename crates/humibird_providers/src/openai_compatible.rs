//! Any provider that speaks OpenAI's Chat Completions wire.
//!
//! DeepSeek, OpenAI, OpenRouter, Ollama and an arbitrary compatible endpoint all
//! use the same request shape and the same SSE stream; they differ in the
//! endpoint, the key, and what their deltas are called. One adapter covers them,
//! so another provider is a row in [`crate::PRESETS`], not another file.

use std::sync::Arc;

use futures::{AsyncBufReadExt, AsyncReadExt, StreamExt as _, io::BufReader};
use http_client::{AsyncBody, HttpClient, HttpRequestExt as _, Method, Request as HttpRequest};

use crate::{
    Attachment, ContextExceeded, Effort, EventStream, Message, Preset, Provider, Reasoning,
    Request, Role, StreamDecoder, StreamEvent, Thinking, ToolCall, ToolDefinition,
};

/// One message as an OpenAI-compatible endpoint wants it. The app keeps the
/// person's text and the attachments apart; the wire wants one `content` that is
/// either a string or a list of parts, plus the assistant's calls.
#[derive(serde::Serialize)]
struct WireMessage {
    role: Role,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<WireContent>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<ToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(serde::Serialize)]
#[serde(untagged)]
enum WireContent {
    Text(String),
    Parts(Vec<WirePart>),
}

#[derive(serde::Serialize)]
#[serde(tag = "type")]
enum WirePart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image_url")]
    ImageUrl { image_url: WireImageUrl },
}

#[derive(serde::Serialize)]
struct WireImageUrl {
    url: String,
}

fn wire_message(message: &Message) -> WireMessage {
    let text = crate::user_content(message);
    let images: Vec<&Attachment> = message
        .attachments
        .iter()
        .filter(|a| a.is_image())
        .collect();
    let content = if images.is_empty() {
        (!text.is_empty()).then_some(WireContent::Text(text))
    } else {
        let mut parts = Vec::new();
        if !text.is_empty() {
            parts.push(WirePart::Text { text });
        }
        for attachment in images {
            if let Attachment::Image {
                media_type, data, ..
            } = attachment
            {
                parts.push(WirePart::ImageUrl {
                    image_url: WireImageUrl {
                        url: format!("data:{media_type};base64,{data}"),
                    },
                });
            }
        }
        Some(WireContent::Parts(parts))
    };

    WireMessage {
        role: message.role,
        content,
        tool_calls: message.tool_calls.clone(),
        tool_call_id: message.tool_call_id.clone(),
    }
}

/// The body an OpenAI-compatible endpoint receives. [`Request`] carries the app's
/// thinking switch in normalized form; each preset's wire spells it its own way,
/// so the body is built rather than serialized straight from the request.
#[derive(serde::Serialize)]
struct WireRequest<'a> {
    model: &'a str,
    messages: Vec<WireMessage>,
    #[serde(skip_serializing_if = "no_tools")]
    tools: &'a [ToolDefinition],
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<&'a crate::StreamOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<WireThinking>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<&'static str>,
}

#[derive(serde::Serialize)]
struct WireThinking {
    #[serde(rename = "type")]
    kind: &'static str,
}

fn no_tools(tools: &&[ToolDefinition]) -> bool {
    tools.is_empty()
}

impl<'a> WireRequest<'a> {
    fn new(request: &'a Request, reasoning: Reasoning) -> Self {
        let (thinking, reasoning_effort) = thinking_fields(request.thinking, reasoning);
        Self {
            model: &request.model,
            messages: request.messages.iter().map(wire_message).collect(),
            tools: &request.tools,
            stream: request.stream,
            max_tokens: request.max_tokens,
            temperature: request.temperature,
            stream_options: request.stream_options.as_ref(),
            thinking,
            reasoning_effort,
        }
    }
}

/// What the body says about thinking. `None, None` means "say nothing" and leave
/// the model's own default, which is the only safe thing for a wire with no
/// switch (`Reasoning::None`) and for an endpoint that has no way to turn
/// thinking off.
fn thinking_fields(
    thinking: Option<Thinking>,
    reasoning: Reasoning,
) -> (Option<WireThinking>, Option<&'static str>) {
    let Some(thinking) = thinking else {
        return (None, None);
    };

    match reasoning {
        Reasoning::None => (None, None),
        // DeepSeek has an explicit off switch, so "off" is said rather than left
        // to the model. It counts effort as low/high/max, one level fewer than
        // the app's scale.
        Reasoning::Thinking => {
            if !thinking.enabled {
                return (Some(WireThinking { kind: "disabled" }), None);
            }
            let effort = match thinking.effort {
                Effort::Low => "low",
                Effort::Medium => "high",
                Effort::High => "max",
            };
            (Some(WireThinking { kind: "enabled" }), Some(effort))
        }
        // OpenAI and OpenRouter take the effort alone; there is no field to turn
        // thinking off, so "off" is silence.
        Reasoning::Effort => {
            if !thinking.enabled {
                return (None, None);
            }
            let effort = match thinking.effort {
                Effort::Low => "low",
                Effort::Medium => "medium",
                Effort::High => "high",
            };
            (None, Some(effort))
        }
    }
}

pub struct OpenAiCompatible {
    /// The provider this adapter speaks for: its display name for errors, and the
    /// catalogue that says what a model id means here.
    preset: &'static Preset,
    client: Arc<dyn HttpClient>,
    /// `None` for an endpoint that does not want one, such as a local Ollama.
    api_key: Option<String>,
    base_url: String,
    model: String,
}

impl OpenAiCompatible {
    pub fn new(
        preset: &'static Preset,
        client: Arc<dyn HttpClient>,
        api_key: Option<String>,
    ) -> Self {
        Self {
            preset,
            client,
            api_key,
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
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
    }
}

impl Provider for OpenAiCompatible {
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
                crate::Wire::OpenAiCompatible,
                &base_url,
                api_key.as_deref(),
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

        let body = match serde_json::to_string(&WireRequest::new(&request, self.preset.reasoning)) {
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
                .when_some(api_key.as_deref(), |builder, key| {
                    builder.header("Authorization", format!("Bearer {}", key.trim()))
                })
                .body(AsyncBody::from(body))?;

            let mut response = client.send(request).await?;
            if !response.status().is_success() {
                let status = response.status();
                let mut text = String::new();
                response.body_mut().read_to_string(&mut text).await?;
                let message = format!("{name} request failed with {status}: {text}");
                // A refusal that says the conversation is too long is the one the
                // thread can act on, so it rides as the cause; the text the person
                // reads stays exactly what it was.
                Err(match ContextExceeded::from_rejection(&text) {
                    Some(exceeded) => anyhow::Error::from(exceeded).context(message),
                    None => anyhow::Error::msg(message),
                })?;
            }

            let mut lines = BufReader::new(response.into_body()).lines();
            let mut decoder = StreamDecoder::default();

            while let Some(line) = lines.next().await {
                let line = line?;
                for event in decoder.push(&line)? {
                    yield event;
                }
                if decoder.is_finished() {
                    break;
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use futures::future::BoxFuture;
    use http_client::{Url, http};

    use super::*;
    use crate::{StopReason, Usage, presets};

    /// Answers with a scripted status and body, and keeps the request it was sent
    /// so a test can read the headers.
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

    fn client(status: u16, body: &str) -> Arc<FakeClient> {
        Arc::new(FakeClient {
            status,
            body: body.to_owned(),
            sent: Mutex::new(None),
        })
    }

    fn provider(client: Arc<FakeClient>, key: Option<&str>) -> OpenAiCompatible {
        OpenAiCompatible::new(
            presets::preset("deepseek").expect("the deepseek preset"),
            client,
            key.map(str::to_owned),
        )
        .with_base_url("https://api.deepseek.com/v1")
        .with_model(presets::DEFAULT_MODEL)
    }

    fn events(provider: &OpenAiCompatible) -> Vec<anyhow::Result<StreamEvent>> {
        let request = provider.request(vec![Message::user("hi")]);
        futures::executor::block_on(provider.stream(request).collect())
    }

    #[test]
    fn streams_normalized_events() {
        let body = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hel\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"lo\"},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":2,\"total_tokens\":6}}\n\n",
            "data: [DONE]\n\n",
        );
        let provider = provider(client(200, body), Some("test-key"));

        let events: Vec<StreamEvent> = events(&provider)
            .into_iter()
            .collect::<Result<_, _>>()
            .expect("stream decodes");

        assert_eq!(
            events,
            vec![
                StreamEvent::Text("Hel".to_owned()),
                StreamEvent::Text("lo".to_owned()),
                StreamEvent::Finished {
                    reason: StopReason::Stop
                },
                StreamEvent::Usage(Usage {
                    prompt_tokens: 4,
                    completion_tokens: 2,
                    total_tokens: 6,
                    cache_hit_tokens: 0,
                }),
            ]
        );
    }

    #[test]
    fn the_trait_builds_requests_with_its_model() {
        let provider = provider(client(200, ""), Some("test-key"));
        let as_trait: &dyn Provider = &provider;

        let request = as_trait.request(vec![Message::user("hi")]);

        assert_eq!(request.model, presets::DEFAULT_MODEL);
        assert_eq!(request.messages, vec![Message::user("hi")]);
    }

    #[test]
    fn explicit_model_and_base_url_are_used() {
        let provider = provider(client(200, ""), Some("test-key"))
            .with_model("deepseek-reasoner")
            .with_base_url("https://example.test/v1/");

        assert_eq!(
            provider.request(vec![Message::user("hi")]).model,
            "deepseek-reasoner"
        );
        assert_eq!(
            provider.endpoint(),
            "https://example.test/v1/chat/completions"
        );
    }

    #[test]
    fn a_provider_describes_the_models_it_catalogs() {
        let provider = provider(client(200, ""), Some("k"));

        assert_eq!(
            provider.context_limit(presets::DEFAULT_MODEL),
            Some(1_000_000),
            "the model the catalogue names comes with its window"
        );
        assert_eq!(
            provider.context_limit("a-model-nobody-catalogued"),
            None,
            "and a model it does not know has no window to measure against"
        );
    }

    #[test]
    fn the_key_goes_out_as_a_bearer_header() {
        let client = client(200, "");
        let provider = provider(client.clone(), Some("sk-test"));

        events(&provider);

        let sent = client.sent.lock().expect("sent lock");
        let request = sent.as_ref().expect("the request was sent");
        assert_eq!(
            request.uri().to_string(),
            "https://api.deepseek.com/v1/chat/completions"
        );
        assert_eq!(
            request
                .headers()
                .get("authorization")
                .and_then(|value| value.to_str().ok()),
            Some("Bearer sk-test")
        );
    }

    #[test]
    fn an_endpoint_without_a_key_sends_no_authorization() {
        let client = client(200, "");
        let provider = provider(client.clone(), None);

        events(&provider);

        let sent = client.sent.lock().expect("sent lock");
        let request = sent.as_ref().expect("the request was sent");
        assert!(
            request.headers().get("authorization").is_none(),
            "a keyless endpoint must not get a stray header"
        );
    }

    #[test]
    fn the_model_list_is_read_sorted_and_with_the_key() {
        let client = client(200, r#"{"data":[{"id":"b"},{"id":"a"},{"id":"b"}]}"#);

        let models = futures::executor::block_on(crate::fetch_models(
            client.clone(),
            crate::Wire::OpenAiCompatible,
            "https://api.deepseek.com/v1",
            Some("sk-test"),
        ))
        .expect("the list decodes");

        let ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(ids, ["a", "b"], "sorted and deduplicated");
        let sent = client.sent.lock().expect("sent lock");
        let sent = sent.as_ref().expect("a request was made");
        assert_eq!(sent.method(), http::Method::GET);
        assert_eq!(sent.uri().to_string(), "https://api.deepseek.com/v1/models");
        assert_eq!(
            sent.headers()
                .get("authorization")
                .and_then(|value| value.to_str().ok()),
            Some("Bearer sk-test")
        );
    }

    #[test]
    fn a_provider_that_will_not_list_its_models_is_an_error() {
        let client = client(404, "not found");

        let result = futures::executor::block_on(crate::fetch_models(
            client,
            crate::Wire::OpenAiCompatible,
            "https://api.deepseek.com/v1",
            None,
        ));

        assert!(result.is_err(), "404 is not an empty list");
    }

    #[test]
    fn surfaces_api_errors_with_the_provider_name() {
        let provider = provider(
            client(401, "{\"error\":{\"message\":\"bad key\"}}"),
            Some("k"),
        );
        let request = provider.request(vec![Message::user("hi")]);

        let result = futures::executor::block_on(provider.stream(request).next());
        let error = result
            .expect("one item")
            .expect_err("the request should fail");

        assert!(error.to_string().contains("DeepSeek"), "{error}");
        assert!(error.to_string().contains("401"), "{error}");
        assert!(error.to_string().contains("bad key"), "{error}");
        assert!(
            crate::context_exceeded(&error).is_none(),
            "a bad key is not the context window"
        );
    }

    /// The body DeepSeek receives for one request, as JSON.
    fn sent_body(request: &Request, preset: &str) -> serde_json::Value {
        let reasoning = presets::preset(preset).expect("the preset").reasoning;
        serde_json::to_value(WireRequest::new(request, reasoning)).expect("the body is JSON")
    }

    #[test]
    fn deepseek_is_told_to_think_and_how_hard_when_the_switch_is_on() {
        let mut request = Request::new(presets::DEFAULT_MODEL, vec![Message::user("hi")]);
        request.thinking = Some(Thinking {
            enabled: true,
            effort: Effort::Medium,
        });

        let sent = sent_body(&request, "deepseek");

        assert_eq!(sent["thinking"]["type"], "enabled");
        assert_eq!(
            sent["reasoning_effort"], "high",
            "DeepSeek's middle level is called high on its wire"
        );
    }

    #[test]
    fn deepseek_is_told_not_to_think_when_the_switch_is_off() {
        let mut request = Request::new(presets::DEFAULT_MODEL, vec![Message::user("hi")]);
        request.thinking = Some(Thinking {
            enabled: false,
            effort: Effort::High,
        });

        let sent = sent_body(&request, "deepseek");

        assert_eq!(sent["thinking"]["type"], "disabled");
        assert!(
            sent.get("reasoning_effort").is_none(),
            "an effort with no thinking to spend it on is noise: {sent}"
        );
    }

    #[test]
    fn openai_takes_an_effort_and_has_no_off_switch() {
        let mut request = Request::new("gpt-5", vec![Message::user("hi")]);
        request.thinking = Some(Thinking {
            enabled: true,
            effort: Effort::Low,
        });
        let on = sent_body(&request, "openai");
        assert_eq!(on["reasoning_effort"], "low");
        assert!(
            on.get("thinking").is_none(),
            "OpenAI does not know a thinking field"
        );

        request.thinking = Some(Thinking {
            enabled: false,
            effort: Effort::Low,
        });
        let off = sent_body(&request, "openai");
        assert!(
            off.get("reasoning_effort").is_none() && off.get("thinking").is_none(),
            "off is silence where the wire has no off switch: {off}"
        );
    }

    #[test]
    fn a_wire_with_no_switch_says_nothing_about_thinking() {
        let mut request = Request::new("some-model", vec![Message::user("hi")]);
        request.thinking = Some(Thinking {
            enabled: true,
            effort: Effort::High,
        });

        let sent = sent_body(&request, "ollama");

        assert!(sent.get("thinking").is_none(), "{sent}");
        assert!(sent.get("reasoning_effort").is_none(), "{sent}");
    }

    #[test]
    fn a_text_attachment_travels_inside_the_user_message() {
        let mut request = Request::new(presets::DEFAULT_MODEL, vec![Message::user("look")]);
        request.messages[0].attachments.push(Attachment::Text {
            name: "src/main.rs".to_owned(),
            text: "fn main() {}".to_owned(),
        });

        let sent = sent_body(&request, "deepseek");

        let content = sent["messages"][0]["content"]
            .as_str()
            .expect("a text attachment keeps the content a string");
        assert!(content.starts_with("look"), "{content}");
        assert!(
            content.contains("<file name=\"src/main.rs\">"),
            "the model is told which file it is: {content}"
        );
        assert!(content.contains("fn main() {}"), "{content}");
    }

    #[test]
    fn an_image_becomes_a_data_url_part_beside_the_text() {
        let mut request = Request::new(
            presets::DEFAULT_MODEL,
            vec![Message::user("what is wrong here?")],
        );
        request.messages[0].attachments.push(Attachment::Image {
            name: "shot.png".to_owned(),
            media_type: "image/png".to_owned(),
            data: "iVBORw0KGgo=".to_owned(),
        });

        let sent = sent_body(&request, "deepseek");

        let parts = sent["messages"][0]["content"]
            .as_array()
            .expect("an image makes the content a list");
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[1]["type"], "image_url");
        assert_eq!(
            parts[1]["image_url"]["url"],
            "data:image/png;base64,iVBORw0KGgo="
        );
    }

    #[test]
    fn the_model_list_keeps_the_window_the_wire_reported() {
        let provider = OpenAiCompatible::new(
            presets::preset("openrouter").expect("the openrouter preset"),
            client(
                200,
                r#"{"data":[
                    {"id":"openai/gpt-x","name":"OpenAI: GPT-X","context_length":400000},
                    {"id":"some/vllm","max_model_len":128000},
                    {"id":"plain"}
                ]}"#,
            ),
            Some("k".to_owned()),
        )
        .with_base_url("https://openrouter.ai/api/v1")
        .with_model("openai/gpt-x");

        let models = futures::executor::block_on(
            provider
                .list_models()
                .expect("OpenRouter can list its models"),
        )
        .expect("the list decodes");

        assert_eq!(
            models,
            vec![
                crate::DiscoveredModel {
                    id: "openai/gpt-x".to_owned(),
                    display_name: Some("OpenAI: GPT-X".to_owned()),
                    context_tokens: Some(400_000),
                },
                crate::DiscoveredModel {
                    id: "plain".to_owned(),
                    display_name: None,
                    context_tokens: None,
                },
                crate::DiscoveredModel {
                    id: "some/vllm".to_owned(),
                    display_name: None,
                    // vLLM spells the window `max_model_len`.
                    context_tokens: Some(128_000),
                },
            ]
        );
    }

    #[test]
    fn a_refusal_about_the_context_window_carries_the_cause() {
        let provider = provider(
            client(
                400,
                r#"{"error":{"message":"This model's maximum context length is 163840 tokens. However, you requested 170000 tokens.","type":"invalid_request_error"}}"#,
            ),
            Some("k"),
        );
        let request = provider.request(vec![Message::user("hi")]);

        let result = futures::executor::block_on(provider.stream(request).next());
        let error = result
            .expect("one item")
            .expect_err("the request should fail");

        assert!(
            crate::context_exceeded(&error).is_some(),
            "the turn has to be able to tell this one apart: {error:#}"
        );
        assert!(
            error.to_string().contains("maximum context length"),
            "the person still reads what the provider said: {error}"
        );
    }
}
