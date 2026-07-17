use std::{
    env, fs,
    io::Write as _,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use tempfile::Builder;

const GUI_CONFIG_FILE_NAME: &str = "weather-gui.toml";
const GUI_CONFIG_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GuiConfigDocument {
    config_version: u32,
    #[serde(default)]
    debug: bool,
}

impl Default for GuiConfigDocument {
    fn default() -> Self {
        Self {
            config_version: GUI_CONFIG_VERSION,
            debug: false,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GuiConfigPayload {
    config_version: u32,
    debug: bool,
    config_path: String,
}

pub(crate) struct GuiConfigStore {
    path: PathBuf,
    document: Option<GuiConfigDocument>,
    load_error: Option<String>,
}

impl GuiConfigStore {
    pub(crate) fn open(path: PathBuf) -> Self {
        match load_or_create(&path) {
            Ok(document) => Self {
                path,
                document: Some(document),
                load_error: None,
            },
            Err(error) => Self {
                path,
                document: None,
                load_error: Some(format!("{error:#}")),
            },
        }
    }

    pub(crate) fn debug_for_launch(&self) -> bool {
        self.document.as_ref().is_some_and(|config| config.debug)
    }

    pub(crate) fn payload(&self) -> Result<GuiConfigPayload, String> {
        let document = self.document()?;
        Ok(self.payload_for(document))
    }

    pub(crate) fn set_debug(&mut self, debug: bool) -> Result<GuiConfigPayload, String> {
        let mut candidate = self.document()?.clone();
        candidate.debug = debug;
        write_atomic(&self.path, &candidate).map_err(|error| format!("{error:#}"))?;
        self.document = Some(candidate);
        self.payload()
    }

    fn document(&self) -> Result<&GuiConfigDocument, String> {
        self.document.as_ref().ok_or_else(|| {
            format!(
                "GUI 配置 `{}` 无法加载：{}",
                self.path.display(),
                self.load_error.as_deref().unwrap_or("未知错误")
            )
        })
    }

    fn payload_for(&self, document: &GuiConfigDocument) -> GuiConfigPayload {
        GuiConfigPayload {
            config_version: document.config_version,
            debug: document.debug,
            config_path: self.path.display().to_string(),
        }
    }
}

pub(crate) fn resolve_gui_config_path() -> Result<PathBuf> {
    let current_dir = env::current_dir().context("failed to resolve current directory")?;
    let explicit_gui = env::var_os("WEATHER_GUI_CONFIG").map(PathBuf::from);
    let engine_config = env::var_os("WEATHER_CONFIG").map(PathBuf::from);
    let home = env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    derive_gui_config_path(explicit_gui, engine_config, home, &current_dir)
}

fn derive_gui_config_path(
    explicit_gui: Option<PathBuf>,
    engine_config: Option<PathBuf>,
    home: Option<PathBuf>,
    current_dir: &Path,
) -> Result<PathBuf> {
    if let Some(path) = explicit_gui {
        return Ok(absolute_path(path, current_dir));
    }

    let engine_config = match engine_config {
        Some(path) => absolute_path(path, current_dir),
        None => home
            .context("neither HOME nor USERPROFILE is set")?
            .join(".weather")
            .join("config")
            .join("weather.toml"),
    };
    let parent = engine_config.parent().with_context(|| {
        format!(
            "engine config path `{}` has no parent directory",
            engine_config.display()
        )
    })?;
    Ok(parent.join(GUI_CONFIG_FILE_NAME))
}

fn absolute_path(path: PathBuf, current_dir: &Path) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        current_dir.join(path)
    }
}

fn load_or_create(path: &Path) -> Result<GuiConfigDocument> {
    if !path
        .try_exists()
        .with_context(|| format!("failed to inspect GUI config {}", path.display()))?
    {
        let config = GuiConfigDocument::default();
        write_atomic(path, &config)?;
        return Ok(config);
    }

    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read GUI config {}", path.display()))?;
    let config: GuiConfigDocument = toml::from_str(&content)
        .with_context(|| format!("failed to parse GUI config {}", path.display()))?;
    validate(&config)?;
    Ok(config)
}

fn validate(config: &GuiConfigDocument) -> Result<()> {
    if config.config_version != GUI_CONFIG_VERSION {
        bail!(
            "GUI config version {} is not supported; expected {}",
            config.config_version,
            GUI_CONFIG_VERSION
        );
    }
    Ok(())
}

fn write_atomic(path: &Path, config: &GuiConfigDocument) -> Result<()> {
    validate(config)?;
    let parent = path.parent().with_context(|| {
        format!(
            "GUI config path `{}` has no parent directory",
            path.display()
        )
    })?;
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create GUI config directory {}", parent.display()))?;
    let content = toml::to_string_pretty(config).context("failed to serialize GUI config")?;
    let mut temporary = Builder::new()
        .prefix(".weather-gui-")
        .tempfile_in(parent)
        .with_context(|| {
            format!(
                "failed to create temporary GUI config in {}",
                parent.display()
            )
        })?;
    temporary.write_all(content.as_bytes()).with_context(|| {
        format!(
            "failed to write temporary GUI config for {}",
            path.display()
        )
    })?;
    temporary
        .as_file_mut()
        .sync_all()
        .with_context(|| format!("failed to sync temporary GUI config for {}", path.display()))?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("failed to persist GUI config {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_next_to_engine_config() {
        let path = derive_gui_config_path(
            None,
            Some(PathBuf::from("profiles/dev/weather.toml")),
            Some(PathBuf::from("/home/test")),
            Path::new("/workspace"),
        )
        .unwrap();

        assert_eq!(path, Path::new("/workspace/profiles/dev/weather-gui.toml"));
    }

    #[test]
    fn explicit_gui_path_takes_precedence() {
        let path = derive_gui_config_path(
            Some(PathBuf::from("gui/custom.toml")),
            Some(PathBuf::from("engine/weather.toml")),
            None,
            Path::new("/workspace"),
        )
        .unwrap();

        assert_eq!(path, Path::new("/workspace/gui/custom.toml"));
    }

    #[test]
    fn missing_config_is_created_with_debug_disabled() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested/weather-gui.toml");

        let store = GuiConfigStore::open(path.clone());

        assert!(!store.debug_for_launch());
        assert_eq!(store.payload().unwrap().config_version, GUI_CONFIG_VERSION);
        let persisted = fs::read_to_string(path).unwrap();
        assert!(persisted.contains("debug = false"));
    }

    #[test]
    fn debug_update_is_persisted_and_reloaded() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("weather-gui.toml");
        let mut store = GuiConfigStore::open(path.clone());

        let updated = store.set_debug(true).unwrap();
        let reloaded = GuiConfigStore::open(path);

        assert!(updated.debug);
        assert!(reloaded.debug_for_launch());
    }

    #[test]
    fn unsupported_config_is_not_overwritten() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("weather-gui.toml");
        let original = "config_version = 99\ndebug = true\n";
        fs::write(&path, original).unwrap();

        let mut store = GuiConfigStore::open(path.clone());

        assert!(store.payload().is_err());
        assert!(store.set_debug(false).is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), original);
    }
}
