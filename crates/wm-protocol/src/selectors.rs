// Modified by kdnk for wm on 2026-11-14. Original attribution is preserved in LICENSE.
use serde::{Deserialize, Serialize};

use crate::Direction;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum WorkspaceSelector {
    Index(usize),
    Name(String),
}

#[derive(
    Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, wm_config_derive::ConfigEnum,
)]
#[serde(rename_all = "snake_case")]
pub enum RestoreScope {
    Workspace,
    Space,
    /// The current Space of every connected display.
    Displays,
}

#[derive(
    Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize, wm_config_derive::ConfigEnum,
)]
#[serde(rename_all = "snake_case")]
pub enum RestoreSource {
    #[default]
    SavedActiveSpace,
    CurrentSpace,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DisplaySelector {
    Direction(Direction),
    Index(usize),
    Uuid(String),
}
