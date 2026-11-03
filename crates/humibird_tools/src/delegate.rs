//! The `delegate` tool: one prompt handed to a subagent, which answers with its
//! final text.

use serde::Deserialize;
use serde_json::{Value, json};

use crate::{CallKind, CallLabel, Tool, ToolResult};

/// What the model reads when the subagent never answered: nobody was listening, or
/// the window went away. An error, so the parent can react instead of assuming an
/// empty answer.
pub const CANCELLED_TEXT: &str = "The subagent was cancelled before it answered.";

/// A validated `delegate` call. The conversation runs a second agent over it; the
/// tool itself never executes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Delegation {
    /// What the subagent is for, as the parent's timeline shows it.
    pub description: String,
    /// The whole task, written as if the subagent were being spoken to: it has none
    /// of this conversation's context.
    pub prompt: String,
}

#[derive(Deserialize)]
struct DelegateInput {
    description: String,
    prompt: String,
}

impl Delegation {
    /// Parses the raw arguments of the call, the way the loop receives them.
    pub fn parse(arguments: &str) -> Result<Self, String> {
        let input: DelegateInput = serde_json::from_str(arguments)
            .map_err(|error| format!("invalid arguments: {error}"))?;

        let description = input.description.trim().to_owned();
        if description.is_empty() {
            return Err("the description is empty".to_owned());
        }
        let prompt = input.prompt.trim().to_owned();
        if prompt.is_empty() {
            return Err("the prompt is empty".to_owned());
        }

        Ok(Self {
            description,
            prompt,
        })
    }
}

pub struct Delegate;

impl Tool for Delegate {
    fn name(&self) -> &str {
        "delegate"
    }

    fn kind(&self) -> CallKind {
        CallKind::Delegate
    }

    fn label(&self, input: &Value) -> Option<CallLabel> {
        let input: DelegateInput = serde_json::from_value(input.clone()).ok()?;
        Some(CallLabel::new("Delegate", input.description))
    }

    fn description(&self) -> &str {
        "Hand one self-contained task to a subagent that works with its own context \
         and answers with its final report. Use it for work that would bury this \
         conversation in detail — reading across many files, a search that needs \
         several attempts — and say what to investigate and what to report. The \
         subagent cannot ask you questions or see anything of this conversation, and \
         the reply is the only thing you get back."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "description": {
                    "type": "string",
                    "description": "A short label for the task, shown in the timeline. Three to six words."
                },
                "prompt": {
                    "type": "string",
                    "description": "The whole task, written as a self-contained instruction: what to look at, what to decide, and what the report must contain."
                }
            },
            "required": ["description", "prompt"]
        })
    }

    /// The answer comes from a second conversation, not from the tool.
    fn delegates(&self) -> bool {
        true
    }

    fn run(&self, _input: &Value) -> ToolResult {
        Err("delegate is run by the conversation, not by the tool".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(value: Value) -> Result<Delegation, String> {
        Delegation::parse(&value.to_string())
    }

    #[test]
    fn parses_a_delegation() {
        let delegation = parse(json!({
            "description": "  find the flaky test  ",
            "prompt": "  Run the test suite ten times and report which test fails.  "
        }))
        .expect("a delegation");

        assert_eq!(delegation.description, "find the flaky test");
        assert_eq!(
            delegation.prompt,
            "Run the test suite ten times and report which test fails."
        );
    }

    #[test]
    fn an_empty_prompt_is_not_a_task() {
        assert!(parse(json!({ "description": "look", "prompt": "  " })).is_err());
        assert!(parse(json!({ "description": " ", "prompt": "look" })).is_err());
        assert!(parse(json!({ "description": "look" })).is_err());
    }
}
