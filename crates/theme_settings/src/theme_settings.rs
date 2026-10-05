#![deny(missing_docs)]

//! # Theme Settings
//!
//! This crate provides theme settings integration for Zed,
//! bridging the theme system with the settings infrastructure.

mod schema;
mod settings;
mod token_theme;

use std::path::Path;
use std::sync::Arc;

use ::settings::{IntoGpui, Settings, SettingsStore};
use anyhow::{Context as _, Result};
use gpui::{App, Font, HighlightStyle, Pixels, Refineable, px};
use gpui_util::ResultExt;
use theme::{
    AccentColors, Appearance, AppearanceContent, DEFAULT_DARK_THEME, DEFAULT_ICON_THEME_NAME,
    GlobalTheme, LoadThemes, PlayerColor, PlayerColors, StatusColors, SyntaxTheme,
    SystemAppearance, SystemColors, Theme, ThemeColors, ThemeFamily, ThemeRegistry,
    ThemeSettingsProvider, ThemeStyles, default_color_scales, try_parse_color,
};

pub use crate::schema::{
    FontStyleContent, FontWeightContent, HighlightStyleContent, StatusColorsContent,
    ThemeColorsContent, ThemeContent, ThemeFamilyContent, ThemeStyleContent,
    WindowBackgroundContent, status_colors_refinement, syntax_overrides, theme_colors_refinement,
};
use crate::settings::adjust_buffer_font_size;
pub use crate::settings::{
    AgentBufferFontSize, AgentUiFontSize, BufferLineHeight, FontFamilyName,
    GitCommitBufferFontSize, IconThemeName, IconThemeSelection, MarkdownPreviewFontSize,
    ThemeAppearanceMode, ThemeName, ThemeSelection, ThemeSettings, adjust_agent_buffer_font_size,
    adjust_agent_ui_font_size, adjust_git_commit_buffer_font_size,
    adjust_markdown_preview_font_size, adjust_ui_font_size, adjusted_font_size, appearance_to_mode,
    buffer_line_height_from_settings, clamp_font_size, default_theme,
    observe_buffer_font_size_adjustment, reset_agent_buffer_font_size, reset_agent_ui_font_size,
    reset_buffer_font_size, reset_git_commit_buffer_font_size, reset_markdown_preview_font_size,
    reset_ui_font_size, set_icon_theme, set_mode, set_theme, setup_ui_font,
};
pub use theme::UiDensity;

struct ThemeSettingsProviderImpl;

impl ThemeSettingsProvider for ThemeSettingsProviderImpl {
    fn ui_font<'a>(&'a self, cx: &'a App) -> &'a Font {
        &ThemeSettings::get_global(cx).ui_font
    }

    fn buffer_font<'a>(&'a self, cx: &'a App) -> &'a Font {
        &ThemeSettings::get_global(cx).buffer_font
    }

    fn ui_font_size(&self, cx: &App) -> Pixels {
        ThemeSettings::get_global(cx).ui_font_size(cx)
    }

    fn buffer_font_size(&self, cx: &App) -> Pixels {
        ThemeSettings::get_global(cx).buffer_font_size(cx)
    }

    fn ui_density(&self, cx: &App) -> UiDensity {
        ThemeSettings::get_global(cx).ui_density
    }
}

