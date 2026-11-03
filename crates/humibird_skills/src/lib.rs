//! Skills: written procedures the model loads on demand.
//!
//! A skill is a directory under the profile's `skills/` with a `SKILL.md` at its
//! root. Only the frontmatter's name and description reach the system prompt; the
//! body is read with a tool when the model decides it needs the procedure.
//!
//! Everything here is plain files, so the index is cheap to rebuild and the user
//! can read — and edit — exactly what the agent will.

use std::path::{Component, Path, PathBuf};

/// The file that makes a directory a skill.
pub const SKILL_FILE: &str = "SKILL.md";
/// How many skills the index lists. Beyond this the prompt would cost more than
/// the procedures are worth.
pub const MAX_SKILLS: usize = 40;
/// The whole index budget, in characters.
pub const MAX_INDEX_CHARS: usize = 4_000;
/// A description longer than this is cut when it enters the index.
pub const MAX_DESCRIPTION_CHARS: usize = 200;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    root: PathBuf,
}

impl Skill {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        root: impl Into<PathBuf>,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            root: root.into(),
        }
    }

    /// The directory the skill lives in, which is the root `read_skill` is
    /// confined to.
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn markdown_path(&self) -> PathBuf {
        self.root.join(SKILL_FILE)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SkillIndex {
    skills: Vec<Skill>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SkillError {
    Unknown {
        name: String,
        available: Vec<String>,
    },
    Outside {
        path: String,
    },
    Missing {
        path: String,
    },
    Unreadable {
        path: String,
        reason: String,
    },
    NotASkill {
        path: String,
    },
    AlreadyInstalled {
        name: String,
    },
}

impl std::fmt::Display for SkillError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown { name, available } => {
                if available.is_empty() {
                    write!(
                        formatter,
                        "there is no skill named {name:?}; no skills are installed"
                    )
                } else {
                    write!(
                        formatter,
                        "there is no skill named {name:?}; installed: {}",
                        available.join(", ")
                    )
                }
            }
            Self::Outside { path } => write!(
                formatter,
                "{path:?} is outside the skill; paths are relative to its directory"
            ),
            Self::Missing { path } => write!(formatter, "{path:?} does not exist"),
            Self::Unreadable { path, reason } => {
                write!(formatter, "{path:?} cannot be read: {reason}")
            }
            Self::NotASkill { path } => write!(
                formatter,
                "{path:?} is not a skill: {SKILL_FILE} needs a frontmatter block with a description"
            ),
            Self::AlreadyInstalled { name } => write!(
                formatter,
                "a skill named {name:?} is already installed; delete it first to import again"
            ),
        }
    }
}

impl std::error::Error for SkillError {}

impl SkillIndex {
    /// Reads `directory/*/SKILL.md`. A missing directory is an empty index: a
    /// profile that has never installed a skill is not an error.
    pub fn load(directory: &Path) -> Self {
        let mut paths = match std::fs::read_dir(directory) {
            Ok(entries) => entries
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .collect::<Vec<_>>(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Self::default();
            }
            Err(error) => {
                log::warn!("could not read {}: {error}", directory.display());
                return Self::default();
            }
        };
        // Sorted, so the index is byte-identical for the same set of skills.
        paths.sort();

        let mut index = Self::default();
        let mut budget = MAX_INDEX_CHARS;
        for path in paths {
            if !path.is_dir() {
                continue;
            }
            let fallback_name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_owned();
            let Some(skill) = read_skill(&path, &fallback_name) else {
                continue;
            };
            if index.skills.len() == MAX_SKILLS {
                log::warn!(
                    "ignoring {}: only {MAX_SKILLS} skills are listed",
                    skill.name
                );
                continue;
            }
            let cost = skill.name.chars().count() + skill.description.chars().count();
            if cost > budget {
                log::warn!(
                    "ignoring {}: the skill index is limited to {MAX_INDEX_CHARS} characters",
                    skill.name
                );
                continue;
            }
            budget -= cost;
            index.skills.push(skill);
        }
        index
    }

