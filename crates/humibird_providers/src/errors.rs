//! What a provider's refusal means.
//!
//! A stream yields `anyhow::Error`s, so the one rejection a turn can act on is
//! carried as a typed cause rather than a sentence: the conversation no longer
//! fits the model's context window. Everything else stays a message.

use std::error::Error;
use std::fmt;

/// The provider refused a request because the conversation is larger than the
/// model's context window.
///
/// This is the only signal that a thread is too big, and it arrives without
/// anyone knowing the model's limit — which is the point: a model the catalogue
/// does not describe has no limit to measure against, so nothing else would ever
/// tell the thread to summarize itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextExceeded;

/// Error codes OpenAI-compatible providers use for "the request did not fit".
const CODES: &[&str] = &["context_length_exceeded", "request_too_large"];

/// What the same refusal looks like in prose, for providers that only send a
/// message (DeepSeek says "This model's maximum context length is N tokens").
const PHRASES: &[&str] = &[
    "maximum context length",
    "maximum context window",
    "context length exceeded",
    "exceeds the context window",
    "prompt is too long",
    "too many tokens",
    "request too large",
];

impl ContextExceeded {
    /// Reads a rejection body — OpenAI-shaped JSON, or plain text — and says
    /// whether it is about the context window.
    pub fn from_rejection(body: &str) -> Option<Self> {
        let parsed = serde_json::from_str::<serde_json::Value>(body).ok();
        let error = parsed.as_ref().and_then(|value| value.get("error"));

        let code = error
            .and_then(|error| error.get("code").or_else(|| error.get("type")))
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let message = error
            .and_then(|error| error.get("message"))
            .and_then(|value| value.as_str())
            .unwrap_or(body)
            .to_ascii_lowercase();

        if CODES.iter().any(|known| code == *known) {
            return Some(Self);
        }
        PHRASES
            .iter()
            .any(|phrase| message.contains(phrase))
            .then_some(Self)
    }
}

impl fmt::Display for ContextExceeded {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("the conversation is larger than the model's context window")
    }
}

impl Error for ContextExceeded {}

/// The `ContextExceeded` in an error's chain, if this failure was one.
///
/// Providers keep the readable message on the outside and this type as the cause,
/// so what the person reads is unchanged; the turn asks whether the cause is
/// there instead of matching on the text.
pub fn context_exceeded(error: &anyhow::Error) -> Option<&ContextExceeded> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<ContextExceeded>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_code_says_the_context_was_too_large() {
        let body = r#"{"error":{"message":"Your input exceeds the context window of this model.","code":"context_length_exceeded"}}"#;

        assert_eq!(ContextExceeded::from_rejection(body), Some(ContextExceeded));
    }

    #[test]
    fn a_message_says_it_when_the_code_does_not() {
        // DeepSeek's shape: a generic `invalid_request_error` with the numbers in
        // the prose.
        let body = r#"{"error":{"message":"This model's maximum context length is 163840 tokens. However, you requested 170000 tokens (160000 in the messages, 10000 in the completion).","type":"invalid_request_error"}}"#;

        assert_eq!(ContextExceeded::from_rejection(body), Some(ContextExceeded));
    }

    #[test]
    fn other_refusals_stay_plain_errors() {
        for body in [
            r#"{"error":{"message":"Authentication Fails, Your api key: ****abcd is invalid","type":"authentication_error","code":"invalid_api_key"}}"#,
            r#"{"error":{"message":"Insufficient Balance","type":"unknown_error"}}"#,
            "upstream connect error or disconnect/reset before headers",
            "",
        ] {
            assert_eq!(ContextExceeded::from_rejection(body), None, "{body}");
        }
    }

    #[test]
    fn the_message_the_person_reads_does_not_change() {
        let refusal = format!("DeepSeek request failed with 400 Bad Request: {BODY}");
        let plain = anyhow::Error::msg(refusal.clone());
        assert!(
            context_exceeded(&plain).is_none(),
            "not every failure is this"
        );

        let too_long = anyhow::Error::from(ContextExceeded).context(format!(
            "DeepSeek request failed with 400 Bad Request: {BODY}"
        ));
        assert_eq!(too_long.to_string(), refusal);
        assert!(context_exceeded(&too_long).is_some());
    }

    const BODY: &str =
        r#"{"error":{"message":"This model's maximum context length is 163840 tokens."}}"#;
}
