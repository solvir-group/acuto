use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::UNIX_EPOCH,
};

use anyhow::{Context as _, Result};
use gpui::{
    AnyElement, App, Global, IntoElement, ObjectFit, ParentElement, Styled, StyledImage, Window,
    WindowBackgroundAppearance, div, img, px,
};
use image::{ImageBuffer, ImageReader, Rgb, imageops};
use theme::ActiveTheme;
use util::ResultExt as _;

/// Width the wallpaper is blurred at. The blur is so wide that detail finer
/// than this is gone anyway, and working small keeps it quick.
const BLUR_WIDTH: u32 = 640;
/// Width of the image that is saved and drawn. Large enough that the GPU's
/// stretch to the screen is a gentle one and the dither stays invisible.
const OUTPUT_WIDTH: u32 = 1920;
const BLUR_SIGMA: f32 = 22.0;
/// The darkest the wallpaper may get once frosted for a light theme, as a
/// fraction of white. Mixing white over a dark wallpaper only greys it, and a
/// dark warm colour greyed is brown. Lifting the brightness instead turns
/// every part of the wallpaper into a pale tint of its own colour.
const LIGHTNESS_FLOOR: f32 = 0.74;
/// The brightest the wallpaper may get once frosted for a dark theme. A dark
/// theme's text is light, and lifting the wallpaper towards white under it
/// would put that text on a near-white ground.
const DARK_LIGHTNESS_CEILING: f32 = 0.2;
/// The darkest a dark backdrop gets, so a black wallpaper still reads as glass
/// rather than as a hole in the window.
const DARK_LIGHTNESS_FLOOR: f32 = 0.06;
/// How much of each colour's difference from grey survives. Kept close to
/// whole so the tint is plainly the wallpaper's own colour.
const COLOURFULNESS: f32 = 0.95;
/// The same for a dark backdrop, where full-strength colour at low lightness
/// turns muddy.
const DARK_COLOURFULNESS: f32 = 0.35;

#[derive(Default)]
struct WallpaperBackdrop {
    image: Option<Arc<Path>>,
    /// Whether `image` was frosted for a dark theme.
    image_is_dark: bool,
    loading: bool,
    /// When the wallpaper was last looked at. Looked at again after
    /// [`RECHECK_AFTER`], so a changed wallpaper is picked up and a failed load
    /// is retried rather than leaving the window without a backdrop for good.
    checked_at: Option<std::time::Instant>,
}

const RECHECK_AFTER: std::time::Duration = std::time::Duration::from_secs(60);

impl Global for WallpaperBackdrop {}

/// A frosted theme is one whose window is opaque while its background colour
/// is see-through. Nothing would show through such a window, so the blurred
/// desktop wallpaper is drawn underneath instead.
///
/// The system backdrops (Mica and Acrylic) were tried first. Mica is a tint
/// rather than a blur, and Acrylic drops to a flat colour whenever the window
/// loses focus, so both came out as a muddy brown over a warm wallpaper.
/// Drawing the blur ourselves looks the same whether or not the window is
/// focused.
///
/// A solid ground in the theme's own lightness is always drawn first. Without
/// it the see-through colours sat on the window's black clear colour whenever
/// there was no wallpaper to draw -- on every platform but Windows, before the
/// blur had loaded, or when it failed to -- and a light theme's dark text came
/// out on near-black panels.
pub(crate) fn render_backdrop(window: &Window, cx: &mut App) -> Option<AnyElement> {
    if !is_see_through(cx) {
        return None;
    }

    let dark = !cx.theme().appearance().is_light();
    let ground = if dark {
        gpui::hsla(0., 0., 0.09, 1.)
    } else {
        gpui::hsla(0., 0., 0.95, 1.)
    };
    let backdrop = div()
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .overflow_hidden()
        .bg(ground);

    let image = if cfg!(target_os = "windows") {
        backdrop_image(dark, cx)
    } else {
        None
    };
    let Some(image) = image else {
        return Some(backdrop.into_any_element());
    };

    // Sized and placed as the whole screen, so the window shows the part of
    // the wallpaper that is really behind it, like a pane of frosted glass.
    let window_bounds = window.bounds();
    let screen_bounds = window
        .display(cx)
        .map(|display| display.bounds())
        .unwrap_or(window_bounds);

    Some(
        backdrop
            .child(
                img(image)
                    .absolute()
                    .left(screen_bounds.origin.x - window_bounds.origin.x)
                    .top(screen_bounds.origin.y - window_bounds.origin.y)
                    .w(screen_bounds.size.width.max(px(1.)))
                    .h(screen_bounds.size.height.max(px(1.)))
                    .object_fit(ObjectFit::Cover),
            )
            .into_any_element(),
    )
}

