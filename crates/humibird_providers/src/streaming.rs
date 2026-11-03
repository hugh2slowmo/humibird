//! Decodes OpenAI-compatible SSE lines into [`StreamEvent`]s.
//!
//! Kept separate from the transport so the wire format can be tested without a
//! network connection.

use serde::Deserialize;

use crate::{StopReason, StreamEvent, ToolCall, Usage};

#[derive(Default)]
pub struct StreamDecoder {
    finished: bool,
    /// Calls being assembled, by their wire index. Arguments arrive as string
    /// fragments across chunks.
    tool_calls: Vec<PendingToolCall>,
}

#[derive(Default)]
struct PendingToolCall {
    id: String,
    name: String,
    arguments: String,
}

impl StreamDecoder {
    /// True once `[DONE]` has been seen.
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// Consumes one SSE line. Non-data lines (comments, blanks) produce no
    /// events.
    pub fn push(&mut self, line: &str) -> anyhow::Result<Vec<StreamEvent>> {
        let Some(data) = line.strip_prefix("data:") else {
            return Ok(Vec::new());
        };
        let data = data.trim();
        if data.is_empty() {
            return Ok(Vec::new());
        }
        if data == "[DONE]" {
            self.finished = true;
            return Ok(Vec::new());
        }

        let chunk: Chunk = serde_json::from_str(data)?;
        let mut events = Vec::new();

        for choice in chunk.choices {
            if let Some(reasoning) = choice
                .delta
                .reasoning_content
                .or(choice.delta.reasoning)
                .filter(|text| !text.is_empty())
            {
                events.push(StreamEvent::Reasoning(reasoning));
            }
            if let Some(text) = choice.delta.content.filter(|text| !text.is_empty()) {
                events.push(StreamEvent::Text(text));
            }
            for fragment in choice.delta.tool_calls {
                self.absorb(fragment);
            }
            if let Some(reason) = choice.finish_reason {
                // Arguments are complete by the time a finish reason arrives.
                events.extend(self.take_tool_calls());
                events.push(StreamEvent::Finished {
                    reason: StopReason::from_wire(&reason),
                });
            }
        }

        if let Some(usage) = chunk.usage {
            events.push(StreamEvent::Usage(Usage {
                prompt_tokens: usage.prompt_tokens.unwrap_or_default(),
                completion_tokens: usage.completion_tokens.unwrap_or_default(),
                total_tokens: usage.total_tokens.unwrap_or_default(),
                cache_hit_tokens: usage
                    .prompt_cache_hit_tokens
                    .or_else(|| {
                        usage
                            .prompt_tokens_details
                            .and_then(|details| details.cached_tokens)
                    })
                    .unwrap_or_default(),
            }));
        }

        Ok(events)
    }

    fn absorb(&mut self, fragment: WireToolCall) {
        let index = fragment.index.unwrap_or_default();
        while self.tool_calls.len() <= index {
            self.tool_calls.push(PendingToolCall::default());
        }

        let call = &mut self.tool_calls[index];
        if let Some(id) = fragment.id {
            call.id = id;
        }
        if let Some(name) = fragment.function.name {
            call.name.push_str(&name);
        }
        if let Some(arguments) = fragment.function.arguments {
            call.arguments.push_str(&arguments);
        }
    }

    fn take_tool_calls(&mut self) -> Vec<StreamEvent> {
        std::mem::take(&mut self.tool_calls)
            .into_iter()
            .filter(|call| !call.name.is_empty())
            .map(|call| {
                StreamEvent::ToolCall(ToolCall {
                    id: call.id,
                    name: call.name,
                    arguments: call.arguments,
                })
            })
            .collect()
    }
}

#[derive(Deserialize)]
struct Chunk {
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<WireUsage>,
}

#[derive(Deserialize)]
struct Choice {
    #[serde(default)]
    delta: Delta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
    /// DeepSeek and OpenRouter name it `reasoning_content`; several other
    /// compatible endpoints name it `reasoning`. Both mean the same thing to the
    /// app, so one of them is enough.
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default)]
    tool_calls: Vec<WireToolCall>,
}

#[derive(Deserialize, Default)]
struct WireToolCall {
    #[serde(default)]
    index: Option<usize>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: WireFunction,
}

#[derive(Deserialize, Default)]
struct WireFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Deserialize)]
struct WireUsage {
    #[serde(default)]
    prompt_tokens: Option<u32>,
    #[serde(default)]
    completion_tokens: Option<u32>,
    #[serde(default)]
    total_tokens: Option<u32>,
    /// DeepSeek reports the cache hits at the top level.
    #[serde(default)]
    prompt_cache_hit_tokens: Option<u32>,
    /// OpenAI and OpenRouter report them under `prompt_tokens_details`.
    #[serde(default)]
    prompt_tokens_details: Option<PromptTokensDetails>,
}

