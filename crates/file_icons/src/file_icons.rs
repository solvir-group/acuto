use std::sync::Arc;
use std::{path::Path, str};

use gpui::{App, Hsla, SharedString, hsla};
use theme::{GlobalTheme, IconTheme, ThemeRegistry};
use util::paths::PathExt;

#[derive(Debug)]
pub struct FileIcons {
    icon_theme: Arc<IconTheme>,
}

impl FileIcons {
    pub fn get(cx: &App) -> Self {
        Self {
            icon_theme: GlobalTheme::icon_theme(cx).clone(),
        }
    }

    pub fn get_icon(path: &Path, cx: &App) -> Option<SharedString> {
        let this = Self::get(cx);

        let get_icon_from_suffix = |suffix: &str| -> Option<SharedString> {
            this.icon_theme
                .file_stems
                .get(suffix)
                .or_else(|| this.icon_theme.file_suffixes.get(suffix))
                .and_then(|typ| this.get_icon_for_type(typ, cx))
        };
        // TODO: Associate a type with the languages and have the file's language
        //       override these associations

        if let Some(mut typ) = path.file_name().and_then(|typ| typ.to_str()) {
            // check if file name is in suffixes
            // e.g. catch file named `eslint.config.js` instead of `.eslint.config.js`
            let maybe_path = get_icon_from_suffix(typ);
            if maybe_path.is_some() {
                return maybe_path;
            }

            // check if suffix based on first dot is in suffixes
            // e.g. consider `module.js` as suffix to angular's module file named `auth.module.js`
            while let Some((_, suffix)) = typ.split_once('.') {
                let maybe_path = get_icon_from_suffix(suffix);
                if maybe_path.is_some() {
                    return maybe_path;
                }
                typ = suffix;
            }
        }

        // handle cases where the file extension is made up of multiple important
        // parts (e.g Component.stories.tsx) that refer to an alternative icon style
        if let Some(suffix) = path.multiple_extensions() {
            let maybe_path = get_icon_from_suffix(suffix.as_str());
            if maybe_path.is_some() {
                return maybe_path;
            }
        }

        // primary case: check if the files extension or the hidden file name
        // matches some icon path
        if let Some(suffix) = path.extension_or_hidden_file_name() {
            let maybe_path = get_icon_from_suffix(suffix);
            if maybe_path.is_some() {
                return maybe_path;
            }
        }

        // this _should_ only happen when the file is hidden (has leading '.')
        // and is not a "special" file we have an icon (e.g. not `.eslint.config.js`)
        // that should be caught above. In the remaining cases, we want to check
        // for a normal supported extension e.g. `.data.json` -> `json`
        let extension = path.extension().and_then(|ext| ext.to_str());
        if let Some(extension) = extension {
            let maybe_path = get_icon_from_suffix(extension);
            if maybe_path.is_some() {
                return maybe_path;
            }
        }
        this.get_icon_for_type("default", cx)
    }

    fn default_icon_theme(cx: &App) -> Option<Arc<IconTheme>> {
        let theme_registry = ThemeRegistry::global(cx);
        theme_registry.default_icon_theme().ok()
    }

    pub fn get_icon_for_type(&self, typ: &str, cx: &App) -> Option<SharedString> {
        fn get_icon_for_type(icon_theme: &Arc<IconTheme>, typ: &str) -> Option<SharedString> {
            icon_theme
                .file_icons
                .get(typ)
                .map(|icon_definition| icon_definition.path.clone())
        }

        get_icon_for_type(GlobalTheme::icon_theme(cx), typ).or_else(|| {
            Self::default_icon_theme(cx).and_then(|icon_theme| get_icon_for_type(&icon_theme, typ))
        })
    }

    pub fn get_folder_icon(expanded: bool, path: &Path, cx: &App) -> Option<SharedString> {
        fn get_folder_icon(
            icon_theme: &Arc<IconTheme>,
            path: &Path,
            expanded: bool,
        ) -> Option<SharedString> {
            let name = path.file_name()?.to_str()?.trim();
            if name.is_empty() {
                return None;
            }

            let directory_icons = icon_theme.named_directory_icons.get(name)?;

            if expanded {
                directory_icons.expanded.clone()
            } else {
                directory_icons.collapsed.clone()
            }
        }

        get_folder_icon(GlobalTheme::icon_theme(cx), path, expanded)
            .or_else(|| {
                Self::default_icon_theme(cx)
                    .and_then(|icon_theme| get_folder_icon(&icon_theme, path, expanded))
            })
            .or_else(|| {
                // If we can't find a specific folder icon for the folder at the given path, fall back to the generic folder
                // icon.
                Self::get_generic_folder_icon(expanded, cx)
            })
    }

    fn get_generic_folder_icon(expanded: bool, cx: &App) -> Option<SharedString> {
        fn get_generic_folder_icon(
            icon_theme: &Arc<IconTheme>,
            expanded: bool,
        ) -> Option<SharedString> {
            if expanded {
                icon_theme.directory_icons.expanded.clone()
            } else {
                icon_theme.directory_icons.collapsed.clone()
            }
        }

        get_generic_folder_icon(GlobalTheme::icon_theme(cx), expanded).or_else(|| {
            Self::default_icon_theme(cx)
                .and_then(|icon_theme| get_generic_folder_icon(&icon_theme, expanded))
        })
    }

