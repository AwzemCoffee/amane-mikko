use std::env;
use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU32, Ordering};

use png::{BitDepth, ColorType, Encoder};

const FOLDER: &str = "amane-tray";
const EXTENSIONS: [&str; 2] = ["svg", "png"];

static NEXT_NAME: AtomicU32 = AtomicU32::new(0);

#[derive(Debug, Clone)]
pub struct IconStore {
    temp_dir: PathBuf,
    xdg_roots: Vec<PathBuf>,
}

impl IconStore {
    pub fn from_env() -> Self {
        let runtime = env::var_os("XDG_RUNTIME_DIR").unwrap_or_else(|| env::temp_dir().into());
        let temp_dir = PathBuf::from(runtime).join(FOLDER);
        let xdg_roots = collect_xdg_roots(
            env::var_os("HOME").map(PathBuf::from),
            env::var("XDG_DATA_DIRS").ok(),
        );
        Self {
            temp_dir,
            xdg_roots,
        }
    }

    #[cfg(test)]
    pub fn with_dirs(temp_dir: PathBuf, xdg_roots: Vec<PathBuf>) -> Self {
        Self {
            temp_dir,
            xdg_roots,
        }
    }

    /// Resolves a StatusNotifierItem's icon to a local image file path that Amane's `Image` can render.
    /// Checks absolute paths, `IconThemePath`, XDG icon directories, and finally `IconPixmap` (ARGB32).
    pub fn resolve(
        &self,
        icon_name: &str,
        icon_theme_path: &str,
        pixmaps: &[(i32, i32, Vec<u8>)],
    ) -> Option<String> {
        if !icon_name.is_empty() {
            let direct = Path::new(icon_name);
            if direct.is_absolute() && direct.is_file() {
                return Some(icon_name.to_string());
            }

            if !icon_theme_path.is_empty()
                && let Some(found) = search_dir(Path::new(icon_theme_path), icon_name, 3)
            {
                return Some(found.to_string_lossy().into_owned());
            }

            if let Some(found) = self.search_xdg_icons(icon_name) {
                return Some(found.to_string_lossy().into_owned());
            }
        }

        self.save_best_pixmap(pixmaps)
            .map(|path| path.to_string_lossy().into_owned())
    }

    /// Removes a temp PNG created by `save_best_pixmap`; never deletes app-owned or theme files.
    pub fn remove_temp(&self, path: &Option<String>) {
        let Some(path) = path else {
            return;
        };

        let path = Path::new(path);
        if path.parent() == Some(self.temp_dir.as_path()) {
            let _ = fs::remove_file(path);
        }
    }

    fn search_xdg_icons(&self, name: &str) -> Option<PathBuf> {
        for root in &self.xdg_roots {
            if !root.is_dir() {
                continue;
            }

            // Prefer hicolor / common scalable & 48x48/32x32/24x24/22x22 slots before root check
            for sub in [
                "hicolor/scalable/apps",
                "hicolor/scalable/status",
                "hicolor/48x48/apps",
                "hicolor/48x48/status",
                "hicolor/32x32/apps",
                "hicolor/32x32/status",
                "hicolor/24x24/apps",
                "hicolor/24x24/status",
                "hicolor/22x22/apps",
                "hicolor/22x22/status",
            ] {
                if let Some(found) = check_exact(&root.join(sub), name) {
                    return Some(found);
                }
            }

            if let Some(found) = check_exact(root, name) {
                return Some(found);
            }
        }

        None
    }

    /// SNI `IconPixmap` entries are `(width, height, argb32_big_endian_bytes)`.
    fn save_best_pixmap(&self, pixmaps: &[(i32, i32, Vec<u8>)]) -> Option<PathBuf> {
        let best = pixmaps
            .iter()
            .filter(|(w, h, bytes)| {
                *w > 0 && *h > 0 && bytes.len() == (*w as usize) * (*h as usize) * 4
            })
            .max_by_key(|(w, h, _)| (*w as i64) * (*h as i64))?;

        let width = best.0 as u32;
        let height = best.1 as u32;
        let rgba = argb32_be_to_rgba8(&best.2);

        fs::create_dir_all(&self.temp_dir).ok()?;

        let seq = NEXT_NAME.fetch_add(1, Ordering::Relaxed);
        let path = self.temp_dir.join(format!("{}-{seq}.png", process::id()));

        let file = BufWriter::new(File::create(&path).ok()?);
        let mut encoder = Encoder::new(file, width, height);
        encoder.set_color(ColorType::Rgba);
        encoder.set_depth(BitDepth::Eight);
        encoder
            .write_header()
            .ok()?
            .write_image_data(&rgba)
            .ok()?;

        Some(path)
    }
}

