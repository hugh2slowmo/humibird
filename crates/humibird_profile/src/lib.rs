//! Profile home resolution, settings (`config.toml`) and secrets (`.env`).
//!
//! A profile is a directory. Only the
//! pieces needed to start a provider live here for now.

mod config;
mod mcp;

pub use config::{Config, Effort, ProviderConfig, ProviderOverride, Theme, ThinkingConfig};
pub use mcp::{McpConfig, McpServer, McpTransport};

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Context as _;

pub const DEFAULT_PROFILE: &str = "default";

#[derive(Clone, Debug)]
pub struct Profile {
    root: PathBuf,
}

impl Profile {
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The profile a fresh install uses.
    pub fn default_location() -> Option<Self> {
        Some(Self::at(profiles_root()?.join(DEFAULT_PROFILE)))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn env_path(&self) -> PathBuf {
        self.root.join(".env")
    }

    /// The profile's session database.
    pub fn database_path(&self) -> PathBuf {
        self.root.join("sessions").join("sessions.sqlite3")
    }

    /// The profile's optional persona / standing instructions.
    pub fn soul_path(&self) -> PathBuf {
        self.root.join("SOUL.md")
    }

    /// The profile's skills, one directory each. Missing is normal: a
    /// profile that has never installed a skill has no directory.
    pub fn skills_dir(&self) -> PathBuf {
        self.root.join("skills")
    }

    /// The profile's default workspace, created on first use. It is the directory
    /// a thread starts in when the user has not chosen one, not a place for the
    /// profile's own files.
    pub fn workspace_dir(&self) -> anyhow::Result<PathBuf> {
        let path = self.root.join("workspace");
        std::fs::create_dir_all(&path)
            .with_context(|| format!("failed to create {}", path.display()))?;
        Ok(path)
    }

    /// Whether `path` may be a workspace. The profile holds `.env` and the session
    /// database, so neither it nor anything containing it can be one, and the
    /// filesystem root or `$HOME` on their own would make confinement meaningless.
    pub fn accept_workspace(&self, path: &Path) -> anyhow::Result<()> {
        let resolved = std::fs::canonicalize(path).with_context(|| {
            format!(
                "{} cannot be a workspace: it is not a directory that exists",
                path.display()
            )
        })?;
        // The profile root may not exist yet; an unresolved path still catches the
        // cases that matter, and a root that is not there has no children to
        // protect.
        let root = std::fs::canonicalize(&self.root).unwrap_or_else(|_| self.root.clone());

        anyhow::ensure!(
            resolved.parent().is_some(),
            "the filesystem root cannot be a workspace"
        );
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            let home = std::fs::canonicalize(&home).unwrap_or(home);
            anyhow::ensure!(
                resolved != home,
                "$HOME itself cannot be a workspace; choose a directory inside it"
            );
        }
        anyhow::ensure!(
            resolved != root,
            "{} is the profile directory, which holds secrets and session history",
            resolved.display()
        );
        // The profile's own `workspace/` is the default workspace, so it
        // is the one directory under the profile root that is allowed. It has no
        // route back to `.env`: `..` and absolute paths are refused by the
        // workspace, and a symlink out of it is refused too.
        let default_workspace = root.join("workspace");
        anyhow::ensure!(
            !resolved.starts_with(&root) || resolved.starts_with(&default_workspace),
            "{} is inside the profile directory, which holds secrets and session history; \
             only its `workspace/` directory is a workspace",
            resolved.display()
        );
        anyhow::ensure!(
            !root.starts_with(&resolved),
            "{} contains the profile directory, so it cannot be a workspace",
            resolved.display()
        );
        Ok(())
    }

    /// A secret from the process environment, falling back to the profile's
    /// `.env`. Process env wins so a shell override never requires editing the
    /// profile.
    pub fn secret(&self, name: &str) -> Option<String> {
        self.secret_with(name, |name| std::env::var(name).ok())
    }

    /// The key stored for this profile wins over one exported in the shell: the
    /// file is what the person configured for this agent, and a variable another
    /// tool exported is not something to override it with. The environment is the
    /// fallback, for a launch that has no file yet.
    pub fn secret_with(
        &self,
        name: &str,
        from_process_env: impl Fn(&str) -> Option<String>,
    ) -> Option<String> {
        self.dotenv()
            .remove(name)
            .filter(|value| !value.trim().is_empty())
            .or_else(|| from_process_env(name).filter(|value| !value.trim().is_empty()))
    }

    pub fn dotenv(&self) -> HashMap<String, String> {
        match std::fs::read_to_string(self.env_path()) {
            Ok(contents) => parse_dotenv(&contents),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(error) => {
                log::warn!("failed to read {}: {error}", self.env_path().display());
                HashMap::new()
            }
        }
    }

    /// Writes a secret to the profile's `.env`, preserving comments and other
    /// entries. Creates the profile directory if needed.
    pub fn set_secret(&self, name: &str, value: &str) -> anyhow::Result<()> {
        validate_secret(name, value)?;
        let contents = self.read_env_text()?;
        let updated = upsert_dotenv(&contents, name, Some(value));
        self.write_env_text(&updated)
    }

