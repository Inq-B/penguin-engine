//! What the daemon remembers between runs (`state.json`: the wallpaper of
//! each output, property overrides, settings changed over the socket) and
//! the user's settings file (`config.toml`).

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use wpe_ipc::{Fill, Settings};

const APP_DIR: &str = wpe_library::APP_DIR;

/// `$XDG_STATE_HOME/penguin-engine/state.json` (`~/.local/state/...`).
pub fn state_path() -> Option<PathBuf> {
    Some(dirs::state_dir()?.join(APP_DIR).join("state.json"))
}

/// `$XDG_CONFIG_HOME/penguin-engine/config.toml` (`~/.config/...`).
pub fn config_path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join(APP_DIR).join("config.toml"))
}

/// The settings in the config file at `path`; the defaults without one.
pub fn load_config(path: &Path) -> Result<Settings> {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("{}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// One output's wallpaper.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedOutput {
    pub wallpaper: PathBuf,
    /// User properties changed from the wallpaper's defaults.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub properties: BTreeMap<String, Value>,
    /// Fill mode of this output, instead of the global one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fill: Option<Fill>,
    /// The wallpaper's own volume in percent of the global one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume: Option<u32>,
}

impl SavedOutput {
    pub fn new(wallpaper: PathBuf) -> Self {
        Self {
            wallpaper,
            properties: BTreeMap::new(),
            fill: None,
            volume: None,
        }
    }
}

/// Settings retained for a wallpaper after switching away from it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SavedWallpaper {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub properties: BTreeMap<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume: Option<u32>,
}