    pub fn skills(&self) -> &[Skill] {
        &self.skills
    }

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|skill| skill.name == name)
    }

    /// The whole `SKILL.md`, frontmatter included: the model already has the
    /// name and description, and seeing them again costs nothing.
    pub fn read(&self, name: &str) -> Result<String, SkillError> {
        let skill = self.find(name)?;
        let path = skill.markdown_path();
        std::fs::read_to_string(&path).map_err(|error| SkillError::Unreadable {
            path: format!("{name}/{SKILL_FILE}"),
            reason: error.to_string(),
        })
    }

    /// A file next to the skill's `SKILL.md`, confined to the skill's directory.
    pub fn read_file(&self, name: &str, file: &str) -> Result<String, SkillError> {
        let skill = self.find(name)?;
        let relative = Path::new(file);
        if relative.is_absolute()
            || relative
                .components()
                .any(|component| matches!(component, Component::ParentDir))
        {
            return Err(SkillError::Outside {
                path: file.to_owned(),
            });
        }

        let root = std::fs::canonicalize(&skill.root).map_err(|error| SkillError::Unreadable {
            path: name.to_owned(),
            reason: error.to_string(),
        })?;
        let target =
            std::fs::canonicalize(root.join(relative)).map_err(|error| match error.kind() {
                std::io::ErrorKind::NotFound => SkillError::Missing {
                    path: file.to_owned(),
                },
                _ => SkillError::Unreadable {
                    path: file.to_owned(),
                    reason: error.to_string(),
                },
            })?;
        if !target.starts_with(&root) {
            return Err(SkillError::Outside {
                path: file.to_owned(),
            });
        }

        std::fs::read_to_string(&target).map_err(|error| SkillError::Unreadable {
            path: file.to_owned(),
            reason: if error.kind() == std::io::ErrorKind::InvalidData {
                "it is not text".to_owned()
            } else {
                error.to_string()
            },
        })
    }

    fn find(&self, name: &str) -> Result<&Skill, SkillError> {
        self.get(name).ok_or_else(|| SkillError::Unknown {
            name: name.to_owned(),
            available: self.skills.iter().map(|skill| skill.name.clone()).collect(),
        })
    }
}

/// A skill found somewhere that is not a profile: what an import would copy.
pub fn scan(directory: &Path) -> Vec<Skill> {
    let index = SkillIndex::load(directory);
    index.skills
}

/// The skills in `source` that are not installed yet, so the UI can offer them.
pub fn importable(source: &Path, installed: &SkillIndex) -> Vec<Skill> {
    scan(source)
        .into_iter()
        .filter(|skill| installed.get(&skill.name).is_none())
        .collect()
}

/// Copies a skill directory into `destination` (a profile's `skills/`). The copy
/// is what makes a profile self-contained, so an import never links.
pub fn import(destination: &Path, source: &Path) -> Result<Skill, SkillError> {
    let fallback_name = source
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_owned();
    let skill = read_skill(source, &fallback_name).ok_or_else(|| SkillError::NotASkill {
        path: source.display().to_string(),
    })?;

    let target = destination.join(&skill.name);
    if target.exists() {
        return Err(SkillError::AlreadyInstalled {
            name: skill.name.clone(),
        });
    }

    copy_directory(source, &target)?;
    Ok(Skill {
        root: target,
        ..skill
    })
}

fn read_skill(directory: &Path, fallback_name: &str) -> Option<Skill> {
    let markdown = directory.join(SKILL_FILE);
    let contents = match std::fs::read_to_string(&markdown) {
        Ok(contents) => contents,
        Err(error) => {
            log::warn!("ignoring {}: {error}", markdown.display());
            return None;
        }
    };
    let metadata = parse_frontmatter(&contents, fallback_name).or_else(|| {
        log::warn!(
            "ignoring {}: the frontmatter has no description",
            markdown.display()
        );
        None
    })?;
    Some(Skill {
        name: metadata.name,
        description: metadata.description,
        root: directory.to_path_buf(),
    })
}

struct Frontmatter {
    name: String,
    description: String,
}

/// Flat `key: value` pairs between the first two `---` lines. Anything richer —
/// nested keys, folded values — is out of scope; the body is where prose goes.
fn parse_frontmatter(contents: &str, fallback_name: &str) -> Option<Frontmatter> {
    let mut lines = contents.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }

    let mut name = None;
    let mut description = None;
    for line in lines {
        if line.trim() == "---" {
            break;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = unquote(value.trim());
        match key.trim() {
            "name" if !value.is_empty() => name = Some(value.to_owned()),
            "description" if !value.is_empty() => description = Some(value.to_owned()),
            _ => {}
        }
    }

    let description = description?;
    Some(Frontmatter {
        name: name.unwrap_or_else(|| fallback_name.to_owned()),
        description: truncate(&description, MAX_DESCRIPTION_CHARS),
    })
}

fn unquote(value: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(rest) = value.strip_prefix(quote)
            && let Some(value) = rest.strip_suffix(quote)
        {
            return value;
        }
    }
    value
}

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let mut shortened = text.chars().take(limit).collect::<String>();
    shortened.push('…');
    shortened
}