/// Initialize the theme system with settings integration.
///
/// This is the full initialization for the application. It calls [`theme::init`]
/// and then wires up settings observation for theme/font changes.
pub fn init(themes_to_load: LoadThemes, cx: &mut App) {
    let load_user_themes = matches!(&themes_to_load, LoadThemes::All(_));

    theme::init(themes_to_load, cx);
    theme::set_theme_settings_provider(Box::new(ThemeSettingsProviderImpl), cx);

    if load_user_themes {
        let registry = ThemeRegistry::global(cx);
        load_bundled_themes(&registry);
        load_bundled_icon_themes(&registry);
    }

    let theme = configured_theme(cx);
    let icon_theme = configured_icon_theme(cx);
    GlobalTheme::update_theme(cx, theme);
    GlobalTheme::update_icon_theme(cx, icon_theme);

    let settings = ThemeSettings::get_global(cx);

    let mut prev_buffer_font_size_settings = settings.buffer_font_size_settings();
    let mut prev_ui_font_size_settings = settings.ui_font_size_settings();
    let mut prev_agent_ui_font_size_settings = settings.agent_ui_font_size_settings();
    let mut prev_agent_buffer_font_size_settings = settings.agent_buffer_font_size_settings();
    let mut prev_git_commit_buffer_font_size_settings =
        settings.git_commit_buffer_font_size_settings();
    let mut prev_markdown_preview_font_size_settings =
        settings.markdown_preview_font_size_settings();
    let mut prev_theme_name = settings.theme.name(SystemAppearance::global(cx).0);
    let mut prev_icon_theme_name = settings.icon_theme.name(SystemAppearance::global(cx).0);
    let mut prev_theme_overrides = (
        settings.experimental_theme_overrides.clone(),
        settings.theme_overrides.clone(),
    );

    cx.observe_global::<SettingsStore>(move |cx| {
        let settings = ThemeSettings::get_global(cx);

        let buffer_font_size_settings = settings.buffer_font_size_settings();
        let ui_font_size_settings = settings.ui_font_size_settings();
        let agent_ui_font_size_settings = settings.agent_ui_font_size_settings();
        let agent_buffer_font_size_settings = settings.agent_buffer_font_size_settings();
        let git_commit_buffer_font_size_settings = settings.git_commit_buffer_font_size_settings();
        let markdown_preview_font_size_settings = settings.markdown_preview_font_size_settings();
        let theme_name = settings.theme.name(SystemAppearance::global(cx).0);
        let icon_theme_name = settings.icon_theme.name(SystemAppearance::global(cx).0);
        let theme_overrides = (
            settings.experimental_theme_overrides.clone(),
            settings.theme_overrides.clone(),
        );

        if buffer_font_size_settings != prev_buffer_font_size_settings {
            prev_buffer_font_size_settings = buffer_font_size_settings;
            reset_buffer_font_size(cx);
        }

        if ui_font_size_settings != prev_ui_font_size_settings {
            prev_ui_font_size_settings = ui_font_size_settings;
            reset_ui_font_size(cx);
        }

        if agent_ui_font_size_settings != prev_agent_ui_font_size_settings {
            prev_agent_ui_font_size_settings = agent_ui_font_size_settings;
            reset_agent_ui_font_size(cx);
        }

        if agent_buffer_font_size_settings != prev_agent_buffer_font_size_settings {
            prev_agent_buffer_font_size_settings = agent_buffer_font_size_settings;
            reset_agent_buffer_font_size(cx);
        }

        if git_commit_buffer_font_size_settings != prev_git_commit_buffer_font_size_settings {
            prev_git_commit_buffer_font_size_settings = git_commit_buffer_font_size_settings;
            reset_git_commit_buffer_font_size(cx);
        }

        if markdown_preview_font_size_settings != prev_markdown_preview_font_size_settings {
            prev_markdown_preview_font_size_settings = markdown_preview_font_size_settings;
            reset_markdown_preview_font_size(cx);
        }

        if theme_name != prev_theme_name || theme_overrides != prev_theme_overrides {
            prev_theme_name = theme_name;
            prev_theme_overrides = theme_overrides;
            reload_theme(cx);
        }

        if icon_theme_name != prev_icon_theme_name {
            prev_icon_theme_name = icon_theme_name;
            reload_icon_theme(cx);
        }
    })
    .detach();
}

fn configured_theme(cx: &mut App) -> Arc<Theme> {
    let themes = ThemeRegistry::default_global(cx);
    let theme_settings = ThemeSettings::get_global(cx);
    let system_appearance = SystemAppearance::global(cx);

    let theme_name = theme_settings.theme.name(*system_appearance);

    let theme = match themes.get(&theme_name.0) {
        Ok(theme) => theme,
        Err(err) => {
            if themes.extensions_loaded() {
                log::error!("{err}");
            }
            themes
                .get(default_theme(*system_appearance))
                .unwrap_or_else(|_| themes.get(DEFAULT_DARK_THEME).unwrap())
        }
    };
    theme_settings.apply_theme_overrides(theme)
}

fn configured_icon_theme(cx: &mut App) -> Arc<theme::IconTheme> {
    let themes = ThemeRegistry::default_global(cx);
    let theme_settings = ThemeSettings::get_global(cx);
    let system_appearance = SystemAppearance::global(cx);

    let icon_theme_name = theme_settings.icon_theme.name(*system_appearance);

    match themes.get_icon_theme(&icon_theme_name.0) {
        Ok(theme) => theme,
        Err(err) => {
            if themes.extensions_loaded() {
                log::error!("{err}");
            }
            themes.get_icon_theme(DEFAULT_ICON_THEME_NAME).unwrap()
        }
    }
}

