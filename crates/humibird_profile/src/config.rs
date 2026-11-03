//! The profile's `config.toml`.
//!
//! Only what the app can act on today lives here: the theme and any provider
//! overrides. Missing keys fall back to the app's defaults, and unknown keys are
//! ignored so a file written by a newer version still loads; an invalid value is
//! an error, because a typo should be visible rather than silently ignored.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use crate::Profile;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    Light,
    Dark,
}

/// How hard the model should think. Each provider spells the levels its own way;
/// this is the app's scale, and it is what the composer's effort control sets.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Low,
    #[default]
    Medium,
    High,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct ThinkingConfig {
    /// Whether the composer asks for thinking. Off is the request saying nothing
    /// where the wire has no off switch, and saying "disabled" where it does.
    pub enabled: bool,
    pub effort: Effort,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct ProviderConfig {
    /// Which provider the profile talks to: a preset id such as `deepseek`.
    pub id: String,
    /// Overrides the adapter's default endpoint. Read as the provider in use's own
    /// overrides when no `[providers.<id>]` record exists; new writes go to the
    /// record.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Overrides the adapter's default model, and the model this thread talks to.
    /// Required by a provider that lists no models of its own.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The environment variable the key is read from, when it is not the one the
    /// preset names. This is how an endpoint without a key of its own gets one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_env: Option<String>,
    /// The context window to assume for this provider's models, in tokens. Only
    /// needed when neither the preset nor the provider's model listing says; a
    /// wrong value here beats no value, because it is what the meter and the
    /// compaction threshold measure against.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            id: "deepseek".to_owned(),
            base_url: None,
            model: None,
            key_env: None,
            context_window: None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub theme: Theme,
    pub provider: ProviderConfig,
    /// Endpoint and model overrides per provider id, written as
    /// `[providers.<id>]`. They belong to the provider, not to the profile's current
    /// choice, which is what lets `custom` be filled in without switching to it.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub providers: BTreeMap<String, ProviderOverride>,
    pub thinking: ThinkingConfig,
}

/// What a profile says about one provider: where it is, what key it wants, how
/// large its window is. The same fields the `[provider]` block holds for the one in
/// use, kept apart so they survive a switch.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct ProviderOverride {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_env: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
    /// A model the provider cannot name itself, such as a custom endpoint that
    /// answers no `GET /models`. The picker offers it like a catalogued name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl Config {
    /// The overrides that apply to `id`: its own record, or — for a file written
    /// before records existed — the `[provider]` block, which described the provider
    /// in use.
    pub fn overrides(&self, id: &str) -> ProviderOverride {
        if let Some(overrides) = self.providers.get(id) {
            return overrides.clone();
        }
        if self.provider.id == id {
            return ProviderOverride {
                base_url: self.provider.base_url.clone(),
                key_env: self.provider.key_env.clone(),
                context_window: self.provider.context_window,
                model: None,
            };
        }
        ProviderOverride::default()
    }

    /// Keeps the `[provider]` block's overrides with the provider they were written
    /// for, so a switch does not hand them to the new one. Called before the block's
    /// `id` changes; a record that already exists wins field by field.
    pub fn keep_overrides_with_their_provider(&mut self) {
        let provider = &mut self.provider;
        if provider.base_url.is_none()
            && provider.key_env.is_none()
            && provider.context_window.is_none()
        {
            return;
        }

        let record = self.providers.entry(provider.id.clone()).or_default();
        if record.base_url.is_none() {
            record.base_url = provider.base_url.take();
        }
        if record.key_env.is_none() {
            record.key_env = provider.key_env.take();
        }
        if record.context_window.is_none() {
            record.context_window = provider.context_window.take();
        }
    }
}

impl Profile {
    pub fn config_path(&self) -> PathBuf {
        self.root().join("config.toml")
    }