#[derive(Deserialize)]
struct PromptTokensDetails {
    #[serde(default)]
    cached_tokens: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(lines: &[&str]) -> Vec<StreamEvent> {
        let mut decoder = StreamDecoder::default();
        let mut events = Vec::new();
        for line in lines {
            events.extend(decoder.push(line).expect("line decodes"));
        }
        events
    }

    #[test]
    fn decodes_text_reasoning_and_finish() {
        let events = decode(&[
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"},\"finish_reason\":null}]}",
            "",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hel\"},\"finish_reason\":null}]}",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"lo\"},\"finish_reason\":null}]}",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"thinking\"},\"finish_reason\":null}]}",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}",
        ]);

        assert_eq!(
            events,
            vec![
                StreamEvent::Text("Hel".to_owned()),
                StreamEvent::Text("lo".to_owned()),
                StreamEvent::Reasoning("thinking".to_owned()),
                StreamEvent::Finished {
                    reason: StopReason::Stop
                },
            ]
        );
    }

    #[test]
    fn assembles_streamed_tool_calls() {
        let events = decode(&[
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"pa\"}}]},\"finish_reason\":null}]}",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"th\\\":\\\"src/main.rs\\\"}\"}}]},\"finish_reason\":null}]}",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}",
        ]);

        assert_eq!(
            events,
            vec![
                StreamEvent::ToolCall(ToolCall {
                    id: "call_1".to_owned(),
                    name: "read_file".to_owned(),
                    arguments: "{\"path\":\"src/main.rs\"}".to_owned(),
                }),
                StreamEvent::Finished {
                    reason: StopReason::ToolCalls
                },
            ]
        );
    }

    #[test]
    fn keeps_parallel_tool_calls_apart() {
        let events = decode(&[
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"a\",\"function\":{\"name\":\"list_directory\",\"arguments\":\"{}\"}}]},\"finish_reason\":null}]}",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"b\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"a.txt\\\"}\"}}]},\"finish_reason\":null}]}",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}",
        ]);

        let calls: Vec<&str> = events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::ToolCall(call) => Some(call.id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(calls, vec!["a", "b"]);
    }

    #[test]
    fn decodes_usage_from_the_final_chunk() {
        let events = decode(&[
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":3,\"total_tokens\":15,\"prompt_cache_hit_tokens\":8}}",
        ]);

        assert_eq!(
            events,
            vec![
                StreamEvent::Text("hi".to_owned()),
                StreamEvent::Usage(Usage {
                    prompt_tokens: 12,
                    completion_tokens: 3,
                    total_tokens: 15,
                    cache_hit_tokens: 8,
                }),
            ]
        );
    }

    #[test]
    fn takes_reasoning_under_either_name() {
        let events = decode(&[
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"one\"},\"finish_reason\":null}]}",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning\":\"two\"},\"finish_reason\":null}]}",
        ]);

        assert_eq!(
            events,
            vec![
                StreamEvent::Reasoning("one".to_owned()),
                StreamEvent::Reasoning("two".to_owned()),
            ]
        );
    }

    #[test]
    fn takes_the_cache_hit_count_from_either_shape() {
        let events = decode(&[
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":20,\"completion_tokens\":2,\"total_tokens\":22,\"prompt_tokens_details\":{\"cached_tokens\":16}}}",
        ]);

        assert_eq!(
            events,
            vec![StreamEvent::Usage(Usage {
                prompt_tokens: 20,
                completion_tokens: 2,
                total_tokens: 22,
                cache_hit_tokens: 16,
            })]
        );
    }

    #[test]
    fn ignores_comments_and_blank_lines() {
        let mut decoder = StreamDecoder::default();

        assert!(decoder.push(": keep-alive").expect("ok").is_empty());
        assert!(decoder.push("").expect("ok").is_empty());
        assert!(decoder.push("event: message").expect("ok").is_empty());
    }

    #[test]
    fn stops_at_done() {
        let mut decoder = StreamDecoder::default();

        assert!(!decoder.is_finished());
        assert!(decoder.push("data: [DONE]").expect("ok").is_empty());
        assert!(decoder.is_finished());
    }

    #[test]
    fn surfaces_malformed_payloads() {
        let mut decoder = StreamDecoder::default();

        assert!(decoder.push("data: {not json").is_err());
    }
}