fn copy_directory(from: &Path, to: &Path) -> Result<(), SkillError> {
    let unreadable = |path: &Path, error: std::io::Error| SkillError::Unreadable {
        path: path.display().to_string(),
        reason: error.to_string(),
    };
    std::fs::create_dir_all(to).map_err(|error| unreadable(to, error))?;

    let entries = std::fs::read_dir(from).map_err(|error| unreadable(from, error))?;
    for entry in entries {
        let entry = entry.map_err(|error| unreadable(from, error))?;
        let path = entry.path();
        let target = to.join(entry.file_name());
        let file_type = entry
            .file_type()
            .map_err(|error| unreadable(&path, error))?;
        if file_type.is_dir() {
            copy_directory(&path, &target)?;
        } else if file_type.is_file() {
            std::fs::copy(&path, &target).map_err(|error| unreadable(&path, error))?;
        }
        // A symlink is skipped: an import must not drag something in from outside
        // the skill it is copying.
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(skills: &Path, name: &str, description: &str, body: &str) -> PathBuf {
        let directory = skills.join(name);
        std::fs::create_dir_all(&directory).expect("create the skill directory");
        std::fs::write(
            directory.join(SKILL_FILE),
            format!("---\nname: {name}\ndescription: {description}\n---\n\n{body}\n"),
        )
        .expect("write SKILL.md");
        directory
    }

    #[test]
    fn loads_skills_sorted_with_their_descriptions() {
        let directory = tempfile::tempdir().expect("temp dir");
        let skills = directory.path().join("skills");
        write_skill(&skills, "second", "does the second thing", "# Second");
        write_skill(&skills, "first", "does the first thing", "# First");

        let index = SkillIndex::load(&skills);

        let names: Vec<&str> = index
            .skills()
            .iter()
            .map(|skill| skill.name.as_str())
            .collect();
        assert_eq!(names, ["first", "second"], "the index is ordered by name");
        assert_eq!(index.skills()[0].description, "does the first thing");
    }

    #[test]
    fn a_skill_built_in_memory_points_at_its_markdown() {
        let skill = Skill::new("card", "makes a card", "/skills/card");

        assert_eq!(skill.name, "card");
        assert_eq!(skill.description, "makes a card");
        assert_eq!(skill.root(), Path::new("/skills/card"));
        assert_eq!(skill.markdown_path(), Path::new("/skills/card/SKILL.md"));
    }

    #[test]
    fn a_missing_skills_directory_is_an_empty_index() {
        let directory = tempfile::tempdir().expect("temp dir");

        let index = SkillIndex::load(&directory.path().join("skills"));

        assert!(index.is_empty());
        assert_eq!(index.skills(), []);
    }

    #[test]
    fn a_skill_without_a_description_is_ignored() {
        let directory = tempfile::tempdir().expect("temp dir");
        let skills = directory.path().join("skills");
        let broken = skills.join("nameless");
        std::fs::create_dir_all(&broken).expect("create");
        std::fs::write(broken.join(SKILL_FILE), "---\nname: nameless\n---\nbody\n").expect("write");
        write_skill(&skills, "fine", "a real skill", "body");

        let index = SkillIndex::load(&skills);

        assert_eq!(index.skills().len(), 1);
        assert_eq!(index.skills()[0].name, "fine");
    }

    #[test]
    fn the_name_falls_back_to_the_directory_and_quotes_are_stripped() {
        let directory = tempfile::tempdir().expect("temp dir");
        let skills = directory.path().join("skills");
        let skill = skills.join("from-directory");
        std::fs::create_dir_all(&skill).expect("create");
        std::fs::write(
            skill.join(SKILL_FILE),
            "---\ndescription: \"a quoted description\"\n---\nbody\n",
        )
        .expect("write");

        let index = SkillIndex::load(&skills);

        assert_eq!(index.skills()[0].name, "from-directory");
        assert_eq!(index.skills()[0].description, "a quoted description");
    }

    #[test]
    fn a_long_description_is_cut_for_the_index() {
        let directory = tempfile::tempdir().expect("temp dir");
        let skills = directory.path().join("skills");
        write_skill(&skills, "wordy", &"长".repeat(300), "body");

        let index = SkillIndex::load(&skills);

        let description = &index.skills()[0].description;
        assert_eq!(description.chars().count(), MAX_DESCRIPTION_CHARS + 1);
        assert!(description.ends_with('…'));
    }

    #[test]
    fn reads_a_body_and_a_bundled_file() {
        let directory = tempfile::tempdir().expect("temp dir");
        let skills = directory.path().join("skills");
        let skill = write_skill(&skills, "card", "makes a card", "# Card\n\nStep one.");
        std::fs::create_dir_all(skill.join("references")).expect("create references");
        std::fs::write(skill.join("references/template.md"), "template\n").expect("write");
        let index = SkillIndex::load(&skills);

        let body = index.read("card").expect("the body");
        assert!(body.contains("Step one."), "{body}");
        assert!(
            body.contains("description: makes a card"),
            "the frontmatter is kept"
        );

        let template = index
            .read_file("card", "references/template.md")
            .expect("the bundled file");
        assert_eq!(template, "template\n");
    }

    #[test]
    fn refuses_paths_that_leave_the_skill() {
        let directory = tempfile::tempdir().expect("temp dir");
        let skills = directory.path().join("skills");
        let skill = write_skill(&skills, "card", "makes a card", "body");
        std::fs::write(directory.path().join("secret.txt"), "secret").expect("write");
        let index = SkillIndex::load(&skills);

        for path in [
            "../secret.txt",
            "/etc/passwd",
            "references/../../secret.txt",
        ] {
            let error = index.read_file("card", path).expect_err(path);
            assert!(
                error.to_string().contains("outside the skill"),
                "{path}: {error}"
            );
        }

        let error = index.read_file("card", "nope.md").expect_err("missing");
        assert!(error.to_string().contains("does not exist"), "{error}");
        assert!(skill.exists());
    }

    #[cfg(unix)]
    #[test]
    fn refuses_a_symlink_that_leaves_the_skill() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().expect("temp dir");
        let skills = directory.path().join("skills");
        let skill = write_skill(&skills, "card", "makes a card", "body");
        std::fs::write(directory.path().join("secret.txt"), "secret").expect("write");
        symlink(directory.path().join("secret.txt"), skill.join("link.md")).expect("symlink");
        let index = SkillIndex::load(&skills);

        let error = index.read_file("card", "link.md").expect_err("escape");

        assert!(error.to_string().contains("outside the skill"), "{error}");
    }

    #[test]
    fn an_unknown_skill_lists_what_is_installed() {
        let directory = tempfile::tempdir().expect("temp dir");
        let skills = directory.path().join("skills");
        write_skill(&skills, "card", "makes a card", "body");
        let index = SkillIndex::load(&skills);

        let error = index.read("nope").expect_err("unknown");

        assert!(error.to_string().contains("installed: card"), "{error}");
    }

    #[test]
    fn import_copies_a_skill_and_refuses_to_overwrite() {
        let directory = tempfile::tempdir().expect("temp dir");
        let source = directory.path().join("agents/skills");
        let given = write_skill(&source, "card", "makes a card", "body");
        std::fs::create_dir_all(given.join("scripts")).expect("create scripts");
        std::fs::write(given.join("scripts/render.sh"), "#!/bin/sh\n").expect("write");
        let destination = directory.path().join("profile/skills");

        let imported = import(&destination, &given).expect("import");

        assert_eq!(imported.name, "card");
        assert!(destination.join("card/SKILL.md").is_file());
        assert_eq!(
            std::fs::read_to_string(destination.join("card/scripts/render.sh")).expect("copied"),
            "#!/bin/sh\n"
        );
        assert_eq!(
            SkillIndex::load(&destination).skills().len(),
            1,
            "the copy is a skill the profile can read"
        );

        let error = import(&destination, &given).expect_err("already installed");
        assert!(error.to_string().contains("already installed"), "{error}");
    }

    #[test]
    fn import_refuses_something_that_is_not_a_skill() {
        let directory = tempfile::tempdir().expect("temp dir");
        let source = directory.path().join("agents/skills/empty");
        std::fs::create_dir_all(&source).expect("create");
        let destination = directory.path().join("profile/skills");

        let error = import(&destination, &source).expect_err("not a skill");

        assert!(error.to_string().contains("is not a skill"), "{error}");
    }

    #[test]
    fn importable_lists_only_what_is_not_installed() {
        let directory = tempfile::tempdir().expect("temp dir");
        let source = directory.path().join("agents/skills");
        write_skill(&source, "one", "the first", "body");
        write_skill(&source, "two", "the second", "body");
        let destination = directory.path().join("profile/skills");
        write_skill(&destination, "one", "the first", "body");
        let installed = SkillIndex::load(&destination);

        let candidates = importable(&source, &installed);

        let names: Vec<&str> = candidates.iter().map(|skill| skill.name.as_str()).collect();
        assert_eq!(names, ["two"]);
    }
}
