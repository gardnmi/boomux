use std::collections::{BTreeMap, HashSet};
use std::env;
use std::error::Error;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

const DEFAULT_PROJECT_SEARCH_DEPTH: usize = 3;
const MAX_PROJECT_SEARCH_DEPTH: usize = 10;
const MAX_RECIPE_TERMINALS: usize = 12;
const MAX_RECIPE_NAME_LENGTH: usize = 64;

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawConfig {
    terminal: Option<String>,
    projects: Option<RawProjectsConfig>,
    recipes: Option<BTreeMap<String, RawRecipeConfig>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawProjectsConfig {
    roots: Option<Vec<String>>,
    max_depth: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawRecipeConfig {
    label: Option<String>,
    terminals: Vec<RawRecipeTerminal>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRecipeTerminal {
    name: String,
    command: Option<String>,
}

#[derive(Debug)]
pub(crate) struct Config {
    pub(crate) terminal: Option<String>,
    pub(crate) projects: ProjectsConfig,
    pub(crate) recipes: Vec<RecipeConfig>,
    pub(crate) path: Option<PathBuf>,
}

#[derive(Debug)]
pub(crate) struct ProjectsConfig {
    pub(crate) roots: Vec<PathBuf>,
    pub(crate) max_depth: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RecipeConfig {
    pub(crate) id: String,
    pub(crate) label: String,
    pub(crate) terminals: Vec<RecipeTerminalConfig>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RecipeTerminalConfig {
    pub(crate) name: String,
    pub(crate) command: Option<String>,
}

#[derive(Debug)]
struct ConfigError(String);

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for ConfigError {}

pub(crate) fn load() -> Result<Config, Box<dyn Error>> {
    let global_path = global_config_path();
    let mut raw = RawConfig::default();
    let mut loaded_path = None;

    if let Some(path) = global_path.as_deref()
        && path.is_file()
    {
        merge(&mut raw, read(path)?);
        loaded_path = Some(path.to_owned());
    }

    if let Some(path) = env::var_os("BOOMUX_CONFIG") {
        let path = PathBuf::from(path);
        if path.as_os_str().is_empty() {
            return Err(ConfigError("BOOMUX_CONFIG cannot be empty".into()).into());
        }
        merge(&mut raw, read(&path)?);
        loaded_path = Some(path);
    }

    resolve(raw, loaded_path)
}

pub(crate) fn global_config_path() -> Option<PathBuf> {
    env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .map(|home| home.join(".config"))
        })
        .map(|directory| directory.join("boomux/config.toml"))
}

fn read(path: &Path) -> Result<RawConfig, Box<dyn Error>> {
    let contents = fs::read_to_string(path)
        .map_err(|error| ConfigError(format!("could not read {}: {error}", path.display())))?;
    toml::from_str(&contents)
        .map_err(|error| ConfigError(format!("invalid config {}: {error}", path.display())).into())
}

fn merge(base: &mut RawConfig, next: RawConfig) {
    if next.terminal.is_some() {
        base.terminal = next.terminal;
    }
    if let Some(next_projects) = next.projects {
        let projects = base.projects.get_or_insert_default();
        if next_projects.roots.is_some() {
            projects.roots = next_projects.roots;
        }
        if next_projects.max_depth.is_some() {
            projects.max_depth = next_projects.max_depth;
        }
    }
    if let Some(next_recipes) = next.recipes {
        base.recipes.get_or_insert_default().extend(next_recipes);
    }
}

fn resolve(raw: RawConfig, path: Option<PathBuf>) -> Result<Config, Box<dyn Error>> {
    let terminal = raw
        .terminal
        .map(|terminal| terminal.trim().to_owned())
        .map(|terminal| {
            crate::terminal::validate_desktop_entry(&terminal)?;
            Ok::<_, Box<dyn Error>>(terminal)
        })
        .transpose()?;
    let projects = raw.projects.unwrap_or_default();
    let max_depth = projects.max_depth.unwrap_or(DEFAULT_PROJECT_SEARCH_DEPTH);
    if !(1..=MAX_PROJECT_SEARCH_DEPTH).contains(&max_depth) {
        return Err(ConfigError(format!(
            "projects.max_depth must be between 1 and {MAX_PROJECT_SEARCH_DEPTH}"
        ))
        .into());
    }

    let roots = projects
        .roots
        .unwrap_or_default()
        .into_iter()
        .map(|root| expand_root(&root))
        .collect::<Result<_, _>>()?;
    let recipes = resolve_recipes(raw.recipes.unwrap_or_default())?;

    Ok(Config {
        terminal,
        projects: ProjectsConfig { roots, max_depth },
        recipes,
        path,
    })
}

fn resolve_recipes(
    recipes: BTreeMap<String, RawRecipeConfig>,
) -> Result<Vec<RecipeConfig>, Box<dyn Error>> {
    let mut ids = HashSet::new();
    let mut labels = HashSet::new();
    let mut recipes = recipes
        .into_iter()
        .map(|(id, recipe)| {
            let id = id.trim().to_lowercase();
            if id.is_empty() || id == "default" || id.chars().any(char::is_control) {
                return Err(ConfigError(
                    "recipe IDs cannot be empty or use the reserved name 'default'".into(),
                )
                .into());
            }
            if !ids.insert(id.clone()) {
                return Err(ConfigError(format!("duplicate normalized recipe ID {id}")).into());
            }
            if recipe.terminals.is_empty() || recipe.terminals.len() > MAX_RECIPE_TERMINALS {
                return Err(ConfigError(format!(
                    "recipe {id} must define between 1 and {MAX_RECIPE_TERMINALS} terminals"
                ))
                .into());
            }

            let mut names = HashSet::new();
            let terminals = recipe
                .terminals
                .into_iter()
                .map(|terminal| {
                    let name = terminal.name.trim().to_owned();
                    if name.is_empty()
                        || name.len() > MAX_RECIPE_NAME_LENGTH
                        || name.starts_with('-')
                        || name.chars().any(char::is_control)
                    {
                        return Err(ConfigError(format!(
                            "recipe {id} contains invalid terminal name {name:?}"
                        )));
                    }
                    if !names.insert(name.clone()) {
                        return Err(ConfigError(format!(
                            "recipe {id} contains duplicate terminal name {name}"
                        )));
                    }
                    let command = terminal
                        .command
                        .map(|command| command.trim().to_owned())
                        .filter(|command| !command.is_empty());
                    Ok(RecipeTerminalConfig { name, command })
                })
                .collect::<Result<Vec<_>, ConfigError>>()?;
            let label = recipe
                .label
                .map(|label| label.trim().to_owned())
                .filter(|label| !label.is_empty())
                .unwrap_or_else(|| id.clone());
            if label.len() > MAX_RECIPE_NAME_LENGTH || label.chars().any(char::is_control) {
                return Err(ConfigError(format!("recipe {id} contains an invalid label")).into());
            }
            let normalized_label = label.to_lowercase();
            if normalized_label == "default" || !labels.insert(normalized_label) {
                return Err(ConfigError(format!(
                    "recipe {id} uses a reserved or duplicate label {label:?}"
                ))
                .into());
            }
            Ok(RecipeConfig {
                id,
                label,
                terminals,
            })
        })
        .collect::<Result<Vec<_>, Box<dyn Error>>>()?;
    recipes.sort_by_cached_key(|recipe| (recipe.terminals.len() > 1, recipe.label.to_lowercase()));
    Ok(recipes)
}

fn expand_root(root: &str) -> Result<PathBuf, Box<dyn Error>> {
    let path = if root == "~" {
        home_directory()?
    } else if let Some(relative) = root.strip_prefix("~/") {
        home_directory()?.join(relative)
    } else {
        PathBuf::from(root)
    };
    if !path.is_absolute() {
        return Err(ConfigError(format!(
            "project root must be absolute or start with ~: {root}"
        ))
        .into());
    }
    Ok(path)
}

fn home_directory() -> Result<PathBuf, Box<dyn Error>> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| ConfigError("HOME must be an absolute path to expand ~".into()).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_project_settings() {
        let raw: RawConfig = toml::from_str(
            r#"
                [projects]
                roots = ["/tmp/projects"]
                max_depth = 4
            "#,
        )
        .expect("valid config");
        let config = resolve(raw, None).expect("resolved config");

        assert_eq!(config.projects.roots, [PathBuf::from("/tmp/projects")]);
        assert_eq!(config.projects.max_depth, 4);
    }

    #[test]
    fn parses_terminal_preference() {
        let raw: RawConfig =
            toml::from_str(r#"terminal = "Alacritty.desktop""#).expect("valid config");

        let config = resolve(raw, None).expect("resolved config");

        assert_eq!(config.terminal.as_deref(), Some("Alacritty.desktop"));
    }

    #[test]
    fn terminal_preference_can_be_overridden() {
        let mut base: RawConfig =
            toml::from_str(r#"terminal = "Alacritty.desktop""#).expect("valid base");
        let next: RawConfig = toml::from_str(r#"terminal = "com.mitchellh.ghostty.desktop""#)
            .expect("valid override");

        merge(&mut base, next);
        let config = resolve(base, None).expect("resolved config");

        assert_eq!(
            config.terminal.as_deref(),
            Some("com.mitchellh.ghostty.desktop")
        );
    }

    #[test]
    fn rejects_invalid_terminal_desktop_entries() {
        for terminal in ["", "Alacritty", "-Alacritty.desktop", "bad\n.desktop"] {
            let raw: RawConfig =
                toml::from_str(&format!("terminal = {terminal:?}")).expect("valid TOML");

            assert!(resolve(raw, None).is_err());
        }
    }

    #[test]
    fn override_merges_only_specified_project_fields() {
        let mut base: RawConfig = toml::from_str(
            r#"
                [projects]
                roots = ["/tmp/projects"]
                max_depth = 2
            "#,
        )
        .expect("valid base");
        let next: RawConfig = toml::from_str(
            r#"
                [projects]
                max_depth = 5
            "#,
        )
        .expect("valid override");

        merge(&mut base, next);
        let config = resolve(base, None).expect("resolved config");

        assert_eq!(config.projects.roots, [PathBuf::from("/tmp/projects")]);
        assert_eq!(config.projects.max_depth, 5);
    }

    #[test]
    fn rejects_unknown_settings_and_relative_roots() {
        assert!(toml::from_str::<RawConfig>("unknown = true").is_err());

        let raw: RawConfig = toml::from_str(
            r#"
                [projects]
                roots = ["Projects"]
            "#,
        )
        .expect("valid TOML");
        assert!(resolve(raw, None).is_err());
    }

    #[test]
    fn rejects_unbounded_project_depth() {
        let raw: RawConfig = toml::from_str(
            r#"
                [projects]
                max_depth = 11
            "#,
        )
        .expect("valid TOML");

        assert!(resolve(raw, None).is_err());
    }

    #[test]
    fn parses_and_normalizes_recipes() {
        let raw: RawConfig = toml::from_str(
            r#"
                [recipes.full-dev]
                label = "Full Dev"
                terminals = [
                    { name = "opencode", command = "opencode" },
                    { name = "shell", command = "" },
                ]
            "#,
        )
        .expect("valid config");

        let config = resolve(raw, None).expect("resolved config");

        assert_eq!(config.recipes[0].id, "full-dev");
        assert_eq!(config.recipes[0].label, "Full Dev");
        assert_eq!(
            config.recipes[0].terminals[0].command.as_deref(),
            Some("opencode")
        );
        assert_eq!(config.recipes[0].terminals[1].command, None);
    }

    #[test]
    fn recipe_overrides_replace_matching_recipe_definitions() {
        let mut base: RawConfig = toml::from_str(
            r#"
                [recipes.dev]
                terminals = [{ name = "shell" }]
            "#,
        )
        .expect("valid base");
        let next: RawConfig = toml::from_str(
            r#"
                [recipes.dev]
                label = "Development"
                terminals = [{ name = "agent", command = "opencode" }]
            "#,
        )
        .expect("valid override");

        merge(&mut base, next);
        let config = resolve(base, None).expect("resolved config");

        assert_eq!(config.recipes[0].label, "Development");
        assert_eq!(config.recipes[0].terminals[0].name, "agent");
    }

    #[test]
    fn rejects_invalid_recipe_definitions() {
        for contents in [
            "[recipes.default]\nterminals = [{ name = 'shell' }]",
            "[recipes.Default]\nterminals = [{ name = 'shell' }]",
            "[recipes.empty]\nterminals = []",
            "[recipes.duplicate]\nterminals = [{ name = 'shell' }, { name = 'shell' }]",
            "[recipes.blank]\nterminals = [{ name = ' ' }]",
            "[recipes.option]\nterminals = [{ name = '--clear' }]",
            "[recipes.control]\nterminals = [{ name = \"bad\\nname\" }]",
        ] {
            let raw: RawConfig = toml::from_str(contents).expect("valid TOML");
            assert!(resolve(raw, None).is_err());
        }
    }

    #[test]
    fn rejects_recipe_ids_that_collide_after_layered_normalization() {
        let mut base: RawConfig =
            toml::from_str("[recipes.dev]\nterminals = [{ name = 'shell' }]").expect("valid base");
        let next: RawConfig = toml::from_str(
            "[recipes.\" Dev \"]\nterminals = [{ name = 'agent', command = 'opencode' }]",
        )
        .expect("valid override");

        merge(&mut base, next);

        assert!(resolve(base, None).is_err());
    }

    #[test]
    fn rejects_reserved_and_duplicate_recipe_labels() {
        for contents in [
            "[recipes.shell]\nlabel = 'Default'\nterminals = [{ name = 'shell' }]",
            "[recipes.one]\nlabel = 'Dev'\nterminals = [{ name = 'one' }]\n\
             [recipes.two]\nlabel = 'dev'\nterminals = [{ name = 'two' }]",
        ] {
            let raw: RawConfig = toml::from_str(contents).expect("valid TOML");
            assert!(resolve(raw, None).is_err());
        }
    }
}
