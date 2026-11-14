//! Finding Wallpaper Engine's files in a Steam install: the stock assets and
//! the workshop wallpapers.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Wallpaper Engine's Steam app id; workshop items live under this.
pub const APP_ID: &str = "431960";

/// Steam library roots of the common Linux installs (native, Debian's
/// symlink, Flatpak), relative to the home directory.
pub const STEAM_ROOTS: [&str; 3] = [
    ".local/share/Steam",
    ".steam/steam",
    ".var/app/com.valvesoftware.Steam/.local/share/Steam",
];

/// Steam's `libraryfolders.vdf` accepts both the modern indexed blocks and
/// the old numeric string entries.
fn parse_library_folders(text: &str) -> Vec<PathBuf> {
    let document = crate::acf::parse(text);
    let Some(section) = document.get("libraryfolders") else {
        return Vec::new();
    };
    section
        .entries()
        .iter()
        .filter_map(|(index, entry)| {
            index.parse::<u32>().ok()?;
            entry
                .as_str()
                .or_else(|| entry.get("path").and_then(|v| v.as_str()))
                .map(PathBuf::from)
        })
        .collect()
}

/// Installed Steam library roots, including libraries on other drives.
fn steam_libraries_from(home: &Path) -> Vec<PathBuf> {
    let mut result = Vec::new();
    let mut seen = HashSet::new();
    for relative in STEAM_ROOTS {
        let root = home.join(relative);
        if !root.is_dir() {
            continue;
        }
        let mut candidates = vec![root.clone()];
        if let Ok(text) = std::fs::read_to_string(root.join("steamapps/libraryfolders.vdf")) {
            candidates.extend(parse_library_folders(&text));
        }
        for candidate in candidates {
            if !candidate.is_dir() {
                continue;
            }
            let canonical = candidate.canonicalize().unwrap_or_else(|_| candidate.clone());
            if seen.insert(canonical) {
                result.push(candidate);
            }
        }
    }
    result
}

/// `relative` under every installed Steam library, where it exists.
fn steam_paths(relative: &str) -> Vec<PathBuf> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    steam_libraries_from(&home)
        .into_iter()
        .map(|root| root.join(relative))
        .filter(|path| path.exists())
        .collect()
}

fn steam_dirs(relative: &str) -> Vec<PathBuf> {
    steam_paths(relative).into_iter().filter(|p| p.is_dir()).collect()
}

/// Wallpaper Engine's install directory (`steamapps/common/wallpaper_engine`).
pub fn we_dir() -> Option<PathBuf> {
    steam_dirs("steamapps/common/wallpaper_engine")
        .into_iter()
        .next()
}

/// Wallpaper Engine's stock assets directory, if Steam installed it in a
/// usual place.
pub fn default_assets_dir() -> Option<PathBuf> {
    steam_dirs("steamapps/common/wallpaper_engine/assets")
        .into_iter()
        .next()
}

/// Steam's workshop metadata for Wallpaper Engine
/// (`steamapps/workshop/appworkshop_431960.acf`).
pub fn acf_path() -> Option<PathBuf> {
    steam_paths(&format!("steamapps/workshop/appworkshop_{APP_ID}.acf"))
        .into_iter()
        .find(|p| p.is_file())
}

/// The `.acf` that describes the items of the workshop content directory
/// `content` (`.../workshop/content/431960` → `.../workshop/appworkshop_431960.acf`).
pub fn acf_for(content: &Path) -> Option<PathBuf> {
    let workshop = content.parent()?.parent()?;
    let acf = workshop.join(format!("appworkshop_{APP_ID}.acf"));
    acf.is_file().then_some(acf)
}

/// The workshop content directories holding subscribed wallpapers, one per
/// wallpaper id.
pub fn workshop_dirs() -> Vec<PathBuf> {
    let mut dirs = steam_dirs(&format!("steamapps/workshop/content/{APP_ID}"));
    // `.steam/steam` is usually a symlink to `.local/share/Steam`.
    let mut seen = Vec::new();
    dirs.retain(|d| {
        let real = d.canonicalize().unwrap_or_else(|_| d.clone());
        let new = !seen.contains(&real);
        seen.push(real);
        new
    });
    dirs
}

/// Whether `s` looks like a workshop id.
pub fn is_workshop_id(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// Resolve what a user named as a wallpaper: an existing path (directory,
/// `.pkg` or scene `.json`), or a workshop id looked up in `workshop`.
pub fn resolve_wallpaper_in(spec: &str, workshop: &[PathBuf]) -> Result<PathBuf, String> {
    let path = Path::new(spec);
    if path.exists() {
        return Ok(path.to_owned());
    }
    if is_workshop_id(spec) {
        return workshop
            .iter()
            .map(|dir| dir.join(spec))
            .find(|p| p.is_dir())
            .ok_or_else(|| format!("workshop wallpaper {spec} is not installed"));
    }
    Err(format!("{spec}: no such file or directory"))
}

/// [`resolve_wallpaper_in`] against [`workshop_dirs`].
pub fn resolve_wallpaper(spec: &str) -> Result<PathBuf, String> {
    resolve_wallpaper_in(spec, &workshop_dirs())
}

/// The workshop id of the wallpaper at `path`: its directory's name when
/// that is a number (workshop items are stored by id).
pub fn workshop_id(path: &Path) -> Option<String> {
    let dir = if path.is_dir() { path } else { path.parent()? };
    let name = dir.file_name()?.to_str()?;
    is_workshop_id(name).then(|| name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_multiple_steam_libraries() {
        let home = tempfile::tempdir().unwrap();
        let primary = home.path().join(STEAM_ROOTS[0]);
        let secondary = home.path().join("ExternalDrive/SteamLibrary");
        std::fs::create_dir_all(primary.join("steamapps")).unwrap();
        std::fs::create_dir_all(&secondary).unwrap();
        std::fs::write(
            primary.join("steamapps/libraryfolders.vdf"),
            format!(
                "\"libraryfolders\" {{ \"0\" {{ \"path\" \"{}\" }} \"1\" {{ \"path\" \"{}\" }} \"2\" \"{}\" }}",
                primary.display(), secondary.display(), secondary.display()
            ),
        )
        .unwrap();
        let result = steam_libraries_from(home.path());
        assert_eq!(result.len(), 2);
        assert_eq!(result[0], primary);
        assert_eq!(result[1], secondary);
    }

    #[test]
    fn resolves_paths_and_ids() {
        let root = std::env::temp_dir().join(format!("wpe-steam-{}", std::process::id()));
        let item = root.join("123456");
        std::fs::create_dir_all(&item).unwrap();
        let workshop = [root.clone()];

        assert_eq!(resolve_wallpaper_in("123456", &workshop).unwrap(), item);
        let path = item.to_str().unwrap();
        assert_eq!(resolve_wallpaper_in(path, &workshop).unwrap(), item);
        assert!(resolve_wallpaper_in("999", &workshop).is_err());
        assert!(resolve_wallpaper_in("/nonexistent/x", &workshop).is_err());
        assert_eq!(workshop_id(&item).as_deref(), Some("123456"));
        assert_eq!(
            workshop_id(&item.join("scene.pkg")).as_deref(),
            Some("123456")
        );
        assert_eq!(workshop_id(&root), None);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