    /// Reads `config.toml`. A missing file is the default configuration.
    pub fn config(&self) -> Result<Config> {
        let path = self.config_path();
        match std::fs::read_to_string(&path) {
            Ok(contents) => toml::from_str(&contents)
                .with_context(|| format!("failed to parse {}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
        }
    }

    /// Replaces `config.toml`, creating the profile directory if needed.
    pub fn save_config(&self, config: &Config) -> Result<()> {
        let contents =
            toml::to_string_pretty(config).context("failed to serialize the configuration")?;
        std::fs::create_dir_all(self.root())?;

        let path = self.config_path();
        let temporary = self.root().join("config.toml.tmp");
        std::fs::write(&temporary, contents)?;
        std::fs::rename(&temporary, path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile_with_config(contents: &str) -> (tempfile::TempDir, Profile) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path());
        std::fs::write(profile.config_path(), contents).expect("write config");
        (directory, profile)
    }

    #[test]
    fn a_missing_file_is_the_default_configuration() {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path());

        let config = profile.config().expect("the defaults load");

        assert_eq!(config, Config::default());
        assert_eq!(config.theme, Theme::Light);
        assert_eq!(config.provider.id, "deepseek");
        assert_eq!(config.provider.base_url, None);
        assert_eq!(config.provider.model, None);
        assert_eq!(config.provider.key_env, None);
    }

    #[test]
    fn a_file_without_a_provider_id_keeps_the_default_one() {
        let (_directory, profile) =
            profile_with_config("[provider]\nmodel = \"deepseek-reasoner\"\n");

        let config = profile.config().expect("the file parses");

        assert_eq!(config.provider.id, "deepseek");
    }

    #[test]
    fn reads_a_provider_choice_and_a_key_variable() {
        let (_directory, profile) = profile_with_config(
            "[provider]\n\
             id = \"custom\"\n\
             base_url = \"https://gateway.example/v1\"\n\
             model = \"some-model\"\n\
             key_env = \"GATEWAY_KEY\"\n",
        );

        let config = profile.config().expect("the file parses");

        assert_eq!(config.provider.id, "custom");
        assert_eq!(
            config.provider.base_url.as_deref(),
            Some("https://gateway.example/v1")
        );
        assert_eq!(config.provider.model.as_deref(), Some("some-model"));
        assert_eq!(config.provider.key_env.as_deref(), Some("GATEWAY_KEY"));
    }

    #[test]
    fn a_file_without_a_thinking_switch_leaves_it_off_at_medium() {
        let (_directory, profile) = profile_with_config("");

        let config = profile.config().expect("the defaults load");

        assert!(!config.thinking.enabled);
        assert_eq!(config.thinking.effort, Effort::Medium);
    }

    #[test]
    fn reads_the_thinking_switch() {
        let (_directory, profile) =
            profile_with_config("[thinking]\nenabled = true\neffort = \"high\"\n");

        let config = profile.config().expect("the file parses");

        assert!(config.thinking.enabled);
        assert_eq!(config.thinking.effort, Effort::High);
    }

    #[test]
    fn reads_a_context_window_the_profile_supplies() {
        let (_directory, profile) = profile_with_config(
            "[provider]\n\
             id = \"custom\"\n\
             model = \"local\"\n\
             context_window = 131072\n",
        );

        let config = profile.config().expect("the file parses");

        assert_eq!(config.provider.context_window, Some(131_072));
    }

    #[test]
    fn reads_the_theme_and_provider_overrides() {
        let (_directory, profile) = profile_with_config(
            "theme = \"dark\"\n\
             \n\
             [provider]\n\
             base_url = \"https://example.test/v1\"\n\
             model = \"deepseek-reasoner\"\n",
        );

        let config = profile.config().expect("the file parses");

        assert_eq!(config.theme, Theme::Dark);
        assert_eq!(
            config.provider.base_url.as_deref(),
            Some("https://example.test/v1")
        );
        assert_eq!(config.provider.model.as_deref(), Some("deepseek-reasoner"));
    }

    #[test]
    fn reads_overrides_for_a_provider_that_is_not_in_use() {
        let (_directory, profile) = profile_with_config(
            "[provider]\n\
             id = \"deepseek\"\n\
             model = \"deepseek-flash\"\n\
             \n\
             [providers.custom]\n\
             base_url = \"http://localhost:8000/v1\"\n\
             model = \"local\"\n\
             context_window = 32000\n",
        );

        let config = profile.config().expect("the file parses");
        let custom = config.overrides("custom");

        assert_eq!(custom.base_url.as_deref(), Some("http://localhost:8000/v1"));
        assert_eq!(custom.model.as_deref(), Some("local"));
        assert_eq!(custom.context_window, Some(32_000));
        assert_eq!(
            config.overrides("deepseek").base_url,
            None,
            "another provider's record is not the provider in use's"
        );
    }

    #[test]
    fn a_provider_record_wins_over_the_provider_block() {
        let (_directory, profile) = profile_with_config(
            "[provider]\n\
             id = \"custom\"\n\
             base_url = \"https://old.example/v1\"\n\
             \n\
             [providers.custom]\n\
             base_url = \"https://new.example/v1\"\n",
        );

        let config = profile.config().expect("the file parses");

        assert_eq!(
            config.overrides("custom").base_url.as_deref(),
            Some("https://new.example/v1"),
            "the record is the provider's own answer"
        );
    }

    #[test]
    fn a_switch_keeps_the_old_providers_overrides_with_it() {
        let mut config = Config::default();
        config.provider.id = "custom".to_owned();
        config.provider.base_url = Some("http://localhost:8000/v1".to_owned());
        config.provider.context_window = Some(64_000);

        config.keep_overrides_with_their_provider();
        config.provider.id = "deepseek".to_owned();

        assert_eq!(config.provider.base_url, None);
        let custom = config.overrides("custom");
        assert_eq!(custom.base_url.as_deref(), Some("http://localhost:8000/v1"));
        assert_eq!(custom.context_window, Some(64_000));
        assert_eq!(
            config.overrides("deepseek").base_url,
            None,
            "the new provider does not inherit them"
        );
    }

    #[test]
    fn saving_round_trips_and_creates_the_profile() {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profiles/default"));
        let config = Config {
            theme: Theme::Dark,
            provider: ProviderConfig {
                id: "deepseek".to_owned(),
                base_url: None,
                model: Some("deepseek-reasoner".to_owned()),
                key_env: None,
                context_window: Some(200_000),
            },
            ..Config::default()
        };

        profile.save_config(&config).expect("the config is written");

        assert_eq!(profile.config().expect("it reads back"), config);
        let contents = std::fs::read_to_string(profile.config_path()).expect("read config");
        assert!(contents.contains("theme = \"dark\""), "{contents}");
        assert!(
            contents.contains("model = \"deepseek-reasoner\""),
            "{contents}"
        );
        assert!(!contents.contains("base_url"), "{contents}");
        assert!(!contents.contains("key_env"), "{contents}");
    }

    #[test]
    fn an_invalid_value_names_the_file() {
        let (_directory, profile) = profile_with_config("theme = \"purple\"\n");

        let error = profile.config().expect_err("purple is not a theme");

        assert!(format!("{error:#}").contains("config.toml"), "{error:#}");
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let (_directory, profile) = profile_with_config(
            "theme = \"dark\"\n\
             \n\
             [provider]\n\
             model = \"deepseek-flash\"\n\
             something_new = true\n",
        );

        let config = profile
            .config()
            .expect("unknown keys are forward compatible");

        assert_eq!(config.theme, Theme::Dark);
        assert_eq!(config.provider.model.as_deref(), Some("deepseek-flash"));
    }
}