    pub fn get_chevron_icon(expanded: bool, cx: &App) -> Option<SharedString> {
        fn get_chevron_icon(icon_theme: &Arc<IconTheme>, expanded: bool) -> Option<SharedString> {
            if expanded {
                icon_theme.chevron_icons.expanded.clone()
            } else {
                icon_theme.chevron_icons.collapsed.clone()
            }
        }

        get_chevron_icon(GlobalTheme::icon_theme(cx), expanded).or_else(|| {
            Self::default_icon_theme(cx)
                .and_then(|icon_theme| get_chevron_icon(&icon_theme, expanded))
        })
    }
}

/// The colour a file's icon is drawn in, by language family.
///
/// Six hues, not twenty. A file tree is read by scanning it, and scanning works
/// on a small vocabulary of well-separated colours -- one per family of related
/// things -- rather than on a unique colour per extension that nobody can hold
/// in their head. Saturation is kept low for the same reason: the tree should
/// read as filenames with a hint of colour, not as a column of stickers.
///
/// `None` means the file takes whatever muted colour the surface uses for
/// everything else, which is better than inventing a hue for a file type nobody
/// associates with one.
pub fn icon_color(path: &Path, cx: &App) -> Option<Hsla> {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();

    let family = match extension.as_str() {
        // Code. One hue for everything you write logic in, because the tree
        // already tells you which language by the glyph -- the colour is there
        // to separate code from everything around it.
        "ts" | "tsx" | "mts" | "cts" | "js" | "jsx" | "mjs" | "cjs" | "rs" | "go" | "py"
        | "pyi" | "rb" | "erb" | "java" | "kt" | "kts" | "scala" | "swift" | "c" | "h" | "cc"
        | "cpp" | "hpp" | "cxx" | "cs" | "fs" | "fsx" | "php" | "vue" | "svelte" | "elm"
        | "lua" | "zig" | "nim" | "dart" | "ex" | "exs" | "erl" | "hs" | "ml" | "clj" | "cljs"
        | "scm" | "lisp" => Family::Code,

        // Markup and style: the layer you look at, rather than the layer that
        // runs.
        "html" | "htm" | "xml" | "svg" | "astro" | "css" | "scss" | "sass" | "less" | "pcss"
        | "postcss" | "styl" => Family::Markup,

        // Configuration and data. The files you edit to change behaviour
        // without writing any.
        "json" | "jsonc" | "json5" | "toml" | "yaml" | "yml" | "ini" | "cfg" | "conf" | "env"
        | "properties" | "lock" | "sum" | "sql" | "db" | "sqlite" => Family::Config,

        // Anything that runs as a command.
        "sh" | "bash" | "zsh" | "fish" | "ps1" | "bat" | "cmd" => Family::Shell,

        // Prose.
        "md" | "mdx" | "rst" | "adoc" | "txt" | "pdf" | "doc" | "docx" => Family::Document,

        // Things that are not text at all.
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "avif" | "ico" | "bmp" | "mp3" | "wav"
        | "flac" | "ogg" | "mp4" | "mov" | "webm" | "mkv" | "zip" | "tar" | "gz" | "bz2"
        | "xz" | "7z" | "rar" => Family::Asset,

        _ => {
            // Extensionless files that everyone recognises anyway: Dockerfile
            // and Makefile carry as much meaning as any suffix does.
            let stem = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase();
            match stem.as_str() {
                "dockerfile" | "containerfile" => Family::Config,
                "makefile" | "justfile" | "rakefile" => Family::Shell,
                "license" | "licence" | "copying" => Family::Document,
                _ => return None,
            }
        }
    };

    // Dark themes need the icons a little brighter than their text and light
    // themes a little darker, or the colour reads as a smudge either way. The
    // saturation stays well under half in both: these sit beside text, and a
    // saturated dot beside a word pulls the eye off the word.
    let (saturation, lightness) = if GlobalTheme::theme(cx).appearance().is_light() {
        (0.42, 0.44)
    } else {
        (0.40, 0.66)
    };

    Some(hsla(family.hue(), saturation, lightness, 1.0))
}

/// The families a file icon can belong to.
#[derive(Clone, Copy)]
enum Family {
    Code,
    Markup,
    Config,
    Shell,
    Document,
    Asset,
}

impl Family {
    /// Hues chosen to be distinguishable from each other at a glance, and to
    /// survive the common forms of colour blindness: no red/green pair carries
    /// a distinction on its own, and the two warm families differ in lightness
    /// as well as hue.
    fn hue(self) -> f32 {
        match self {
            Family::Code => 0.575,     // blue
            Family::Markup => 0.078,   // terracotta
            Family::Config => 0.125,   // ochre
            Family::Shell => 0.385,    // green
            Family::Document => 0.60,  // slate blue
            Family::Asset => 0.79,     // violet
        }
    }
}
