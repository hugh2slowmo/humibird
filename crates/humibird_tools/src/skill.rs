//! The `read_skill` tool: a skill's procedure, or a file next to it.

use std::sync::Arc;

use humibird_skills::SkillIndex;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{CallKind, CallLabel, Tool, ToolResult};

pub struct ReadSkill {
    index: Arc<SkillIndex>,
}

impl ReadSkill {
    pub fn new(index: Arc<SkillIndex>) -> Self {
        Self { index }
    }
}

#[derive(Deserialize)]
struct ReadSkillInput {
    name: String,
    #[serde(default)]
    file: Option<String>,
}

impl Tool for ReadSkill {
    fn name(&self) -> &str {
        "read_skill"
    }

    fn kind(&self) -> CallKind {
        CallKind::Skill
    }

    fn label(&self, input: &Value) -> Option<CallLabel> {
        let input: ReadSkillInput = serde_json::from_value(input.clone()).ok()?;
        let target = match input.file {
            Some(file) => format!("{}/{}", input.name, file),
            None => input.name,
        };
        Some(CallLabel::new("Read skill", target))
    }

    fn description(&self) -> &str {
        "Read a skill's procedure, or a file that sits next to it. The skills \
         installed for this profile are listed in your instructions; read one \
         before following it, and follow its paths relative to the skill."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "The skill's name, as listed in your instructions."
                },
                "file": {
                    "type": "string",
                    "description": "A file inside the skill's directory, relative to it, such as `references/template.md`. Omit to read `SKILL.md` itself."
                }
            },
            "required": ["name"]
        })
    }

    fn run(&self, input: &Value) -> ToolResult {
        let input: ReadSkillInput = serde_json::from_value(input.clone())
            .map_err(|error| format!("invalid arguments: {error}"))?;

        let read = match input.file.as_deref() {
            Some(file) => self.index.read_file(&input.name, file),
            None => self.index.read(&input.name),
        };
        read.map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn index_with_skill(directory: &Path) -> Arc<SkillIndex> {
        let skills = directory.join("skills");
        let skill = skills.join("card");
        std::fs::create_dir_all(&skill).expect("create the skill");
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: card\ndescription: makes a card\n---\n\n# Card\n",
        )
        .expect("write SKILL.md");
        std::fs::create_dir_all(skill.join("references")).expect("create references");
        std::fs::write(skill.join("references/template.md"), "template\n").expect("write");
        Arc::new(SkillIndex::load(&skills))
    }

    fn run(tool: &ReadSkill, input: Value) -> ToolResult {
        tool.run(&input)
    }

    #[test]
    fn reads_the_body_and_a_bundled_file() {
        let directory = tempfile::tempdir().expect("temp dir");
        let tool = ReadSkill::new(index_with_skill(directory.path()));

        let body = run(&tool, json!({"name": "card"})).expect("the body");
        assert!(body.contains("# Card"), "{body}");

        let template = run(
            &tool,
            json!({"name": "card", "file": "references/template.md"}),
        )
        .expect("the file");
        assert_eq!(template, "template\n");
    }

    #[test]
    fn an_unknown_skill_says_what_is_installed() {
        let directory = tempfile::tempdir().expect("temp dir");
        let tool = ReadSkill::new(index_with_skill(directory.path()));

        let error = run(&tool, json!({"name": "nope"})).expect_err("unknown");

        assert!(error.contains("installed: card"), "{error}");
    }

    #[test]
    fn a_path_outside_the_skill_is_refused() {
        let directory = tempfile::tempdir().expect("temp dir");
        let tool = ReadSkill::new(index_with_skill(directory.path()));

        let error = run(&tool, json!({"name": "card", "file": "../secret"})).expect_err("escape");

        assert!(error.contains("outside the skill"), "{error}");
    }

    #[test]
    fn reads_without_asking_for_approval() {
        let directory = tempfile::tempdir().expect("temp dir");
        let tool = ReadSkill::new(index_with_skill(directory.path()));

        assert!(!tool.needs_approval());
        assert_eq!(tool.parameters()["required"], json!(["name"]));
        assert!(!tool.description().is_empty());
    }
}