fn is_see_through(cx: &App) -> bool {
    let theme = cx.theme();
    theme.window_background_appearance() == WindowBackgroundAppearance::Opaque
        && theme.colors().background.a < 1.0
}

/// Whether the active theme is drawn over the wallpaper.
pub(crate) fn is_active(cx: &App) -> bool {
    cfg!(target_os = "windows") && is_see_through(cx)
}

fn backdrop_image(dark: bool, cx: &mut App) -> Option<Arc<Path>> {
    let state = cx.default_global::<WallpaperBackdrop>();
    if state.image_is_dark != dark {
        // Switched between a light and a dark see-through theme. The blur
        // frosted for the other one would put this theme's text on the wrong
        // ground, so it is not drawn while the right one is made.
        state.image = None;
        state.image_is_dark = dark;
        state.checked_at = None;
    }
    let due = state
        .checked_at
        .is_none_or(|checked_at| checked_at.elapsed() >= RECHECK_AFTER);
    if state.loading || !due {
        return state.image.clone();
    }
    state.loading = true;
    state.checked_at = Some(std::time::Instant::now());
    let current = state.image.clone();
    let previous = current.clone();

    // Cheap when nothing changed: the blurred copy is keyed by the
    // wallpaper's modification time and found on disk.
    let task = cx
        .background_executor()
        .spawn(async move { blurred_wallpaper(dark) });
    cx.spawn(async move |cx| {
        let image = task.await.log_err().map(Arc::<Path>::from);
        cx.update(|cx| {
            let state = cx.default_global::<WallpaperBackdrop>();
            state.loading = false;
            if state.image_is_dark != dark {
                // The theme changed while this was being made; the next
                // frame asks for the right one.
                state.checked_at = None;
                cx.refresh_windows();
            } else if image.is_some() && image != previous {
                state.image = image;
                cx.refresh_windows();
            }
        });
    })
    .detach();
    current
}

fn wallpaper_path() -> Result<PathBuf> {
    // Windows keeps its own decoded copy of whatever the wallpaper is, which is
    // there even when the original file has moved or came from a slideshow.
    let app_data = std::env::var_os("APPDATA").context("APPDATA is not set")?;
    let path = PathBuf::from(app_data)
        .join("Microsoft")
        .join("Windows")
        .join("Themes")
        .join("TranscodedWallpaper");
    anyhow::ensure!(path.exists(), "no wallpaper at {}", path.display());
    Ok(path)
}