    /// Removes a secret from the profile's `.env`.
    pub fn remove_secret(&self, name: &str) -> anyhow::Result<()> {
        validate_secret_name(name)?;
        let contents = self.read_env_text()?;
        let updated = upsert_dotenv(&contents, name, None);
        self.write_env_text(&updated)
    }

    fn read_env_text(&self) -> anyhow::Result<String> {
        match std::fs::read_to_string(self.env_path()) {
            Ok(contents) => Ok(contents),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(error) => Err(error.into()),
        }
    }

    fn write_env_text(&self, contents: &str) -> anyhow::Result<()> {
        std::fs::create_dir_all(&self.root)?;
        let path = self.env_path();
        let temporary = self.root.join(".env.tmp");
        std::fs::write(&temporary, contents)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))?;
        }

        std::fs::rename(&temporary, &path)?;
        Ok(())
    }
}

/// Every profile lives under `~/.humibird/profiles`.
///
/// A dot directory in the home directory, not `~/Library/Application Support`:
/// a profile is meant to be opened, copied and deleted by hand, and the other
/// agents a person runs already keep theirs in `~/.name`.
pub fn profiles_root() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".humibird").join("profiles"))
}

/// Where skills written for other agents live, `~/.agents/skills`. It is only
/// ever read: an import copies out of it.
pub fn agents_skills_dir(home: &Path) -> PathBuf {
    home.join(".agents").join("skills")
}

fn parse_dotenv(contents: &str) -> HashMap<String, String> {
    let mut values = HashMap::new();

    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);

        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }

        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
            .or_else(|| {
                value
                    .strip_prefix('\'')
                    .and_then(|value| value.strip_suffix('\''))
            })
            .unwrap_or(value);

        values.insert(key.to_owned(), value.to_owned());
    }

    values
}

fn validate_secret_name(name: &str) -> anyhow::Result<()> {
    let valid = !name.is_empty()
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_');

    anyhow::ensure!(
        valid,
        "{name:?} is not a valid .env key: use letters, digits and underscores"
    );
    Ok(())
}

fn validate_secret(name: &str, value: &str) -> anyhow::Result<()> {
    validate_secret_name(name)?;
    anyhow::ensure!(
        !value.is_empty(),
        "{name} cannot be set to an empty value; remove it instead"
    );
    anyhow::ensure!(
        !value
            .chars()
            .any(|character| character.is_whitespace() || character == '"' || character == '\''),
        "{name} cannot contain whitespace or quotes"
    );
    Ok(())
}

