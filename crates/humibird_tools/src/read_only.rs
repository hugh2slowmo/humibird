//! The read-only tools: `read_file`, `list_directory`, `find_path`, `grep`.

use std::path::Path;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::{CallKind, CallLabel, Tool, ToolResult, Workspace};

/// A single call may not return more than this many lines; the model can page
/// with `start_line` / `end_line`.
const MAX_LINES: u32 = 2000;
/// Long lines (minified assets, source maps, data rows) would eat the context.
const MAX_LINE_CHARS: usize = 2000;

pub struct ReadFile {
    workspace: Workspace,
}

impl ReadFile {
    pub fn new(workspace: Workspace) -> Self {
        Self { workspace }
    }
}

#[derive(Deserialize)]
struct ReadFileInput {
    path: String,
    #[serde(default)]
    start_line: Option<u32>,
    #[serde(default)]
    end_line: Option<u32>,
}

impl Tool for ReadFile {
    fn name(&self) -> &str {
        "read_file"
    }

    fn kind(&self) -> CallKind {
        CallKind::Read
    }

    fn label(&self, input: &Value) -> Option<CallLabel> {
        let input: ReadFileInput = serde_json::from_value(input.clone()).ok()?;
        Some(CallLabel::new("Read", input.path))
    }

    fn description(&self) -> &str {
        "Read a file from the workspace and return it with line numbers.\n\
         Read a range with `start_line` and `end_line` (1-based, inclusive) when \
         a file is long; reading a large file whole is wasteful."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path relative to the workspace root."
                },
                "start_line": {
                    "type": "integer",
                    "description": "First line to return, 1-based. Defaults to the first line."
                },
                "end_line": {
                    "type": "integer",
                    "description": "Last line to return, inclusive. Defaults to the last line."
                }
            },
            "required": ["path"]
        })
    }

    fn run(&self, input: &Value) -> ToolResult {
        let input: ReadFileInput = serde_json::from_value(input.clone())
            .map_err(|error| format!("invalid arguments: {error}"))?;
        let path = self.workspace.resolve(&input.path)?;
        let contents = std::fs::read(&path).map_err(|error| format!("{}: {error}", input.path))?;
        let contents = String::from_utf8(contents).map_err(|_| {
            format!(
                "{} is not UTF-8 text; use a command to inspect it",
                input.path
            )
        })?;

        if contents.is_empty() {
            return Ok(format!("{} is empty", input.path));
        }

        let first = input.start_line.unwrap_or(1).max(1);
        let last = input.end_line.unwrap_or(u32::MAX).max(first);
        let line_count = contents.lines().count() as u32;

        if first > line_count {
            return Ok(format!(
                "{} has {line_count} lines; start_line {first} is past the end",
                input.path
            ));
        }

        let mut output = String::new();
        let mut returned = 0;
        let mut truncated = false;
        for (index, line) in contents.lines().enumerate() {
            let number = index as u32 + 1;
            if number < first {
                continue;
            }
            if number > last {
                break;
            }
            if returned == MAX_LINES {
                truncated = true;
                break;
            }
            let line = truncate(line, MAX_LINE_CHARS);
            output.push_str(&format!("{number:>6}\t{line}\n"));
            returned += 1;
        }

        if truncated {
            output.push_str(&format!(
                "... (more lines follow; file has {line_count} lines, read a range with start_line and end_line)\n"
            ));
        }
        Ok(output)
    }
}

fn truncate(line: &str, max_chars: usize) -> String {
    if line.chars().count() <= max_chars {
        return line.to_owned();
    }
    let mut truncated: String = line.chars().take(max_chars).collect();
    truncated.push_str(" ... (line truncated)");
    truncated
}

const MAX_ENTRIES: usize = 200;

pub struct ListDirectory {
    workspace: Workspace,
}

impl ListDirectory {
    pub fn new(workspace: Workspace) -> Self {
        Self { workspace }
    }
}

#[derive(Deserialize)]
struct ListDirectoryInput {
    #[serde(default)]
    path: Option<String>,
}

impl Tool for ListDirectory {
    fn name(&self) -> &str {
        "list_directory"
    }

    fn kind(&self) -> CallKind {
        CallKind::List
    }

    fn label(&self, input: &Value) -> Option<CallLabel> {
        let input: ListDirectoryInput = serde_json::from_value(input.clone()).ok()?;
        Some(CallLabel::new(
            "List",
            input.path.unwrap_or_else(|| ".".to_owned()),
        ))
    }