fn blurred_wallpaper(dark: bool) -> Result<PathBuf> {
    let source = wallpaper_path()?;
    let metadata = std::fs::metadata(&source)?;
    let modified = metadata
        .modified()?
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default();

    // Named after the wallpaper's timestamp and size, so a new wallpaper gets a
    // new file and the image cache, which is keyed by path, picks it up.
    let output = std::env::temp_dir().join(format!(
        "acuto-wallpaper-{modified}-{}-{}-v3.png",
        metadata.len(),
        if dark { "dark" } else { "light" },
    ));
    if output.exists() {
        return Ok(output);
    }

    let wallpaper = ImageReader::open(&source)?
        .with_guessed_format()?
        .decode()
        .with_context(|| format!("decoding {}", source.display()))?;
    let (width, height) = (wallpaper.width(), wallpaper.height());
    anyhow::ensure!(width > 0 && height > 0, "the wallpaper is empty");

    // Shrunk before it is widened to floating point: a 4K or 8K wallpaper at
    // twelve bytes a pixel is hundreds of megabytes for detail the blur is
    // about to throw away.
    let blur_height = scaled_height(width, height, BLUR_WIDTH);
    let small = wallpaper
        .resize_exact(BLUR_WIDTH, blur_height, imageops::FilterType::Triangle)
        .to_rgb32f();
    let blurred = imageops::blur(&small, BLUR_SIGMA);

    let output_height = scaled_height(width, height, OUTPUT_WIDTH);
    let large = imageops::resize(
        &blurred,
        OUTPUT_WIDTH,
        output_height,
        imageops::FilterType::CatmullRom,
    );

    let frosted = ImageBuffer::from_fn(OUTPUT_WIDTH, output_height, |x, y| {
        let Rgb([red, green, blue]) = *large.get_pixel(x, y);
        let luma = 0.2126 * red + 0.7152 * green + 0.0722 * blue;
        // Blurring and washing leaves long, very gentle gradients, which show as
        // bands once they are cut to 8 bits. Half a step of noise hides them.
        let noise = dither(x, y);
        let (lifted, colourfulness) = if dark {
            (
                DARK_LIGHTNESS_FLOOR + (DARK_LIGHTNESS_CEILING - DARK_LIGHTNESS_FLOOR) * luma,
                DARK_COLOURFULNESS,
            )
        } else {
            (
                LIGHTNESS_FLOOR + (1.0 - LIGHTNESS_FLOOR) * luma,
                COLOURFULNESS,
            )
        };
        let channel = |value: f32| {
            let frosted = lifted + (value - luma) * colourfulness;
            (frosted.clamp(0.0, 1.0) * 255.0 + noise)
                .round()
                .clamp(0.0, 255.0) as u8
        };
        Rgb([channel(red), channel(green), channel(blue)])
    });

    // Written under a name of its own, so two windows starting at once cannot
    // interleave their writes, then moved into place whole.
    let partial = output.with_extension(format!("{}.partial", std::process::id()));
    frosted.save_with_format(&partial, image::ImageFormat::Png)?;
    std::fs::rename(&partial, &output)?;
    remove_stale_backdrops(&output);
    Ok(output)
}

/// Deletes the blurs of earlier wallpapers. Each wallpaper gets its own file,
/// so with a slideshow they would otherwise pile up in the temp directory.
fn remove_stale_backdrops(current: &Path) {
    let Some(directory) = current.parent() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_backdrop = path
            .file_name()
            .and_then(|name| name.to_str())
            // Another window may be midway through writing its own.
            .is_some_and(|name| {
                name.starts_with("acuto-wallpaper-") && !name.ends_with(".partial")
            });
        if is_backdrop && path != current {
            std::fs::remove_file(&path).log_err();
        }
    }
}

/// The height that keeps `width`x`height` in proportion at `target_width`.
///
/// Capped at four times the width: a corrupt or absurdly tall image would
/// otherwise ask for a buffer as tall as it likes.
fn scaled_height(width: u32, height: u32, target_width: u32) -> u32 {
    let scaled = (height as f64 * target_width as f64 / width as f64).round();
    (scaled.min(target_width as f64 * 4.0) as u32).max(1)
}

fn dither(x: u32, y: u32) -> f32 {
    // The MurmurHash3 finaliser: every output bit depends on every input bit,
    // so the noise has no rows or columns to line up into visible streaks.
    let mut hash = x ^ y.rotate_left(16) ^ 0x9E37_79B9;
    hash ^= hash >> 16;
    hash = hash.wrapping_mul(0x85EB_CA6B);
    hash ^= hash >> 13;
    hash = hash.wrapping_mul(0xC2B2_AE35);
    hash ^= hash >> 16;
    (hash & 0xFFFF) as f32 / 65535.0 - 0.5
}