/// Replaces `name`'s line (dropping duplicates) or appends it, leaving every
/// other line untouched. Passing `None` removes the entry.
fn upsert_dotenv(contents: &str, name: &str, value: Option<&str>) -> String {
    let mut output = String::new();
    let mut seen = false;

    for line in contents.lines() {
        let trimmed = line.trim();
        let key = trimmed
            .strip_prefix("export ")
            .unwrap_or(trimmed)
            .split_once('=')
            .map(|(key, _)| key.trim());

        if key == Some(name) {
            if !seen {
                seen = true;
                if let Some(value) = value {
                    output.push_str(&format!("{name}={value}\n"));
                }
            }
        } else {
            output.push_str(line);
            output.push('\n');
        }
    }

    if !seen && let Some(value) = value {
        output.push_str(&format!("{name}={value}\n"));
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile_with_env(contents: &str) -> (tempfile::TempDir, Profile) {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join(".env"), contents).expect("write .env");
        let profile = Profile::at(dir.path());
        (dir, profile)
    }

    #[test]
    fn the_default_workspace_is_created_inside_the_profile() {
        let dir = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(dir.path().join("profile"));

        let workspace = profile.workspace_dir().expect("workspace");

        assert_eq!(workspace, profile.root().join("workspace"));
        assert!(workspace.is_dir(), "it is created on first use");
    }

    #[test]
    fn a_profile_that_never_installed_a_skill_has_no_skills_directory() {
        let dir = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(dir.path().join("profile"));

        let skills = profile.skills_dir();

        assert_eq!(skills, profile.root().join("skills"));
        assert!(!skills.exists(), "it is not created until a skill arrives");
    }

    #[test]
    fn the_agents_skills_directory_hangs_off_home() {
        assert_eq!(
            agents_skills_dir(Path::new("/home/me")),
            Path::new("/home/me/.agents/skills")
        );
    }

    #[test]
    fn a_workspace_may_not_expose_the_profile() {
        let dir = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(dir.path().join("profile"));
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).expect("create the project");
        std::fs::create_dir_all(profile.root().join("memories")).expect("create memories");

        profile
            .accept_workspace(&project)
            .expect("a project next to the profile is fine");
        let workspace = profile.workspace_dir().expect("default workspace");
        profile
            .accept_workspace(&workspace)
            .expect("the profile's own workspace is fine");
        std::fs::create_dir_all(workspace.join("checkout")).expect("create a nested project");
        profile
            .accept_workspace(&workspace.join("checkout"))
            .expect("a project inside the default workspace is fine");

        let cases = [
            (PathBuf::from("/"), "filesystem root"),
            (profile.root().to_path_buf(), "is the profile directory"),
            (
                profile.root().join("memories"),
                "is inside the profile directory",
            ),
            (dir.path().to_path_buf(), "contains the profile directory"),
            (dir.path().join("missing"), "cannot be a workspace"),
        ];

        for (path, expected) in cases {
            let error = profile
                .accept_workspace(&path)
                .expect_err(&path.display().to_string());
            assert!(
                error.to_string().contains(expected),
                "{} should be refused with {expected:?}, got {error}",
                path.display()
            );
        }
    }

    #[test]
    fn the_home_directory_itself_is_not_a_workspace() {
        let Some(home) = std::env::var_os("HOME") else {
            return;
        };
        let dir = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(dir.path().join("profile"));

        let error = profile
            .accept_workspace(Path::new(&home))
            .expect_err("home itself");

        assert!(error.to_string().contains("$HOME itself"), "{error}");
    }

    #[test]
    fn parses_plain_quoted_and_exported_values() {
        let (_dir, profile) = profile_with_env(
            "# comment\n\
             DEEPSEEK_API_KEY=sk-plain\n\
             export OTHER_KEY=\"quoted value\"\n\
             SINGLE='single'\n\
             EMPTY=\n\
             not a pair\n",
        );

        let values = profile.dotenv();
        assert_eq!(values.get("DEEPSEEK_API_KEY"), Some(&"sk-plain".to_owned()));
        assert_eq!(values.get("OTHER_KEY"), Some(&"quoted value".to_owned()));
        assert_eq!(values.get("SINGLE"), Some(&"single".to_owned()));
        assert_eq!(values.get("EMPTY"), Some(&"".to_owned()));
        assert_eq!(values.len(), 4);
    }

    #[test]
    fn the_dotenv_wins_over_the_process_env() {
        let (_dir, profile) = profile_with_env("DEEPSEEK_API_KEY=sk-from-file\n");

        let secret = profile.secret_with("DEEPSEEK_API_KEY", |_| Some("sk-from-env".to_owned()));

        assert_eq!(secret.as_deref(), Some("sk-from-file"));
    }

    #[test]
    fn a_blank_dotenv_entry_falls_back_to_the_process_env() {
        let (_dir, profile) = profile_with_env("DEEPSEEK_API_KEY=   \n");

        let secret = profile.secret_with("DEEPSEEK_API_KEY", |_| Some("sk-from-env".to_owned()));

        assert_eq!(secret.as_deref(), Some("sk-from-env"));
    }

    #[test]
    fn writes_a_secret_creating_the_profile() {
        let dir = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(dir.path().join("profiles/default"));

        profile
            .set_secret("DEEPSEEK_API_KEY", "sk-new")
            .expect("write secret");

        assert_eq!(
            profile.dotenv().get("DEEPSEEK_API_KEY"),
            Some(&"sk-new".to_owned())
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(profile.env_path())
                .expect("env metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, ".env should not be world readable");
        }
    }

    #[test]
    fn replaces_a_secret_and_keeps_other_lines() {
        let (_dir, profile) = profile_with_env("# keep me\nDEEPSEEK_API_KEY=sk-old\nOTHER=1\n");

        profile
            .set_secret("DEEPSEEK_API_KEY", "sk-new")
            .expect("write secret");

        let text = std::fs::read_to_string(profile.env_path()).expect("read .env");
        assert!(text.contains("# keep me"));
        assert!(text.contains("OTHER=1"));
        assert!(text.contains("DEEPSEEK_API_KEY=sk-new"));
        assert!(!text.contains("sk-old"));
    }

    #[test]
    fn removes_a_secret() {
        let (_dir, profile) = profile_with_env("DEEPSEEK_API_KEY=sk\nOTHER=1\n");

        profile.remove_secret("DEEPSEEK_API_KEY").expect("remove");

        let env = profile.dotenv();
        assert!(!env.contains_key("DEEPSEEK_API_KEY"));
        assert_eq!(env.get("OTHER"), Some(&"1".to_owned()));
    }

    #[test]
    fn rejects_invalid_names_and_values() {
        let (_dir, profile) = profile_with_env("");

        assert!(profile.set_secret("BAD KEY", "value").is_err());
        assert!(profile.set_secret("KEY", "").is_err());
        assert!(profile.set_secret("KEY", "has space").is_err());
        assert!(profile.set_secret("KEY", "quote\"d").is_err());
    }

    #[test]
    fn missing_values_are_none() {
        let (_dir, profile) = profile_with_env("");

        assert_eq!(profile.secret_with("DEEPSEEK_API_KEY", |_| None), None);
    }

    #[test]
    fn missing_env_file_is_not_an_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(dir.path());

        assert!(profile.dotenv().is_empty());
    }
}