/// Reloads the current theme from settings.
pub fn reload_theme(cx: &mut App) {
    let theme = configured_theme(cx);
    GlobalTheme::update_theme(cx, theme);
    cx.refresh_windows();
}

/// Reloads the current icon theme from settings.
pub fn reload_icon_theme(cx: &mut App) {
    let icon_theme = configured_icon_theme(cx);
    GlobalTheme::update_icon_theme(cx, icon_theme);
    cx.refresh_windows();
}

/// Loads the icon themes bundled with the editor into the registry.
///
/// The same shape as the colour themes above: a directory of JSON in the
/// assets, each file describing one family. Until this existed an icon theme
/// could only arrive through an extension, so a set shipped with the editor had
/// nowhere to declare itself.
///
/// Icon paths inside these files are already written relative to the assets
/// root, so they resolve against an empty prefix rather than an extension's
/// working directory.
pub fn load_bundled_icon_themes(registry: &ThemeRegistry) {
    let Some(paths) = registry.assets().list("icon_themes/").log_err() else {
        return;
    };

    for path in paths.into_iter().filter(|path| path.ends_with(".json")) {
        let Some(contents) = registry.assets().load(&path).log_err().flatten() else {
            continue;
        };

        let Some(family) = serde_json::from_slice(&contents)
            .with_context(|| format!("failed to parse icon theme at path \"{path}\""))
            .log_err()
        else {
            continue;
        };

        registry
            .load_icon_theme(family, Path::new(""))
            .with_context(|| format!("failed to load icon theme at path \"{path}\""))
            .log_err();
    }
}

/// Loads the themes bundled with the Zed binary into the registry.
pub fn load_bundled_themes(registry: &ThemeRegistry) {
    let theme_paths = registry
        .assets()
        .list("themes/")
        .expect("failed to list theme assets")
        .into_iter()
        .filter(|path| path.ends_with(".json"));

    for path in theme_paths {
        let Some(theme) = registry.assets().load(&path).log_err().flatten() else {
            continue;
        };

        let Some(theme_family) = serde_json::from_slice(&theme)
            .with_context(|| format!("failed to parse theme at path \"{path}\""))
            .log_err()
        else {
            continue;
        };

        let refined = refine_theme_family(theme_family);
        registry.insert_theme_families([refined]);
    }
}

/// Loads a user theme from the given bytes into the registry.
pub fn load_user_theme(registry: &ThemeRegistry, bytes: &[u8]) -> Result<()> {
    let theme = deserialize_user_theme(bytes)?;
    let refined = refine_theme_family(theme);
    registry.insert_theme_families([refined]);
    Ok(())
}

/// Deserializes a user theme from the given bytes.
pub fn deserialize_user_theme(bytes: &[u8]) -> Result<ThemeFamilyContent> {
    let theme_family: ThemeFamilyContent = serde_json_lenient::from_slice(bytes)?;

    for theme in &theme_family.themes {
        if theme
            .style
            .colors
            .deprecated_scrollbar_thumb_background
            .is_some()
        {
            log::warn!(
                r#"Theme "{theme_name}" is using a deprecated style property: scrollbar_thumb.background. Use `scrollbar.thumb.background` instead."#,
                theme_name = theme.name
            )
        }
    }

    Ok(theme_family)
}

/// Refines a [`ThemeFamilyContent`] and its [`ThemeContent`]s into a [`ThemeFamily`].
pub fn refine_theme_family(theme_family_content: ThemeFamilyContent) -> ThemeFamily {
    let id = uuid::Uuid::new_v4().to_string();
    let name = theme_family_content.name.clone();
    let author = theme_family_content.author.clone();

    let themes: Vec<Theme> = theme_family_content
        .themes
        .iter()
        .map(|theme_content| refine_theme(theme_content))
        .collect();

    ThemeFamily {
        id,
        name: name.into(),
        author: author.into(),
        themes,
        scales: default_color_scales(),
    }
}

