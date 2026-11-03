//! The `ask_user` tool: a question whose answer comes from the person.

use serde::Deserialize;
use serde_json::{Value, json};

use crate::{CallKind, CallLabel, Tool, ToolResult};

/// What the model reads when someone answers, whatever shape the answer took.
const ANSWERED: &str = "The user selected: ";
/// What the model reads when nobody answers. An error, so it can react instead of
/// assuming a silent yes.
pub const DECLINED_TEXT: &str = "The user declined to answer the question.";

/// A validated `ask_user` call. The window renders it; the loop never runs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Question {
    pub question: String,
    /// Labels offered as buttons, trimmed, in the order the model gave them.
    pub options: Vec<String>,
    pub allow_free_text: bool,
}

#[derive(Deserialize)]
struct AskUserInput {
    question: String,
    #[serde(default)]
    options: Vec<String>,
    #[serde(default)]
    allow_free_text: bool,
}

impl Question {
    /// Parses the raw arguments of the call, the way the loop receives them.
    pub fn parse(arguments: &str) -> Result<Self, String> {
        let input: AskUserInput = serde_json::from_str(arguments)
            .map_err(|error| format!("invalid arguments: {error}"))?;

        let question = input.question.trim().to_owned();
        if question.is_empty() {
            return Err("the question is empty".to_owned());
        }

        let options = input
            .options
            .iter()
            .map(|option| option.trim().to_owned())
            .filter(|option| !option.is_empty())
            .collect::<Vec<_>>();
        // A single button is a statement the user has to click; an answer without
        // a choice is not an answer.
        if options.len() < 2 && !input.allow_free_text {
            return Err(
                "ask_user needs at least two `options`, or `allow_free_text` set to true. \
                 Ask an open-ended question in prose instead."
                    .to_owned(),
            );
        }

        Ok(Self {
            question,
            options,
            allow_free_text: input.allow_free_text,
        })
    }
}

/// The tool result for an answer. The model is told the string came from the
/// user, so it does not mistake it for its own guess.
pub fn answer_text(selected: &str) -> String {
    format!("{ANSWERED}{selected}")
}

pub struct AskUser;

impl Tool for AskUser {
    fn name(&self) -> &str {
        "ask_user"
    }

    fn kind(&self) -> CallKind {
        CallKind::Ask
    }

    fn label(&self, input: &Value) -> Option<CallLabel> {
        let input: AskUserInput = serde_json::from_value(input.clone()).ok()?;
        Some(CallLabel::new("Ask", input.question))
    }

    fn description(&self) -> &str {
        "Ask the user a question mid-task: choose between approaches, confirm an \
         assumption, or get a value you cannot infer. Provide at least two \
         `options`, free text, or both; a typed answer wins over a picked one. \
         The turn waits for the reply, so ask only when the answer changes what \
         you do next."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "The question to ask. Keep it short and specific."
                },
                "options": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Labels offered as buttons, returned verbatim when picked. Two or more, unless free text is allowed."
                },
                "allow_free_text": {
                    "type": "boolean",
                    "description": "Whether the user may type their own answer instead of, or in addition to, picking an option."
                }
            },
            "required": ["question"]
        })
    }

    /// The answer is not the tool's to produce: the loop hands the question to
    /// the window and turns the reply into a tool result.
    fn asks_user(&self) -> bool {
        true
    }

    fn run(&self, _input: &Value) -> ToolResult {
        Err("ask_user is answered by the window, not by the tool".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(value: Value) -> Result<Question, String> {
        Question::parse(&value.to_string())
    }

    #[test]
    fn parses_a_question_with_options() {
        let question = parse(json!({
            "question": "  Which approach?  ",
            "options": ["Rewrite the parser", "Patch the caller"],
            "allow_free_text": true
        }))
        .expect("a question");

        assert_eq!(question.question, "Which approach?");
        assert_eq!(question.options, ["Rewrite the parser", "Patch the caller"]);
        assert!(question.allow_free_text);
    }

    #[test]
    fn free_text_alone_is_a_question() {
        let question = parse(json!({"question": "What is the domain?", "allow_free_text": true}))
            .expect("a question");

        assert!(question.options.is_empty());
        assert!(question.allow_free_text);
    }

    #[test]
    fn one_option_without_free_text_is_refused() {
        let error = parse(json!({ "question": "Go on?", "options": ["Yes"] }))
            .expect_err("one option is not a choice");

        assert!(error.contains("at least two `options`"), "{error}");
    }

    #[test]
    fn blank_options_do_not_count_and_labels_are_trimmed() {
        let question = parse(json!({
            "question": "Which?",
            "options": ["  one  ", "   "]
        }))
        .expect_err("one real option");
        assert!(question.contains("at least two"), "{question}");

        let question = parse(json!({
            "question": "Which?",
            "options": ["  one  ", "two", ""]
        }))
        .expect("two real options");
        assert_eq!(question.options, ["one", "two"]);
    }

    #[test]
    fn an_empty_question_is_refused() {
        let error =
            parse(json!({"question": "   ", "allow_free_text": true})).expect_err("no question");

        assert!(error.contains("empty"), "{error}");
    }

    #[test]
    fn the_answer_and_the_decline_are_worded_for_the_model() {
        assert_eq!(
            answer_text("Rewrite the parser"),
            "The user selected: Rewrite the parser"
        );
        assert!(DECLINED_TEXT.contains("declined"), "{DECLINED_TEXT}");
    }

    #[test]
    fn the_tool_hands_the_question_to_the_window_instead_of_running() {
        let tool = AskUser;

        assert!(tool.asks_user());
        assert!(!tool.needs_approval());
        assert_eq!(tool.name(), "ask_user");
        assert_eq!(tool.parameters()["required"], json!(["question"]));
        assert!(!tool.description().is_empty());
        let error = tool
            .run(&json!({"question": "Which?"}))
            .expect_err("not run");
        assert!(error.contains("answered by the window"), "{error}");
    }
}
