//! The system prompt: what the model is told before the conversation starts.
//!
//! It is assembled once per session and sent as the first message, so nothing
//! here may depend on the current time beyond the date the caller passes in:
//! a changing prefix would invalidate the provider's cache.
//!
//! The shape follows Zed's `system_prompt.hbs`: identity, how to work, what the
//! tools allow, the skills that can be loaded, how to change files, how to
//! answer, the user's own instructions, the curated memory, and the environment.
//! Sections that do not apply are left out rather than described as if they did.

use humibird_memory::{Memory, MemoryFile};
use humibird_skills::Skill;

pub struct PromptInputs<'a> {
    pub model: &'a str,
    pub os: &'a str,
    pub shell: &'a str,
    /// The caller decides this, so the prompt is stable for a whole session and
    /// testable without a clock.
    pub date: &'a str,
    pub workspace: &'a str,
    /// The tools this session has, by name. The prompt does not describe them:
    /// the request carries their schemas, and naming them here would make the
    /// prompt change whenever the list does. Two decisions still need
    /// the names — whether there are any tools at all, and whether the memory
    /// tool is among them.
    pub tools: &'a [&'a str],
    /// `SOUL.md`: the user's standing instructions for this profile.
    pub soul: Option<&'a str>,
    /// `memories/USER.md` as it was when the session started. `None` with
    /// `agent_memory` means the profile has no memory to show.
    pub user_memory: Option<&'a Memory>,
    /// `memories/MEMORY.md`, likewise frozen for the session.
    pub agent_memory: Option<&'a Memory>,
    /// The skills the profile has installed, in index order.
    pub skills: &'a [Skill],
}

pub fn build(inputs: &PromptInputs<'_>) -> String {
    let mut prompt = String::new();

    prompt.push_str(
        "You are humibird, an agent that runs on the user's own computer. You answer in a \
         conversation window and you can inspect and change files in one workspace.\n\n",
    );

    if inputs.tools.is_empty() {
        prompt.push_str(
            "You have no tools in this conversation: you can only answer from the conversation \
             itself. If an answer needs a file, a command, or current information, say what you \
             need instead of guessing.\n\n",
        );
    } else {
        prompt.push_str("## How you work\n\n");
        prompt.push_str(
            "- Gather context before acting: read the file, search for the symbol, list the \
             directory. Do not invent paths, APIs, or command output.\n\
             - Prefer the most direct tool. Use the file tools for files, the search tools for \
             finding things, and the terminal for builds, tests, and one-off commands.\n\
             - When a request is ambiguous in a way that changes what you do, ask; otherwise \
             keep going until the task is done.\n\
             - Say what you verified and how. Never claim a command passed or a test ran if you \
             did not see it.\n\n",
        );

        prompt.push_str("## Tools\n\n");
        prompt.push_str(
            "- The tool definitions in this request are the whole list; use only those, and give \
             every call the arguments its schema requires.\n\
             - Every path you pass is relative to the workspace root. Absolute paths, `..`, and \
             anything that resolves outside the workspace are refused.\n\
             - A tool that changes the workspace, or runs a command, asks the user first. If a \
             call is denied, do not retry it: explain what you would do instead, or ask.\n\
             - Terminal commands have a timeout and return stdout, stderr, and the exit code. Set \
             `cd` rather than prefixing a `cd` inside the command.\n\n",
        );

        if let Some(section) = describe_skills(inputs) {
            prompt.push_str(&section);
        }

        prompt.push_str("## Changing files\n\n");
        prompt.push_str(
            "- Read a file before you edit it.\n\
             - Prefer `edit_file` with a snippet that appears once over rewriting the whole file.\n\
             - Keep changes focused on the request; do not reformat or refactor unrelated code.\n\n",
        );
    }

    prompt.push_str("## Answering\n\n");
    prompt.push_str(
        "- Answer in markdown. Use backticks for paths, commands, and identifiers.\n\
         - Reference files by their workspace-relative path.\n\
         - Be concise; structured lists and tables only when they help.\n\n",
    );

    if let Some(soul) = inputs.soul.map(str::trim).filter(|soul| !soul.is_empty()) {
        prompt.push_str("## Instructions from the user\n\n");
        prompt.push_str(
            "These are the user's standing instructions for this profile. Follow them, and still \
             follow the tool rules above.\n\n",
        );
        prompt.push_str(soul);
        prompt.push_str("\n\n");
    }

    if let Some(section) = describe_memory(inputs) {
        prompt.push_str(&section);
    }

    prompt.push_str("## Environment\n\n");
    prompt.push_str(&format!("- Workspace: {}\n", inputs.workspace));
    prompt.push_str(&format!("- Operating system: {}\n", inputs.os));
    prompt.push_str(&format!("- Shell: {}\n", inputs.shell));
    prompt.push_str(&format!("- Date: {}\n", inputs.date));
    prompt.push_str(&format!("- Model: {}\n", inputs.model));

    prompt
}