    fn description(&self) -> &str {
        "List the entries of a directory in the workspace. Directories are \
         marked with a trailing `/`. Use `grep` to search contents instead of \
         walking directories by hand."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Directory relative to the workspace root. Defaults to the root."
                }
            }
        })
    }

    fn run(&self, input: &Value) -> ToolResult {
        let input: ListDirectoryInput = serde_json::from_value(input.clone())
            .map_err(|error| format!("invalid arguments: {error}"))?;
        let relative = input.path.unwrap_or_else(|| ".".to_owned());
        let path = self.workspace.resolve(&relative)?;

        let entries = std::fs::read_dir(&path)
            .map_err(|error| format!("{relative} is not a readable directory: {error}"))?;

        let mut directories = Vec::new();
        let mut files = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => directories.push(format!("{name}/")),
                Ok(_) => files.push(name),
                Err(_) => files.push(name),
            }
        }
        directories.sort();
        files.sort();

        let mut output = String::new();
        let all = directories.iter().chain(files.iter());
        for (index, name) in all.enumerate() {
            if index == MAX_ENTRIES {
                output.push_str("... (more entries; narrow the path)\n");
                break;
            }
            output.push_str(name);
            output.push('\n');
        }

        if output.is_empty() {
            return Ok(format!("{relative} is empty"));
        }
        Ok(output)
    }
}

const DEFAULT_MAX_RESULTS: usize = 50;
const MAX_RESULTS: usize = 200;
const MAX_SEARCHED_FILE_BYTES: u64 = 1024 * 1024;

/// Paths are cheap next to line matches, so a name search returns more of them.
const DEFAULT_MAX_PATHS: usize = 100;
const MAX_PATHS: usize = 500;

pub struct Grep {
    workspace: Workspace,
}

impl Grep {
    pub fn new(workspace: Workspace) -> Self {
        Self { workspace }
    }
}

#[derive(Deserialize)]
struct GrepInput {
    regex: String,
    #[serde(default)]
    include_pattern: Option<String>,
    #[serde(default)]
    case_sensitive: Option<bool>,
    #[serde(default)]
    max_results: Option<usize>,
}

impl Tool for Grep {
    fn name(&self) -> &str {
        "grep"
    }

    fn kind(&self) -> CallKind {
        CallKind::Search
    }

    fn label(&self, input: &Value) -> Option<CallLabel> {
        let input: GrepInput = serde_json::from_value(input.clone()).ok()?;
        Some(CallLabel::new("Search", input.regex))
    }

    fn description(&self) -> &str {
        "Search file contents with a regular expression. Respects .gitignore and \
         skips hidden files, so it searches the project rather than the junk in \
         it. Results are `path:line: text`."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "regex": {
                    "type": "string",
                    "description": "Rust regular expression syntax, e.g. `fn\\s+\\w+`."
                },
                "include_pattern": {
                    "type": "string",
                    "description": "Optional glob limiting which files are searched, e.g. `src/**/*.rs`."
                },
                "case_sensitive": {
                    "type": "boolean",
                    "description": "Defaults to false; set true to match case exactly."
                },
                "max_results": {
                    "type": "integer",
                    "description": "Maximum number of matching lines. Defaults to 50."
                }
            },
            "required": ["regex"]
        })
    }

    fn run(&self, input: &Value) -> ToolResult {
        let input: GrepInput = serde_json::from_value(input.clone())
            .map_err(|error| format!("invalid arguments: {error}"))?;

        let regex = regex::RegexBuilder::new(&input.regex)
            .case_insensitive(!input.case_sensitive.unwrap_or(false))
            .build()
            .map_err(|error| format!("invalid regular expression: {error}"))?;

        let limit = input
            .max_results
            .unwrap_or(DEFAULT_MAX_RESULTS)
            .clamp(1, MAX_RESULTS);

        let mut walker = ignore::WalkBuilder::new(self.workspace.root());
        if let Some(pattern) = &input.include_pattern {
            let mut overrides = ignore::overrides::OverrideBuilder::new(self.workspace.root());
            overrides
                .add(pattern)
                .map_err(|error| format!("invalid include_pattern {pattern:?}: {error}"))?;
            walker.overrides(
                overrides
                    .build()
                    .map_err(|error| format!("invalid include_pattern {pattern:?}: {error}"))?,
            );
        }

        let mut matches = Vec::new();
        let mut truncated = false;
        'search: for entry in walker.build().flatten() {
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            if entry
                .metadata()
                .is_ok_and(|metadata| metadata.len() > MAX_SEARCHED_FILE_BYTES)
            {
                continue;
            }
            // Anything that is not UTF-8 text (binaries, images) is not worth
            // reporting a line match in.
            let Ok(contents) = std::fs::read_to_string(entry.path()) else {
                continue;
            };

            for (index, line) in contents.lines().enumerate() {
                if regex.is_match(line) {
                    let path = self.workspace.display(entry.path());
                    let line = truncate(line.trim(), 300);
                    matches.push(format!("{path}:{}: {line}", index + 1));
                    if matches.len() == limit {
                        truncated = true;
                        break 'search;
                    }
                }
            }
        }

        if matches.is_empty() {
            return Ok(format!("no matches for {:?}", input.regex));
        }
        matches.sort();
        if truncated {
            matches.push(format!(
                "... (stopped at {limit} matches; narrow the search)"
            ));
        }
        let mut output = matches.join("\n");
        output.push('\n');
        Ok(output)
    }
}

