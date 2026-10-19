//! Optional owner-local icon artwork. A missing or malformed file keeps the built-in vector icon.
//!
//! The artwork never ships with FilmCraft (AGENTS.md §1): it is read from a per-user folder
//! outside any checkout, so every build on the owner's machine shows it by default.
//! `FILMCRAFT_ICON_DIR` overrides the folder. Files are `<icon-name>.png` (kebab case of the
//! [`Icon`] variant, e.g. `track-select-fwd.png`), at most 256×256. White-on-alpha masks take
//! the caller's tint; coloured artwork keeps its colours.

use crate::icons::Icon;
use egui::{Color32, Context, Painter, Rect, TextureHandle};
use std::io::Read;
use std::path::PathBuf;

const MAX_BYTES: u64 = 262_144;
const MAX_SIDE: u32 = 256;

/// `~/Library/Application Support/FilmCraft/owner-assets/icons` on macOS,
/// `%APPDATA%\FilmCraft\owner-assets\icons` on Windows, `~/.local/share/filmcraft/owner-assets/icons` elsewhere.
fn icon_dir() -> Option<PathBuf> {
    // Unit tests draw the built-in icons, whatever is installed on the machine.
    if cfg!(test) {
        return None;
    }
    if let Some(dir) = std::env::var_os("FILMCRAFT_ICON_DIR") {
        return Some(PathBuf::from(dir));
    }
    if cfg!(target_os = "windows") {
        return Some(PathBuf::from(std::env::var_os("APPDATA")?).join("FilmCraft").join("owner-assets").join("icons"));
    }
    let home = PathBuf::from(std::env::var_os("HOME")?);
    if cfg!(target_os = "macos") {
        Some(home.join("Library/Application Support/FilmCraft/owner-assets/icons"))
    } else {
        Some(home.join(".local/share/filmcraft/owner-assets/icons"))
    }
}

/// The file stem for `icon`: `TrackSelectFwd` → `track-select-fwd`.
pub(crate) fn file_stem(icon: Icon) -> String {
    let mut out = String::new();
    for (i, c) in format!("{icon:?}").chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                out.push('-');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

fn read_image(icon: Icon) -> Option<image::RgbaImage> {
    let path = icon_dir()?.join(format!("{}.png", file_stem(icon)));
    let mut bytes = Vec::new();
    std::fs::File::open(path).ok()?.take(MAX_BYTES + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > MAX_BYTES {
        return None;
    }
    let reader = image::ImageReader::new(std::io::Cursor::new(&bytes)).with_guessed_format().ok()?;
    let (w, h) = reader.into_dimensions().ok()?;
    if w == 0 || h == 0 || w > MAX_SIDE || h > MAX_SIDE {
        return None;
    }
    Some(image::load_from_memory(&bytes).ok()?.to_rgba8())
}

/// Whether any clearly visible pixel has colour (then the artwork keeps its colours).
fn is_coloured(image: &image::RgbaImage) -> bool {
    image.pixels().any(|p| {
        let [r, g, b, a] = p.0;
        a > 40 && r.max(g).max(b) - r.min(g).min(b) > 40
    })
}

#[derive(Clone)]
struct Loaded {
    texture: TextureHandle,
    coloured: bool,
}

fn load(ctx: &Context, icon: Icon) -> Option<Loaded> {
    let id = egui::Id::new(("owner-icon", icon));
    if let Some(cached) = ctx.data(|d| d.get_temp::<Option<Loaded>>(id)) {
        return cached;
    }
    let loaded = read_image(icon).map(|image| {
        let coloured = is_coloured(&image);
        let size = [image.width() as usize, image.height() as usize];
        let pixels = image.pixels().map(|p| Color32::from_rgba_unmultiplied(p.0[0], p.0[1], p.0[2], p.0[3])).collect();
        let texture = ctx.load_texture(format!("owner-icon-{}", file_stem(icon)), egui::ColorImage::new(size, pixels), egui::TextureOptions::LINEAR);
        Loaded { texture, coloured }
    });
    ctx.data_mut(|d| d.insert_temp(id, loaded.clone()));
    loaded
}

/// Paints the owner-local artwork for `icon` into `rect`; returns false when there is none.
pub(crate) fn paint(painter: &Painter, rect: Rect, icon: Icon, color: Color32) -> bool {
    let Some(loaded) = load(painter.ctx(), icon) else { return false };
    let tint = if loaded.coloured { Color32::from_white_alpha(color.a()) } else { color };
    // Square, centred: the artwork carries its own padding.
    let side = rect.width().min(rect.height());
    let square = Rect::from_center_size(rect.center(), egui::vec2(side, side));
    painter.image(loaded.texture.id(), square, Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)), tint);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_stems_are_kebab_case() {
        assert_eq!(file_stem(Icon::TrackSelectFwd), "track-select-fwd");
        assert_eq!(file_stem(Icon::EyeOff), "eye-off");
        assert_eq!(file_stem(Icon::GoToIn), "go-to-in");
        assert_eq!(file_stem(Icon::Razor), "razor");
    }

    #[test]
    fn masks_take_the_tint_and_coloured_artwork_keeps_its_colours() {
        let ctx = Context::default();
        for (icon, coloured) in [(Icon::Razor, false), (Icon::Sequence, true)] {
            let texture = ctx.load_texture("t", egui::ColorImage::filled([1, 1], Color32::WHITE), Default::default());
            ctx.data_mut(|d| d.insert_temp(egui::Id::new(("owner-icon", icon)), Some(Loaded { texture, coloured })));
        }
        let tint = Color32::from_gray(100);
        let mut output = ctx.run_ui(Default::default(), |ui| {
            for icon in [Icon::Razor, Icon::Sequence] {
                assert!(paint(ui.painter(), Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(18.0, 18.0)), icon, tint));
            }
        });
        let colors: Vec<_> = output
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::Shape::Mesh(mesh) => mesh.vertices.first().map(|v| v.color),
                _ => None,
            })
            .collect();
        output.textures_delta.clear();
        assert_eq!(colors, [tint, Color32::WHITE]);
    }

    #[test]
    fn colour_detection_ignores_grey_and_faint_pixels() {
        let grey = image::RgbaImage::from_pixel(2, 2, image::Rgba([200, 200, 200, 255]));
        let faint = image::RgbaImage::from_pixel(2, 2, image::Rgba([255, 0, 0, 10]));
        let blue = image::RgbaImage::from_pixel(2, 2, image::Rgba([60, 120, 220, 255]));
        assert!(!is_coloured(&grey));
        assert!(!is_coloured(&faint));
        assert!(is_coloured(&blue));
    }
}