impl From<&SavedOutput> for SavedWallpaper {
    fn from(value: &SavedOutput) -> Self {
        Self { properties: value.properties.clone(), volume: value.volume }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct State {
    /// Settings changed over the socket; they win over the config file
    /// until it is reloaded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<Settings>,
    /// By output name.
    #[serde(default)]
    pub outputs: BTreeMap<String, SavedOutput>,
    /// The wallpaper set last, for outputs without their own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<SavedOutput>,
    /// Overrides keyed by wallpaper path, independent of the current output.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub wallpapers: BTreeMap<PathBuf, SavedWallpaper>,
}

impl State {
    /// The state saved at `path`; empty without one (or when it doesn't
    /// parse, which is logged).
    pub fn load(path: &Path) -> Self {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) => {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!("{}: {e}", path.display());
                }
                return Self::default();
            }
        };
        let mut state: Self = serde_json::from_str(&text)
            .inspect_err(|e| tracing::warn!("{}: {e}; starting afresh", path.display()))
            .unwrap_or_default();
        // Older state files stored only the active wallpaper on each output.
        // Preserve those overrides so changing wallpaper no longer discards them.
        state.migrate_legacy();
        state
    }

    /// Write the state to `path` atomically (a temporary file renamed over it).
    pub fn save(&self, path: &Path) -> Result<()> {
        let dir = path.parent().context("state path without a directory")?;
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
        let write = || -> std::io::Result<()> {
            let mut file = std::fs::File::create(&tmp)?;
            serde_json::to_writer_pretty(&mut file, self)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            std::fs::rename(&tmp, path)
        };
        write().inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })?;
        Ok(())
    }

    /// Upgrade the old output-only state format without losing any values.
    fn migrate_legacy(&mut self) {
        if let Some(saved) = &self.last {
            self.wallpapers
                .entry(saved.wallpaper.clone())
                .or_insert_with(|| SavedWallpaper::from(saved));
        }
        for saved in self.outputs.values() {
            self.wallpapers
                .entry(saved.wallpaper.clone())
                .or_insert_with(|| SavedWallpaper::from(saved));
        }
    }

    /// The last saved customizations for a wallpaper, even if it is not active.
    fn remembered(&self, wallpaper: &Path) -> SavedWallpaper {
        self.wallpapers.get(wallpaper).cloned()
            .or_else(|| self.outputs.values()
                .find(|entry| entry.wallpaper == wallpaper)
                .map(SavedWallpaper::from))
            .or_else(|| self.last.as_ref()
                .filter(|entry| entry.wallpaper == wallpaper)
                .map(SavedWallpaper::from))
            .unwrap_or_default()
    }

    /// Remembered values plus changes explicitly supplied for this activation.
    pub fn properties_for(
        &self,
        _name: &str,
        wallpaper: &Path,
        requested: &BTreeMap<String, Value>,
    ) -> BTreeMap<String, Value> {
        let mut properties = self.remembered(wallpaper).properties;
        properties.extend(requested.iter().map(|(k, v)| (k.clone(), v.clone())));
        properties
    }

    /// What output `name` should show: its own wallpaper, or else the one set last.
    pub fn for_output(&self, name: &str) -> Option<&SavedOutput> {
        self.outputs.get(name).or(self.last.as_ref())
    }

    /// Switch the output, retaining the old wallpaper and restoring the new
    /// wallpaper's saved appearance (unless explicit values override it).
    pub fn set_wallpaper(
        &mut self,
        name: &str,
        wallpaper: &Path,
        requested: BTreeMap<String, Value>,
    ) {
        self.migrate_legacy();
        let remembered = self.remembered(wallpaper);
        let properties = self.properties_for(name, wallpaper, &requested);
        let fill = self.outputs.get(name).and_then(|o| o.fill);
        let entry = SavedOutput {
            wallpaper: wallpaper.to_owned(),
            properties: properties.clone(),
            fill,
            volume: remembered.volume,
        };
        self.last = Some(SavedOutput { fill: None, ..entry.clone() });
        self.outputs.insert(name.to_owned(), entry);
        self.wallpapers.insert(wallpaper.to_owned(), SavedWallpaper {
            properties, volume: remembered.volume,
        });
    }

    /// Show a wallpaper everywhere without discarding previously saved looks.
    pub fn set_everywhere(&mut self, mut wallpaper: SavedOutput) {
        self.migrate_legacy();
        let saved = self.remembered(&wallpaper.wallpaper);
        let mut properties = saved.properties;
        properties.extend(wallpaper.properties);
        wallpaper.properties = properties.clone();
        wallpaper.volume = wallpaper.volume.or(saved.volume);
        self.wallpapers.insert(wallpaper.wallpaper.clone(), SavedWallpaper {
            properties, volume: wallpaper.volume,
        });
        self.outputs.clear();
        self.last = Some(wallpaper);
    }

    /// Remember one property change on the current output and its wallpaper.
    pub fn set_property(&mut self, name: &str, wallpaper: &Path, property: &str, value: Value) {
        self.entry(name, wallpaper).properties.insert(property.to_owned(), value.clone());
        self.wallpapers.entry(wallpaper.to_owned()).or_default()
            .properties.insert(property.to_owned(), value);
        if let Some(last) = &mut self.last
            && last.wallpaper == wallpaper
        {
            last.properties.insert(property.to_owned(), self.wallpapers[wallpaper].properties[property].clone());
        }
    }

    /// Reset saved property changes for this wallpaper, including the cache.
    pub fn clear_properties(&mut self, name: &str, wallpaper: &Path) {
        self.wallpapers.entry(wallpaper.to_owned()).or_default().properties.clear();
        if let Some(entry) = self.outputs.get_mut(name)
            && entry.wallpaper == wallpaper
        {
            entry.properties.clear();
        }
        if let Some(last) = &mut self.last
            && last.wallpaper == wallpaper
        {
            last.properties.clear();
        }
    }

    /// Save one wallpaper's own volume.
    pub fn set_volume(&mut self, name: &str, wallpaper: &Path, volume: u32) {
        let volume = (volume != 100).then_some(volume);
        self.entry(name, wallpaper).volume = volume;
        self.wallpapers.entry(wallpaper.to_owned()).or_default().volume = volume;
        if let Some(last) = &mut self.last
            && last.wallpaper == wallpaper
        {
            last.volume = volume;
        }
    }

    /// The selected wallpaper's volume, including saved values from earlier.
    pub fn volume(&self, name: &str, wallpaper: &Path) -> u32 {
        self.outputs.get(name)
            .filter(|s| s.wallpaper == wallpaper)
            .and_then(|s| s.volume)
            .or_else(|| self.remembered(wallpaper).volume)
            .unwrap_or(100)
    }

    pub fn set_fill(&mut self, name: &str, wallpaper: &Path, fill: Option<Fill>) {
        self.entry(name, wallpaper).fill = fill;
    }

    /// Ensure an output entry points to the currently playing wallpaper.
    fn entry(&mut self, name: &str, wallpaper: &Path) -> &mut SavedOutput {
        if self.outputs.get(name).is_none_or(|s| s.wallpaper != wallpaper) {
            let remembered = self.remembered(wallpaper);
            let fill = self.outputs.get(name).and_then(|s| s.fill);
            self.outputs.insert(name.to_owned(), SavedOutput {
                wallpaper: wallpaper.to_owned(),
                properties: remembered.properties,
                volume: remembered.volume,
                fill,
            });
        }
        self.outputs.get_mut(name).expect("just inserted output")
    }

    /// Current or remembered property changes, for live reloads and the GUI.
    pub fn overrides(&self, name: &str, wallpaper: &Path) -> BTreeMap<String, Value> {
        self.outputs.get(name)
            .filter(|s| s.wallpaper == wallpaper)
            .map(|s| s.properties.clone())
            .unwrap_or_else(|| self.remembered(wallpaper).properties)
    }
}