/// Finds files and directories by name, with the same walker `grep` uses: what
/// the project ignores is not searched by a path either.
pub struct FindPath {
    workspace: Workspace,
}

impl FindPath {
    pub fn new(workspace: Workspace) -> Self {
        Self { workspace }
    }
}

#[derive(Deserialize)]
struct FindPathInput {
    glob: String,
    #[serde(default)]
    max_results: Option<usize>,
}

impl Tool for FindPath {
    fn name(&self) -> &str {
        "find_path"
    }

    fn kind(&self) -> CallKind {
        CallKind::Search
    }

    fn label(&self, input: &Value) -> Option<CallLabel> {
        let input: FindPathInput = serde_json::from_value(input.clone()).ok()?;
        Some(CallLabel::new("Find", input.glob))
    }

    fn description(&self) -> &str {
        "Find files and directories by name with a glob, e.g. `**/*.rs` or \
         `src/**/mod.rs`. A glob with no `/` matches a name at any depth, like \
         `*config*`. Respects .gitignore and skips hidden files, like `grep`. \
         Directories come back with a trailing `/`. This matches names; use `grep` \
         to search contents."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "glob": {
                    "type": "string",
                    "description": "Glob matched against every path in the workspace, e.g. `**/*.rs` or `*config*`."
                },
                "max_results": {
                    "type": "integer",
                    "description": "Maximum number of paths. Defaults to 100."
                }
            },
            "required": ["glob"]
        })
    }

    fn run(&self, input: &Value) -> ToolResult {
        let input: FindPathInput = serde_json::from_value(input.clone())
            .map_err(|error| format!("invalid arguments: {error}"))?;
        let limit = input
            .max_results
            .unwrap_or(DEFAULT_MAX_PATHS)
            .clamp(1, MAX_PATHS);

        // Matched here rather than through `ignore`'s overrides: an override is a
        // whitelist that outranks .gitignore, so `**/*.rs` would bring back the
        // files the project already decided to ignore.
        let matcher = globset::GlobBuilder::new(&input.glob)
            .literal_separator(true)
            .build()
            .map_err(|error| format!("invalid glob {:?}: {error}", input.glob))?
            .compile_matcher();
        // A glob with no separator matches a name at any depth, which is what
        // `*.rs` means to a person.
        let by_name = !input.glob.contains('/');

        let mut paths = Vec::new();
        let mut truncated = false;
        for entry in ignore::WalkBuilder::new(self.workspace.root())
            .build()
            .flatten()
        {
            let path = entry.path();
            if path == self.workspace.root() {
                continue;
            }
            let relative = path.strip_prefix(self.workspace.root()).unwrap_or(path);
            let subject = if by_name {
                relative.file_name().map(Path::new).unwrap_or(relative)
            } else {
                relative
            };
            if !matcher.is_match(subject) {
                continue;
            }
            let is_directory = entry.file_type().is_some_and(|kind| kind.is_dir());
            let shown = self.workspace.display(path);
            paths.push(if is_directory {
                format!("{shown}/")
            } else {
                shown
            });
            if paths.len() == limit {
                truncated = true;
                break;
            }
        }

        if paths.is_empty() {
            return Ok(format!("nothing matches {:?}", input.glob));
        }
        paths.sort();
        if truncated {
            paths.push(format!("... (stopped at {limit} paths; narrow the glob)"));
        }
        let mut output = paths.join("\n");
        output.push('\n');
        Ok(output)
    }
}

