#![allow(missing_docs)]

use gpui::SharedString;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::AppearanceContent;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct IconThemeFamilyContent {
    pub name: String,
    pub author: String,
    pub themes: Vec<IconThemeContent>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct IconThemeContent {
    pub name: String,
    pub appearance: AppearanceContent,
    /// Whether this theme's icons carry their own colour.
    ///
    /// Monochrome icons are drawn as a silhouette tinted by the editor, which
    /// keeps them legible on any background. Icons with real artwork in them
    /// have to be drawn as-is, or every one of them comes out a flat block of
    /// the same colour.
    #[serde(default)]
    pub colored: bool,
    #[serde(default)]
    pub directory_icons: DirectoryIconsContent,
    #[serde(default)]
    pub named_directory_icons: HashMap<String, DirectoryIconsContent>,
    #[serde(default)]
    pub chevron_icons: ChevronIconsContent,
    #[serde(default)]
    pub file_stems: HashMap<String, String>,
    #[serde(default)]
    pub file_suffixes: HashMap<String, String>,
    #[serde(default)]
    pub file_icons: HashMap<String, IconDefinitionContent>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct DirectoryIconsContent {
    pub collapsed: Option<SharedString>,
    pub expanded: Option<SharedString>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ChevronIconsContent {
    pub collapsed: Option<SharedString>,
    pub expanded: Option<SharedString>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct IconDefinitionContent {
    pub path: SharedString,
}