/// The skill index: names and one-line descriptions, never the bodies. Reading a
/// body is a tool call the model makes when it decides the task needs it.
fn describe_skills(inputs: &PromptInputs<'_>) -> Option<String> {
    if inputs.skills.is_empty() {
        return None;
    }

    let mut section = String::from("## Skills\n\n");
    if inputs.tools.contains(&"read_skill") {
        section.push_str(
            "Written procedures you can load. When a task matches one, read it with \
             `read_skill` before following it; pass `file` to read a file the skill \
             mentions. A skill is a procedure to follow when it fits, not an \
             instruction to act now.\n\n",
        );
    } else {
        section.push_str("These procedures cannot be loaded in this conversation.\n\n");
    }

    for skill in inputs.skills {
        section.push_str(&format!("- `{}`: {}\n", skill.name, skill.description));
    }
    section.push('\n');
    Some(section)
}

/// The curated memory blocks. They are read once per session, so the prompt says
/// so: a mid-session write must not look as if it took effect immediately.
fn describe_memory(inputs: &PromptInputs<'_>) -> Option<String> {
    if inputs.user_memory.is_none() && inputs.agent_memory.is_none() {
        return None;
    }

    let mut section = String::from("## Memory\n\n");
    section.push_str(
        "You keep two lists across sessions: what you know about the user, and your own notes. \
         They are plain files that the user can read and edit, and an edit by the user wins. ",
    );
    if inputs.tools.contains(&"memory") {
        section.push_str(
            "Change them with the `memory` tool, addressing entries by the number shown. The \
             lists below were read when this session started, so a change you make now applies \
             from the next session.\n\n",
        );
    } else {
        section.push_str(
            "You cannot change them in this conversation. The lists below were read when this \
             session started.\n\n",
        );
    }

    for (memory, file) in [
        (inputs.user_memory, MemoryFile::User),
        (inputs.agent_memory, MemoryFile::Agent),
    ] {
        let Some(memory) = memory else { continue };
        section.push_str(&format!("### {}\n\n", file.heading()));
        if memory.is_empty() {
            section.push_str("Nothing yet.\n\n");
        } else {
            section.push_str(&memory.numbered());
            section.push_str("\n\n");
        }
    }

    Some(section)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tools() -> Vec<&'static str> {
        vec!["read_file", "write_file"]
    }

    fn inputs<'a>(tools: &'a [&'a str], soul: Option<&'a str>) -> PromptInputs<'a> {
        PromptInputs {
            model: "deepseek-flash",
            os: "macos",
            shell: "sh",
            date: "2026-10-30",
            workspace: "/Users/me/project",
            tools,
            soul,
            user_memory: None,
            agent_memory: None,
            skills: &[],
        }
    }

    fn with_memory_tool(mut tools: Vec<&'static str>) -> Vec<&'static str> {
        tools.push("memory");
        tools
    }

    #[test]
    fn describes_the_workspace_and_the_environment_but_not_the_tools() {
        let tools = tools();
        let prompt = build(&inputs(&tools, None));

        assert!(prompt.contains("You are humibird"), "{prompt}");
        // The request carries the schemas; a prompt that named them would change
        // whenever the tool list did, which would break the cached prefix.
        for absent in ["Available:", "`read_file`", "`write_file`"] {
            assert!(!prompt.contains(absent), "{absent:?} in:\n{prompt}");
        }
        for expected in [
            "- Workspace: /Users/me/project",
            "- Operating system: macos",
            "- Shell: sh",
            "- Date: 2026-10-30",
            "- Model: deepseek-flash",
            "relative to the workspace root",
        ] {
            assert!(
                prompt.contains(expected),
                "missing {expected:?} in:\n{prompt}"
            );
        }
        assert!(!prompt.contains("no tools"), "{prompt}");
    }

    #[test]
    fn without_tools_it_says_so_instead_of_describing_them() {
        let prompt = build(&inputs(&[], None));

        assert!(prompt.contains("You have no tools"), "{prompt}");
        assert!(!prompt.contains("## Tools"), "{prompt}");
        // The environment is still worth knowing.
        assert!(
            prompt.contains("- Workspace: /Users/me/project"),
            "{prompt}"
        );
    }

    #[test]
    fn the_soul_file_becomes_the_users_instructions() {
        let tools = tools();
        let prompt = build(&inputs(&tools, Some("\n  Be terse.  \n")));

        assert!(prompt.contains("## Instructions from the user"), "{prompt}");
        assert!(prompt.contains("Be terse."), "{prompt}");
        assert!(!prompt.contains("  Be terse.  "), "the text is trimmed");
    }

    #[test]
    fn a_blank_soul_file_adds_nothing() {
        let tools = tools();

        for soul in [None, Some(""), Some("  \n\t ")] {
            let prompt = build(&inputs(&tools, soul));
            assert!(
                !prompt.contains("## Instructions from the user"),
                "{soul:?} should not add a section"
            );
        }
    }

    #[test]
    fn the_same_inputs_produce_the_same_bytes() {
        // The prefix has to be stable for a whole session or the provider cache
        // is worthless.
        let tools = tools();

        let first = build(&inputs(&tools, Some("Be terse.")));
        let second = build(&inputs(&tools, Some("Be terse.")));

        assert_eq!(first, second);
    }

    #[test]
    fn memory_blocks_are_numbered_under_their_headings() {
        let tools = with_memory_tool(tools());
        let user = Memory::parse("Prefers short answers.\n§\nWorks in Rust.\n");
        let agent = Memory::parse("Keep notes in docs/.\n");

        let prompt = build(&PromptInputs {
            user_memory: Some(&user),
            agent_memory: Some(&agent),
            ..inputs(&tools, None)
        });

        for expected in [
            "## Memory",
            "### About the user",
            "1. Prefers short answers.\n2. Works in Rust.",
            "### Your notes",
            "1. Keep notes in docs/.",
            "Change them with the `memory` tool",
            "applies from the next session",
            "plain files that the user can read and edit",
        ] {
            assert!(
                prompt.contains(expected),
                "missing {expected:?} in:\n{prompt}"
            );
        }
    }

    #[test]
    fn an_empty_memory_says_nothing_yet() {
        let tools = tools();
        let empty = Memory::default();

        let prompt = build(&PromptInputs {
            user_memory: Some(&empty),
            agent_memory: Some(&empty),
            ..inputs(&tools, None)
        });

        assert!(
            prompt.contains("### About the user\n\nNothing yet."),
            "{prompt}"
        );
        assert!(
            prompt.contains("### Your notes\n\nNothing yet."),
            "{prompt}"
        );
    }

    #[test]
    fn skills_appear_as_an_index_of_names_and_descriptions() {
        let mut tools = with_memory_tool(tools());
        tools.push("read_skill");
        let skill = Skill::new(
            "jd-card",
            "turns a JD into a one-page card",
            "/skills/jd-card",
        );

        let prompt = build(&PromptInputs {
            skills: std::slice::from_ref(&skill),
            ..inputs(&tools, None)
        });

        for expected in [
            "## Skills",
            "`read_skill`",
            "- `jd-card`: turns a JD into a one-page card",
            "not an instruction to act now",
        ] {
            assert!(
                prompt.contains(expected),
                "missing {expected:?} in:\n{prompt}"
            );
        }
    }

    #[test]
    fn no_skills_add_no_skills_section() {
        let tools = tools();

        let prompt = build(&inputs(&tools, None));

        assert!(!prompt.contains("## Skills"), "{prompt}");
    }

    #[test]
    fn skills_without_the_tool_say_so_instead_of_promising_a_read() {
        let tools = tools();
        let skill = Skill::new(
            "jd-card",
            "turns a JD into a one-page card",
            "/skills/jd-card",
        );

        let prompt = build(&PromptInputs {
            skills: std::slice::from_ref(&skill),
            ..inputs(&tools, None)
        });

        assert!(prompt.contains("cannot be loaded"), "{prompt}");
        assert!(!prompt.contains("read it with"), "{prompt}");
    }

    #[test]
    fn no_memory_inputs_add_no_memory_section() {
        let tools = tools();

        let prompt = build(&inputs(&tools, None));

        assert!(!prompt.contains("## Memory"), "{prompt}");
        assert!(!prompt.contains("Nothing yet."), "{prompt}");
    }

    #[test]
    fn without_the_memory_tool_the_prompt_does_not_promise_edits() {
        let tools = tools();
        let user = Memory::parse("Works in Rust.\n");

        let prompt = build(&PromptInputs {
            user_memory: Some(&user),
            agent_memory: None,
            ..inputs(&tools, None)
        });

        assert!(prompt.contains("### About the user"), "{prompt}");
        assert!(prompt.contains("You cannot change them"), "{prompt}");
        assert!(!prompt.contains("`memory` tool"), "{prompt}");
        assert!(!prompt.contains("### Your notes"), "{prompt}");
    }
}