/// Refines a [`ThemeContent`] into a [`Theme`].
pub fn refine_theme(theme: &ThemeContent) -> Theme {
    let appearance = match theme.appearance {
        AppearanceContent::Light => Appearance::Light,
        AppearanceContent::Dark => Appearance::Dark,
    };

    let mut refined_status_colors = match theme.appearance {
        AppearanceContent::Light => StatusColors::light(),
        AppearanceContent::Dark => StatusColors::dark(),
    };
    let mut status_colors_refinement = status_colors_refinement(&theme.style.status);
    theme::apply_status_color_defaults(&mut status_colors_refinement);
    refined_status_colors.refine(&status_colors_refinement);

    let mut refined_player_colors = match theme.appearance {
        AppearanceContent::Light => PlayerColors::light(),
        AppearanceContent::Dark => PlayerColors::dark(),
    };
    merge_player_colors(&mut refined_player_colors, &theme.style.players);

    let mut refined_theme_colors = match theme.appearance {
        AppearanceContent::Light => ThemeColors::light(),
        AppearanceContent::Dark => ThemeColors::dark(),
    };
    let mut theme_colors_refinement = theme_colors_refinement(
        &theme.style.colors,
        &status_colors_refinement,
        theme.appearance == AppearanceContent::Light,
    );
    theme::apply_theme_color_defaults(&mut theme_colors_refinement, &refined_player_colors);
    refined_theme_colors.refine(&theme_colors_refinement);

    let mut refined_accent_colors = match theme.appearance {
        AppearanceContent::Light => AccentColors::light(),
        AppearanceContent::Dark => AccentColors::dark(),
    };
    merge_accent_colors(&mut refined_accent_colors, &theme.style.accents);

    let syntax_highlights = theme.style.syntax.iter().map(|(syntax_token, highlight)| {
        (
            syntax_token.clone(),
            HighlightStyle {
                color: highlight
                    .color
                    .as_ref()
                    .and_then(|color| try_parse_color(color).ok()),
                background_color: highlight
                    .background_color
                    .as_ref()
                    .and_then(|color| try_parse_color(color).ok()),
                font_style: highlight.font_style.map(|s| s.into_gpui()),
                font_weight: highlight.font_weight.map(|w| w.into_gpui()),
                ..Default::default()
            },
        )
    });
    let syntax_theme = Arc::new(SyntaxTheme::new(syntax_highlights));

    let window_background_appearance = theme
        .style
        .window_background_appearance
        .map(|w| w.into_gpui())
        .unwrap_or_default();

    Theme {
        id: uuid::Uuid::new_v4().to_string(),
        name: theme.name.clone().into(),
        appearance,
        styles: ThemeStyles {
            system: SystemColors::default(),
            window_background_appearance,
            accents: refined_accent_colors,
            colors: refined_theme_colors,
            status: refined_status_colors,
            player: refined_player_colors,
            syntax: syntax_theme,
        },
    }
}

/// Merges player color overrides into the given [`PlayerColors`].
pub fn merge_player_colors(
    player_colors: &mut PlayerColors,
    user_player_colors: &[::settings::PlayerColorContent],
) {
    if user_player_colors.is_empty() {
        return;
    }

    for (idx, player) in user_player_colors.iter().enumerate() {
        let cursor = player
            .cursor
            .as_ref()
            .and_then(|color| try_parse_color(color).ok());
        let background = player
            .background
            .as_ref()
            .and_then(|color| try_parse_color(color).ok());
        let selection = player
            .selection
            .as_ref()
            .and_then(|color| try_parse_color(color).ok());

        if let Some(player_color) = player_colors.0.get_mut(idx) {
            *player_color = PlayerColor {
                cursor: cursor.unwrap_or(player_color.cursor),
                background: background.unwrap_or(player_color.background),
                selection: selection.unwrap_or(player_color.selection),
            };
        } else {
            player_colors.0.push(PlayerColor {
                cursor: cursor.unwrap_or_default(),
                background: background.unwrap_or_default(),
                selection: selection.unwrap_or_default(),
            });
        }
    }
}

/// Merges accent color overrides into the given [`AccentColors`].
pub fn merge_accent_colors(
    accent_colors: &mut AccentColors,
    user_accent_colors: &[::settings::AccentContent],
) {
    if user_accent_colors.is_empty() {
        return;
    }

    let colors = user_accent_colors
        .iter()
        .filter_map(|accent_color| {
            accent_color
                .0
                .as_ref()
                .and_then(|color| try_parse_color(color).ok())
        })
        .collect::<Vec<_>>();

    if !colors.is_empty() {
        accent_colors.0 = Arc::from(colors);
    }
}

