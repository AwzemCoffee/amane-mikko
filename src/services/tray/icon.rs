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

/// Resolves a StatusNotifierItem's icon to a local image file path that Amane's `Image` can render.
/// Checks absolute paths, `IconThemePath`, XDG icon directories, and finally `IconPixmap` (ARGB32).
pub fn resolve(
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

        if let Some(found) = search_xdg_icons(icon_name) {
            return Some(found.to_string_lossy().into_owned());
        }
    }

    save_best_pixmap(pixmaps).map(|path| path.to_string_lossy().into_owned())
}

/// Removes a temp PNG created by `save_best_pixmap`; never deletes app-owned or theme files.
pub fn remove_temp(path: &Option<String>) {
    let Some(path) = path else {
        return;
    };

    let path = Path::new(path);
    if path.parent() == Some(&temp_folder()) {
        let _ = fs::remove_file(path);
    }
}

fn temp_folder() -> PathBuf {
    let runtime = env::var_os("XDG_RUNTIME_DIR").unwrap_or_else(|| env::temp_dir().into());
    PathBuf::from(runtime).join(FOLDER)
}

fn search_xdg_icons(name: &str) -> Option<PathBuf> {
    let mut roots = Vec::new();

    if let Some(home) = env::var_os("HOME") {
        let home = PathBuf::from(home);
        roots.push(home.join(".local/share/icons"));
        roots.push(home.join(".icons"));
    }

    let data_dirs = env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| String::from("/usr/local/share:/usr/share"));

    for dir in data_dirs.split(':').filter(|dir| !dir.is_empty()) {
        roots.push(PathBuf::from(dir).join("icons"));
        roots.push(PathBuf::from(dir).join("pixmaps"));
    }

    for root in &roots {
        if !root.is_dir() {
            continue;
        }

        // Prefer hicolor / common scalable & 48x48/32x32/24x24/22x22 slots before deep walk
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

/// SNI `IconPixmap` entries are `(width, height, argb32_big_endian_bytes)`.
fn save_best_pixmap(pixmaps: &[(i32, i32, Vec<u8>)]) -> Option<PathBuf> {
    let best = pixmaps
        .iter()
        .filter(|(w, h, bytes)| {
            *w > 0 && *h > 0 && bytes.len() == (*w as usize) * (*h as usize) * 4
        })
        .max_by_key(|(w, h, _)| (*w as i64) * (*h as i64))?;

    let width = best.0 as u32;
    let height = best.1 as u32;
    let rgba = argb32_be_to_rgba8(&best.2);

    let folder = temp_folder();
    fs::create_dir_all(&folder).ok()?;

    let seq = NEXT_NAME.fetch_add(1, Ordering::Relaxed);
    let path = folder.join(format!("{}-{seq}.png", process::id()));

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
    fn saves_largest_pixmap_as_png_and_cleans_up() {
        let small = (1, 1, vec![0xff, 0x00, 0x00, 0xff]);
        let larger = (
            2,
            1,
            vec![0xff, 0x10, 0x20, 0x30, 0x80, 0x40, 0x50, 0x60],
        );

        let saved = resolve("", "", &[small, larger]).expect("saved png path");
        let bitmap = image::read(Path::new(&saved)).expect("valid png");

        assert_eq!(bitmap.width(), 2);
        assert_eq!(bitmap.height(), 1);
        assert_eq!(*bitmap.pixels, [0x10, 0x20, 0x30, 0xff, 0x40, 0x50, 0x60, 0x80]);

        remove_temp(&Some(saved.clone()));
        assert!(!Path::new(&saved).exists());
    }
}