/// Every file under `root` that the project does not ignore, relative and
/// slash-separated, capped at `limit`. This is what the composer's `@` menu
/// offers; hidden files and build output stay out for the same reason `grep`
/// skips them. The cap keeps a monorepo from stalling the composer.
pub fn workspace_files(root: &Path, limit: usize) -> Vec<String> {
    let mut files = Vec::new();
    for entry in ignore::WalkBuilder::new(root).build().flatten() {
        let path = entry.path();
        if path == root || !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let relative = path.strip_prefix(root).unwrap_or(path);
        files.push(relative.to_string_lossy().replace('\\', "/"));
        if files.len() == limit {
            break;
        }
    }
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace_with(files: &[(&str, &str)]) -> (tempfile::TempDir, Workspace) {
        let directory = tempfile::tempdir().expect("temp dir");
        for (path, contents) in files {
            let path = directory.path().join(path);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("create parent");
            }
            std::fs::write(path, contents).expect("write file");
        }
        let workspace = Workspace::new(directory.path()).expect("workspace");
        (directory, workspace)
    }

    fn run(tool: &dyn Tool, input: Value) -> ToolResult {
        tool.run(&input)
    }

    #[test]
    fn read_file_numbers_lines() {
        let (_directory, workspace) = workspace_with(&[("src/main.rs", "fn main() {}\n// done\n")]);

        let output = run(&ReadFile::new(workspace), json!({"path": "src/main.rs"})).expect("read");

        assert_eq!(output, "     1\tfn main() {}\n     2\t// done\n");
    }

    #[test]
    fn read_file_honours_a_line_range() {
        let (_directory, workspace) = workspace_with(&[("notes.txt", "one\ntwo\nthree\nfour\n")]);

        let output = run(
            &ReadFile::new(workspace),
            json!({"path": "notes.txt", "start_line": 2, "end_line": 3}),
        )
        .expect("read");

        assert_eq!(output, "     2\ttwo\n     3\tthree\n");
    }

    #[test]
    fn read_file_refuses_a_binary_file() {
        let directory = tempfile::tempdir().expect("temp dir");
        std::fs::write(directory.path().join("blob.bin"), [0xff, 0xfe, 0x00]).expect("write");
        let workspace = Workspace::new(directory.path()).expect("workspace");

        let error =
            run(&ReadFile::new(workspace), json!({"path": "blob.bin"})).expect_err("binary");

        assert!(error.contains("not UTF-8"), "{error}");
    }

    #[test]
    fn read_file_says_when_the_file_is_empty() {
        let (_directory, workspace) = workspace_with(&[("empty.txt", "")]);

        let output = run(&ReadFile::new(workspace), json!({"path": "empty.txt"})).expect("read");

        assert!(output.contains("empty"), "{output}");
    }

    #[test]
    fn list_directory_puts_directories_first() {
        let (_directory, workspace) =
            workspace_with(&[("b.txt", ""), ("a.txt", ""), ("src/main.rs", "")]);

        let output = run(&ListDirectory::new(workspace), json!({})).expect("list");

        assert_eq!(output, "src/\na.txt\nb.txt\n");
    }

    #[test]
    fn grep_reports_path_line_and_text() {
        let (_directory, workspace) = workspace_with(&[
            ("src/main.rs", "fn main() {}\nlet todo = 1;\n"),
            ("README.md", "no matches here\n"),
        ]);

        let output = run(
            &Grep::new(workspace),
            json!({"regex": "todo", "include_pattern": "**/*.rs"}),
        )
        .expect("grep");

        assert_eq!(output, "src/main.rs:2: let todo = 1;\n");
    }

    #[test]
    fn grep_is_case_insensitive_by_default() {
        let (_directory, workspace) = workspace_with(&[("a.txt", "TODO: fix\n")]);
        let workspace_a = workspace.clone();
        let workspace_b = workspace;

        let insensitive = run(&Grep::new(workspace_a), json!({"regex": "todo"})).expect("grep");
        let sensitive = run(
            &Grep::new(workspace_b),
            json!({"regex": "todo", "case_sensitive": true}),
        )
        .expect("grep");

        assert!(insensitive.contains("a.txt:1"), "{insensitive}");
        assert!(sensitive.contains("no matches"), "{sensitive}");
    }

    #[test]
    fn grep_rejects_an_invalid_regex() {
        let (_directory, workspace) = workspace_with(&[("a.txt", "hi")]);

        let error = run(&Grep::new(workspace), json!({"regex": "("})).expect_err("invalid");

        assert!(error.contains("invalid regular expression"), "{error}");
    }

    #[test]
    fn find_path_matches_names_not_contents() {
        let (_directory, workspace) = workspace_with(&[
            ("src/main.rs", "fn main() {}\n"),
            ("src/deep/mod.rs", "mod x;\n"),
            ("docs/readme.md", "mentions src/main.rs and *.rs\n"),
        ]);

        let output = run(&FindPath::new(workspace), json!({"glob": "**/*.rs"})).expect("find");

        assert_eq!(output, "src/deep/mod.rs\nsrc/main.rs\n");
    }

    #[test]
    fn find_path_marks_directories_with_a_slash() {
        let (_directory, workspace) =
            workspace_with(&[("docs/api.md", "\n"), ("src/lib.rs", "\n")]);

        let output = run(&FindPath::new(workspace), json!({"glob": "*"})).expect("find");

        assert!(output.contains("docs/\n"), "{output}");
        assert!(output.contains("src/\n"), "{output}");
    }

    #[test]
    fn the_composer_file_list_is_relative_and_leaves_hidden_files_out() {
        let (_directory, workspace) = workspace_with(&[
            ("README.md", "# hi"),
            ("src/main.rs", "fn main() {}"),
            (".hidden", "not offered"),
        ]);

        let mut files = workspace_files(workspace.root(), 10);
        files.sort();

        assert_eq!(
            files,
            vec!["README.md".to_owned(), "src/main.rs".to_owned()],
            "a mention is a path, and a hidden file is not offered"
        );
    }

    #[test]
    fn the_composer_file_list_stops_at_its_limit() {
        let (_directory, workspace) =
            workspace_with(&[("a.txt", ""), ("b.txt", ""), ("c.txt", "")]);

        assert_eq!(workspace_files(workspace.root(), 2).len(), 2);
    }

    #[test]
    fn find_path_skips_what_the_project_ignores() {
        // `.gitignore` applies where a `.git` directory says it is a project, the
        // same as `ripgrep` and `grep` here.
        let (_directory, workspace) = workspace_with(&[
            (".git/HEAD", "ref: refs/heads/main\n"),
            (".gitignore", "target/\n"),
            ("src/main.rs", "\n"),
            ("target/debug/old.rs", "\n"),
            (".hidden/secret.rs", "\n"),
        ]);

        let output = run(&FindPath::new(workspace), json!({"glob": "**/*.rs"})).expect("find");

        assert_eq!(output, "src/main.rs\n", "ignored and hidden files stay out");
    }

    #[test]
    fn find_path_says_so_when_nothing_matches() {
        let (_directory, workspace) = workspace_with(&[("a.txt", "hi")]);

        let output = run(&FindPath::new(workspace), json!({"glob": "**/*.py"})).expect("find");

        assert!(output.contains("nothing matches"), "{output}");
    }

    #[test]
    fn find_path_says_when_it_stopped_early() {
        let (_directory, workspace) =
            workspace_with(&[("a.rs", "\n"), ("b.rs", "\n"), ("c.rs", "\n")]);

        let output = run(
            &FindPath::new(workspace),
            json!({"glob": "**/*.rs", "max_results": 2}),
        )
        .expect("find");

        assert!(output.contains("stopped at 2 paths"), "{output}");
    }

    #[test]
    fn find_path_rejects_a_bad_glob() {
        let (_directory, workspace) = workspace_with(&[("a.txt", "hi")]);

        let error =
            run(&FindPath::new(workspace), json!({"glob": "[unclosed"})).expect_err("invalid glob");

        assert!(error.contains("invalid glob"), "{error}");
    }

    #[test]
    fn every_tool_declares_a_schema_and_a_description() {
        let (_directory, workspace) = workspace_with(&[]);
        for tool in crate::read_only_tools(workspace) {
            let schema = tool.parameters();
            assert_eq!(schema["type"], "object", "{}", tool.name());
            assert!(!tool.description().is_empty(), "{}", tool.name());
            assert!(
                schema["properties"].is_object(),
                "{} has no properties",
                tool.name()
            );
        }
    }
}