fn collect_xdg_roots(home: Option<PathBuf>, xdg_data_dirs: Option<String>) -> Vec<PathBuf> {
    let mut roots = Vec::new();

    if let Some(home) = home {
        roots.push(home.join(".local/share/icons"));
        roots.push(home.join(".icons"));
    }

    let data_dirs = xdg_data_dirs
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| String::from("/usr/local/share:/usr/share"));

    for dir in data_dirs.split(':').filter(|dir| !dir.is_empty()) {
        roots.push(PathBuf::from(dir).join("icons"));
        roots.push(PathBuf::from(dir).join("pixmaps"));
    }

    roots
}

fn check_exact(dir: &Path, name: &str) -> Option<PathBuf> {
    if !dir.is_dir() {
        return None;
    }

    for ext in EXTENSIONS {
        let candidate = dir.join(format!("{name}.{ext}"));
        if candidate.is_file() {
            return Some(candidate);
        }
    }

    None
}

fn search_dir(dir: &Path, name: &str, max_depth: usize) -> Option<PathBuf> {
    if let Some(found) = check_exact(dir, name) {
        return Some(found);
    }

    if max_depth == 0 {
        return None;
    }

    let entries = fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir()
            && let Some(found) = search_dir(&path, name, max_depth - 1)
        {
            return Some(found);
        }
    }

    None
}

pub fn argb32_be_to_rgba8(argb: &[u8]) -> Vec<u8> {
    let mut rgba = Vec::with_capacity(argb.len());
    for pixel in argb.chunks_exact(4) {
        // Network byte order ARGB -> RGBA
        rgba.push(pixel[1]);
        rgba.push(pixel[2]);
        rgba.push(pixel[3]);
        rgba.push(pixel[0]);
    }
    rgba
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphics::image;

    fn unique_test_dir(tag: &str) -> PathBuf {
        let seq = NEXT_NAME.fetch_add(1, Ordering::Relaxed);
        let dir = env::temp_dir().join(format!("amane-tray-test-{tag}-{}-{seq}", process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn converts_argb32_be_to_rgba8() {
        // Alpha=0x80, R=0x11, G=0x22, B=0x33
        let argb = [0x80, 0x11, 0x22, 0x33, 0xff, 0xaa, 0xbb, 0xcc];
        assert_eq!(
            argb32_be_to_rgba8(&argb),
            vec![0x11, 0x22, 0x33, 0x80, 0xaa, 0xbb, 0xcc, 0xff]
        );
    }

    #[test]
    fn saves_largest_pixmap_in_store_temp_dir_and_cleans_up() {
        let root = unique_test_dir("pixmap");
        let store = IconStore::with_dirs(root.join("temp"), Vec::new());

        let small = (1, 1, vec![0xff, 0x00, 0x00, 0xff]);
        let larger = (
            2,
            1,
            vec![0xff, 0x10, 0x20, 0x30, 0x80, 0x40, 0x50, 0x60],
        );

        let saved = store.resolve("", "", &[small, larger]).expect("saved png path");
        assert!(Path::new(&saved).starts_with(root.join("temp")));

        let bitmap = image::read(Path::new(&saved)).expect("valid png");
        assert_eq!(bitmap.width(), 2);
        assert_eq!(bitmap.height(), 1);
        assert_eq!(
            *bitmap.pixels,
            [0x10, 0x20, 0x30, 0xff, 0x40, 0x50, 0x60, 0x80]
        );

        store.remove_temp(&Some(saved.clone()));
        assert!(!Path::new(&saved).exists());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn resolves_xdg_hicolor_and_never_deletes_non_temp_icons() {
        let root = unique_test_dir("xdg");
        let xdg_icons = root.join("icons");
        let status_dir = xdg_icons.join("hicolor/32x32/status");
        fs::create_dir_all(&status_dir).unwrap();
        let icon_file = status_dir.join("discord-tray.png");
        fs::write(&icon_file, b"fake").unwrap();

        let store = IconStore::with_dirs(root.join("temp"), vec![xdg_icons]);
        let resolved = store
            .resolve("discord-tray", "", &[])
            .expect("should find xdg icon");
        assert_eq!(Path::new(&resolved), icon_file);

        // remove_temp must never delete theme/app icons outside temp_dir
        store.remove_temp(&Some(resolved));
        assert!(icon_file.exists());

        let _ = fs::remove_dir_all(&root);
    }
}