/// Settings given on the command line, which win over everything else.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Overrides {
    pub fps: Option<u32>,
    pub audio: Option<bool>,
    pub volume: Option<u32>,
    pub fill: Option<Fill>,
    pub layer: Option<wpe_ipc::Layer>,
    pub passthrough: Option<bool>,
}

impl Overrides {
    pub fn apply(&self, mut settings: Settings) -> Settings {
        settings.fps = self.fps.unwrap_or(settings.fps);
        settings.audio = self.audio.unwrap_or(settings.audio);
        settings.volume = self.volume.unwrap_or(settings.volume);
        settings.fill = self.fill.unwrap_or(settings.fill);
        settings.layer = self.layer.unwrap_or(settings.layer);
        settings.passthrough = self.passthrough.unwrap_or(settings.passthrough);
        settings
    }
}

/// The settings in effect at startup: the config file, then what was
/// changed over the socket last run, then the command line.
pub fn startup_settings(config: &Settings, state: &State, cli: &Overrides) -> Settings {
    cli.apply(state.settings.clone().unwrap_or_else(|| config.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wpe_ipc::Layer;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wpe-state-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn saves_and_restores() {
        let dir = temp_dir("save");
        let path = dir.join("nested/state.json");
        assert_eq!(State::load(&path), State::default());

        let mut state = State::default();
        state.set_wallpaper("DP-1", Path::new("/w/1"), BTreeMap::new());
        state.set_property("DP-1", Path::new("/w/1"), "speed", json!(0.5));
        state.set_fill("DP-1", Path::new("/w/1"), Some(Fill::Fit));
        state.set_wallpaper(
            "HDMI-A-1",
            Path::new("/w/2"),
            BTreeMap::from([("rain".to_owned(), json!(false))]),
        );
        state.settings = Some(Settings {
            fps: 30,
            ..Default::default()
        });
        state.save(&path).unwrap();
        // Nothing but the state file is left behind.
        let files: Vec<_> = std::fs::read_dir(path.parent().unwrap()).unwrap().collect();
        assert_eq!(files.len(), 1);

        let back = State::load(&path);
        assert_eq!(back, state);
        let dp = back.for_output("DP-1").unwrap();
        assert_eq!(dp.wallpaper, Path::new("/w/1"));
        assert_eq!(dp.properties["speed"], json!(0.5));
        assert_eq!(dp.fill, Some(Fill::Fit));
        // A new output gets the wallpaper set last, without per-output fill.
        let new = back.for_output("DP-3").unwrap();
        assert_eq!(new.wallpaper, Path::new("/w/2"));
        assert_eq!(new.fill, None);
        assert_eq!(new.properties["rain"], json!(false));

        // Setting a new wallpaper keeps the output's fill, drops properties.
        state.set_wallpaper("DP-1", Path::new("/w/3"), BTreeMap::new());
        let dp = state.for_output("DP-1").unwrap();
        assert!(dp.properties.is_empty());
        assert_eq!(dp.fill, Some(Fill::Fit));

        // Resetting forgets the overrides, here and for new outputs.
        state.clear_properties("HDMI-A-1", Path::new("/w/2"));
        assert!(state.outputs["HDMI-A-1"].properties.is_empty());
        assert!(state.last.as_ref().unwrap().properties.is_empty());

        std::fs::write(&path, "{broken").unwrap();
        assert_eq!(State::load(&path), State::default());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn wallpaper_volume_stays_with_the_wallpaper() {
        let mut state = State::default();
        let (one, two) = (Path::new("/w/1"), Path::new("/w/2"));
        state.set_wallpaper("DP-1", one, BTreeMap::new());
        assert_eq!(state.volume("DP-1", one), 100);
        state.set_volume("DP-1", one, 40);
        assert_eq!(state.volume("DP-1", one), 40);
        // A new output showing the same wallpaper sounds the same.
        assert_eq!(state.volume("HDMI-A-1", one), 40);
        // Another wallpaper starts at full volume.
        state.set_wallpaper("DP-1", two, BTreeMap::new());
        assert_eq!(state.volume("DP-1", two), 100);
        state.set_volume("DP-1", two, 100);
        assert_eq!(state.outputs["DP-1"].volume, None);
    }

    #[test]
    fn everywhere_replaces_every_output() {
        let mut state = State::default();
        state.set_wallpaper("DP-1", Path::new("/w/1"), BTreeMap::new());
        state.set_everywhere(SavedOutput::new("/w/9".into()));
        assert_eq!(
            state.for_output("DP-1").unwrap().wallpaper,
            Path::new("/w/9")
        );
        // Property changes follow onto new outputs showing that wallpaper,
        // and keep the ones it was set with.
        state
            .last
            .as_mut()
            .unwrap()
            .properties
            .insert("y".into(), json!(1));
        state.set_property("DP-1", Path::new("/w/9"), "x", json!(true));
        let dp2 = state.for_output("DP-2").unwrap();
        assert_eq!(dp2.properties["x"], json!(true));
        assert_eq!(dp2.properties["y"], json!(1));
        assert_eq!(state.overrides("DP-1", Path::new("/w/9")).len(), 2);
        assert!(state.overrides("DP-1", Path::new("/w/8")).is_empty());
    }

    #[test]
    fn retains_properties_and_volume_when_switching_wallpapers() {
        let mut state = State::default();
        let (one, two) = (Path::new("/w/one"), Path::new("/w/two"));
        state.set_wallpaper("DP-1", one, BTreeMap::new());
        state.set_property("DP-1", one, "rain", json!(false));
        state.set_volume("DP-1", one, 42);
        state.set_wallpaper("DP-1", two, BTreeMap::new());
        state.set_property("DP-1", two, "speed", json!(3));
        assert_eq!(state.properties_for("DP-1", one, &BTreeMap::new())["rain"], json!(false));
        assert_eq!(state.volume("DP-1", one), 42);
        state.set_wallpaper("DP-1", one, BTreeMap::new());
        assert_eq!(state.overrides("DP-1", one)["rain"], json!(false));
        assert_eq!(state.volume("DP-1", one), 42);
        state.set_wallpaper("DP-1", two, BTreeMap::new());
        assert_eq!(state.overrides("DP-1", two)["speed"], json!(3));
    }

    #[test]
    fn reset_clears_restored_values_but_not_other_wallpapers() {
        let mut state = State::default();
        let (one, two) = (Path::new("/w/one"), Path::new("/w/two"));
        state.set_wallpaper("DP-1", one, BTreeMap::new());
        state.set_property("DP-1", one, "rain", json!(false));
        state.set_wallpaper("DP-1", two, BTreeMap::new());
        state.set_property("DP-1", two, "speed", json!(4));
        state.set_wallpaper("DP-1", one, BTreeMap::new());
        state.clear_properties("DP-1", one);
        state.set_wallpaper("DP-1", two, BTreeMap::new());
        state.set_wallpaper("DP-1", one, BTreeMap::new());
        assert!(state.overrides("DP-1", one).is_empty());
        assert_eq!(state.overrides("DP-1", two)["speed"], json!(4));
    }

    #[test]
    fn loads_legacy_output_only_state_and_retains_its_look() {
        let dir = temp_dir("legacy");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        std::fs::write(&path, r#"{
            "outputs": {"DP-1": {"wallpaper": "/w/legacy", "properties": {"x": 9}, "volume": 20}},
            "last": {"wallpaper": "/w/legacy", "properties": {"x": 9}, "volume": 20}
        }"#).unwrap();
        let mut state = State::load(&path);
        state.set_wallpaper("DP-1", Path::new("/w/other"), BTreeMap::new());
        state.set_wallpaper("DP-1", Path::new("/w/legacy"), BTreeMap::new());
        assert_eq!(state.overrides("DP-1", Path::new("/w/legacy"))["x"], json!(9));
        assert_eq!(state.volume("DP-1", Path::new("/w/legacy")), 20);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn config_files() {
        let dir = temp_dir("config");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        assert_eq!(load_config(&path).unwrap(), Settings::default());
        std::fs::write(&path, "fps = 30\nfill = \"fit\"\nlayer = \"bottom\"\n").unwrap();
        let config = load_config(&path).unwrap();
        assert_eq!(
            config,
            Settings {
                fps: 30,
                fill: Fill::Fit,
                layer: Layer::Bottom,
                ..Default::default()
            }
        );
        std::fs::write(&path, "fps = \"fast\"").unwrap();
        assert!(load_config(&path).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn settings_precedence() {
        let config = Settings {
            fps: 30,
            volume: 50,
            ..Default::default()
        };
        let cli = Overrides {
            volume: Some(10),
            ..Default::default()
        };
        let mut state = State::default();
        assert_eq!(
            startup_settings(&config, &state, &cli),
            Settings {
                fps: 30,
                volume: 10,
                ..Default::default()
            }
        );
        state.settings = Some(Settings {
            fps: 60,
            ..Default::default()
        });
        assert_eq!(
            startup_settings(&config, &state, &cli),
            Settings {
                fps: 60,
                volume: 10,
                ..Default::default()
            }
        );
    }
}