/// Increases the buffer font size by 1 pixel, without persisting the result in the settings.
/// This will be effective until the app is restarted.
pub fn increase_buffer_font_size(cx: &mut App) {
    adjust_buffer_font_size(cx, |size| size + px(1.0));
}

/// Decreases the buffer font size by 1 pixel, without persisting the result in the settings.
/// This will be effective until the app is restarted.
pub fn decrease_buffer_font_size(cx: &mut App) {
    adjust_buffer_font_size(cx, |size| size - px(1.0));
}

#[cfg(test)]
mod bundled_theme_contrast {
    use gpui::{Hsla, Rgba, hsla};

    use super::*;

    /// WCAG contrast ratio between two opaque colours.
    fn contrast(foreground: Hsla, background: Hsla) -> f32 {
        fn luminance(color: Hsla) -> f32 {
            let Rgba { r, g, b, .. } = color.to_rgb();
            let channel = |value: f32| {
                if value <= 0.03928 {
                    value / 12.92
                } else {
                    ((value + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
        }
        let (a, b) = (luminance(foreground), luminance(background));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    /// Every bundled Acuto theme keeps its text, icons and status colours
    /// legible on every surface they are drawn on, including selected and
    /// hovered rows.
    ///
    /// A see-through theme is measured over the darkest ground it can sit on:
    /// the frosted wallpaper is never darker than 74% white for a light theme,
    /// and the backdrop of a dark one is never lighter than 20%.
    #[test]
    fn acuto_themes_are_legible() {
        let family: ThemeFamilyContent =
            serde_json::from_str(include_str!("../../../assets/themes/acuto/acuto.json"))
                .expect("the bundled Acuto themes parse");

        let mut failures = Vec::new();
        for content in &family.themes {
            let theme = refine_theme(content);
            let colors = theme.colors();
            let status = theme.status();
            let ground = if theme.appearance.is_light() {
                hsla(0., 0., 0.74, 1.)
            } else {
                hsla(0., 0., 0.2, 1.)
            };
            let window = ground.blend(colors.background);

            let planes = [
                ("panel", window.blend(colors.panel_background)),
                ("editor", window.blend(colors.editor_background)),
                (
                    "elevated surface",
                    window.blend(colors.elevated_surface_background),
                ),
                ("title bar", window.blend(colors.title_bar_background)),
                ("status bar", window.blend(colors.status_bar_background)),
                ("tab bar", window.blend(colors.tab_bar_background)),
            ];
            let mut surfaces: Vec<(String, Hsla)> = planes
                .iter()
                .map(|(name, color)| (name.to_string(), *color))
                .collect();
            for (row, overlay) in [
                ("hovered", colors.element_hover),
                ("selected", colors.element_selected),
                ("ghost selected", colors.ghost_element_selected),
            ] {
                for (plane, base) in planes {
                    surfaces.push((format!("{row} row on {plane}"), base.blend(overlay)));
                }
            }

            let foregrounds = [
                ("text", colors.text, 4.5),
                ("muted text", colors.text_muted, 3.0),
                ("icon", colors.icon, 3.0),
                ("muted icon", colors.icon_muted, 2.2),
                ("placeholder", colors.text_placeholder, 2.0),
                ("error", status.error, 2.9),
                ("warning", status.warning, 2.9),
                ("success", status.success, 2.9),
                ("modified", status.modified, 2.9),
                ("created", status.created, 2.9),
                ("deleted", status.deleted, 2.9),
            ];
            for (surface_name, surface) in &surfaces {
                for (name, color, needed) in foregrounds {
                    let ratio = contrast(surface.blend(color), *surface);
                    if ratio < needed {
                        failures.push(format!(
                            "{}: {name} on {surface_name} is {ratio:.2}:1, needs {needed}:1",
                            theme.name
                        ));
                    }
                }
            }

            let terminal = window.blend(colors.terminal_background);
            for (name, color, needed) in [
                ("terminal text", colors.terminal_foreground, 4.5),
                ("terminal black", colors.terminal_ansi_black, 1.6),
                ("terminal white", colors.terminal_ansi_white, 2.4),
                (
                    "terminal bright white",
                    colors.terminal_ansi_bright_white,
                    1.6,
                ),
            ] {
                let ratio = contrast(terminal.blend(color), terminal);
                if ratio < needed {
                    failures.push(format!(
                        "{}: {name} is {ratio:.2}:1, needs {needed}:1",
                        theme.name
                    ));
                }
            }
        }

        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }
}
