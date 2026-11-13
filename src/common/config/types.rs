// Modified by kdnk for wm on 2026-11-14. Original attribution is preserved in LICENSE.
use std::collections::BTreeMap;
use std::path::PathBuf;

use regex::RegexBuilder;
use serde::{Deserialize, Serialize};
use serde_with::{DeserializeFromStr, SerializeDisplay};
pub use wm_protocol::{AnimationEasing, ConfigCommand, LayoutMode, WorkspaceSelector};

use crate::actor::wm_controller::WmCommand;
use crate::common::collections::HashMap;
use crate::sys::hotkey::{Hotkey, HotkeySpec, Modifiers, modifiers_from_str};

pub const MAX_WORKSPACES: usize = 128;

// TODO: when to remove these?
pub(super) const DEPRECATED_MAP: &[(&str, &str)] = &[
    ("stack_windows", "toggle_stack"),
    ("unstack_windows", "toggle_stack"),
];

pub fn data_dir() -> PathBuf { dirs::home_dir().unwrap().join(".wm") }
pub fn restore_file() -> PathBuf { data_dir().join("layout.ron") }
pub fn config_file() -> PathBuf {
    dirs::home_dir().unwrap().join(".config").join("wm").join("config.toml")
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Copy, Default, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceMode {
    /// Each macOS desktop has its own set of workspaces, numbered independently.
    PerDisplay,
    /// One set of workspaces across all displays. Each workspace belongs to one display at a time.
    #[default]
    Global,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[serde(deny_unknown_fields)]
pub struct VirtualWorkspaceSettings {
    #[serde(default)]
    /// Each display has its own workspaces, numbered from 1. When off, workspaces are numbered
    /// across all displays, and each one stays on a single display.
    pub mode: WorkspaceMode,
    #[serde(default = "yes")]
    /// Organize windows into separate virtual workspaces.
    pub enabled: bool,
    #[serde(default = "default_workspace_count")]
    /// Number of virtual workspaces.
    pub default_workspace_count: usize,
    #[serde(default = "yes")]
    /// Automatically assign new windows to a workspace.
    pub auto_assign_windows: bool,
    #[serde(default = "yes")]
    /// Remember the focused window in each workspace.
    pub preserve_focus_per_workspace: bool,
    #[serde(default)]
    /// Return to the previous workspace when the active workspace is selected again.
    pub workspace_auto_back_and_forth: bool,
    #[serde(default, alias = "prevent_wrapping_around")]
    /// Stop workspace navigation at the first and last workspace.
    pub prevent_wrapping: bool,
    #[serde(default = "default_workspace_names")]
    /// Names assigned to virtual workspaces.
    pub workspace_names: Vec<String>,
    #[serde(default)]
    /// Workspace used for new windows without another assignment.
    pub default_workspace: usize,
    #[serde(default)]
    /// Recheck workspace rules when a window title changes.
    pub reapply_app_rules_on_title_change: bool,
    #[serde(default)]
    /// Assign matching applications and windows to workspaces.
    pub app_rules: Vec<AppWorkspaceRule>,
    #[serde(default)]
    /// Choose independent layouts and ordered display preferences for individual workspaces.
    pub workspace_rules: Vec<WorkspaceLayoutRule>,
}

#[serde_with::serde_as]
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceLayoutRule {
    pub workspace: WorkspaceSelector,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<LayoutMode>,
    /// Ordered display UUIDs, names, or builtin/left/right/top/bottom selectors.
    /// The first eligible preference wins. Only applies when
    /// `mode = "global"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde_as(as = "Option<serde_with::OneOrMany<_>>")]
    pub display: Option<Vec<String>>,
}

// Allow specifying a workspace by numeric index or by name in the config.
// This supports both `workspace = 2` and `workspace = "coding"` in app rules.
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct AppWorkspaceRule {
    /// Application bundle identifier (e.g., "com.apple.Terminal")
    pub app_id: Option<String>,
    /// Target workspace index (0 based) OR workspace name. If None, window goes to active workspace.
    pub workspace: Option<WorkspaceSelector>,
    /// Preferred window size in logical pixels.
    pub size: Option<AppRuleSize>,
    /// Focus the window after applying this rule, switching virtual workspaces if needed.
    #[serde(default)]
    pub focus: bool,
    /// An explicit management override. `false` makes the window invisible to Wm;
    /// `true` overrides normal manageability heuristics for a visible window. When
    /// omitted, the matching rule leaves Wm's normal manageability decision intact.
    #[serde(default)]
    pub manage: Option<bool>,
    pub app_name: Option<String>,
    /// Optional: Regular expression to match window title (applies to window.title)
    ///
    /// If present, this regex will be used when attempting to match a window by
    /// title.
    pub title_regex: Option<String>,
    /// Optional: Substring to search for in window title (applies to window.title)
    ///
    /// If present, wm will internally treat this as a substring match and will
    /// construct a regex to match titles containing this substring. This allows
    /// people who don't want to write full regexes to match by a simple substring.
    pub title_substring: Option<String>,

    /// Optional: Accessibility role to match (AXRole). If present, it must be a
    /// non-empty string and will be compared against the accessibility role
    /// reported by the AX APIs for a window (exact string match).
    pub ax_role: Option<String>,

    /// Accessibility subrole must be non-empty and is compared against the subrole
    /// reported by the AX APIs for a window (exact string match).
    pub ax_subrole: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Copy)]
#[serde(deny_unknown_fields)]
pub struct AppRulePosition {
    pub x: f64,
    pub y: f64,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Copy)]
#[serde(deny_unknown_fields)]
pub struct AppRuleSize {
    pub w: Option<f64>,
    pub h: Option<f64>,
}

impl Default for VirtualWorkspaceSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: WorkspaceMode::PerDisplay,
            default_workspace_count: default_workspace_count(),
            auto_assign_windows: true,
            preserve_focus_per_workspace: true,
            workspace_auto_back_and_forth: false,
            prevent_wrapping: false,
            workspace_names: default_workspace_names(),
            default_workspace: 0,
            reapply_app_rules_on_title_change: false,
            app_rules: Vec::new(),
            workspace_rules: Vec::new(),
        }
    }
}

impl VirtualWorkspaceSettings {
    pub fn workspace_name(&self, index: usize) -> String {
        self.workspace_names
            .get(index)
            .filter(|name| !name.is_empty())
            .cloned()
            .unwrap_or_else(|| format!("Workspace {}", index + 1))
    }

    pub fn resize(&mut self, count: usize) {
        self.default_workspace_count = count;
        // Invalid counts are left for the shared validator, without large allocations.
        if !(1..=MAX_WORKSPACES).contains(&count) {
            return;
        }
        let removed = self.workspace_names.iter().skip(count).cloned().collect::<Vec<_>>();
        self.workspace_names.truncate(count);
        self.default_workspace = self.default_workspace.min(count - 1);
        let removed_selector = |v: &WorkspaceSelector| match v {
            WorkspaceSelector::Index(i) => *i >= count,
            WorkspaceSelector::Name(n) => removed.contains(n),
        };
        self.workspace_rules.retain(|r| !removed_selector(&r.workspace));
        for r in &mut self.app_rules {
            if r.workspace.as_ref().is_some_and(removed_selector) {
                r.workspace = None;
            }
        }
    }

    pub fn validate(&self) -> Vec<String> {
        let mut issues = Vec::new();

        if self.default_workspace_count == 0 {
            issues.push("default_workspace_count must be at least 1".to_string());
        }
        if self.default_workspace_count > MAX_WORKSPACES {
            issues.push(format!(
                "default_workspace_count should not exceed {} for performance reasons",
                MAX_WORKSPACES
            ));
        }

        if self.workspace_names.len() > self.default_workspace_count {
            issues.push("More workspace names provided than default_workspace_count".to_string());
        }

        if self.default_workspace >= self.default_workspace_count {
            issues.push(format!(
                "default_workspace ({}) must be less than default_workspace_count ({})",
                self.default_workspace, self.default_workspace_count
            ));
        }

        let mut names = crate::common::collections::HashMap::default();
        for index in 0..self.default_workspace_count.min(MAX_WORKSPACES) {
            let name = self.workspace_name(index);
            if let Some(first) = names.get(&name) {
                issues.push(format!(
                    "Duplicate workspace name {name:?} (workspaces {} and {}). Workspace names must be unique.",
                    first + 1, index + 1
                ));
            } else {
                names.insert(name, index);
            }
        }

        for (index, rule) in self.workspace_rules.iter().enumerate() {
            if rule.layout.is_none() && rule.display.is_none() {
                issues.push(format!("Workspace rule {index} has no layout or display"));
            }
            if let Some(preferences) = &rule.display
                && (preferences.is_empty()
                    || preferences.iter().any(|value| value.trim().is_empty()))
            {
                issues.push(format!("Workspace rule {index} display preferences must contain nonempty UUIDs, names, or keywords"));
            }
        }

        // Rules for different apps can share a title or AX role. A rule is only a duplicate
        // when it matches exactly the same windows as an earlier one, and since the earlier
        // rule wins a tie, the later one would never apply.
        let mut seen_matchers: crate::common::collections::HashMap<_, usize> = Default::default();

        for (index, rule) in self.app_rules.iter().enumerate() {
            fn field(value: &Option<String>) -> Option<&str> {
                value.as_deref().filter(|v| !v.is_empty())
            }
            let matcher = (
                field(&rule.app_id).map(str::to_ascii_lowercase),
                field(&rule.app_name).map(str::to_lowercase),
                field(&rule.title_regex).map(str::to_owned),
                field(&rule.title_substring).map(str::to_lowercase),
                field(&rule.ax_role).map(str::to_owned),
                field(&rule.ax_subrole).map(str::to_owned),
            );
            let first = *seen_matchers.entry(matcher).or_insert(index);
            if first != index {
                issues.push(format!(
                    "App rule {index} matches the same windows as rule {first}, so it never applies"
                ));
            }

            let app_id_empty = rule.app_id.as_ref().map_or(true, |id| id.is_empty());
            if app_id_empty
                && rule.app_name.is_none()
                && rule.title_regex.is_none()
                && rule.title_substring.is_none()
                && rule.ax_role.is_none()
                && rule.ax_subrole.is_none()
            {
                issues.push(format!(
                    "App rule {} has no app_id, app_name, title_regex, or title_substring specified",
                    index
                ));
            }

            if let Some(ref workspace) = rule.workspace {
                if let WorkspaceSelector::Index(idx) = workspace {
                    if *idx >= self.default_workspace_count {
                        issues.push(format!(
                            "App rule {} references workspace {} but only {} workspaces will be created",
                            index, idx, self.default_workspace_count
                        ));
                    }
                }
            }

            if let Some(size) = rule.size {
                if size.w.is_none() && size.h.is_none() {
                    issues.push(format!(
                        "App rule {} size must specify at least one of w or h",
                        index
                    ));
                }
                if size.w.is_some_and(|value| !value.is_finite() || value <= 0.0)
                    || size.h.is_some_and(|value| !value.is_finite() || value <= 0.0)
                {
                    issues.push(format!(
                        "App rule {} size dimensions must be finite positive values",
                        index
                    ));
                }
            }

            if let Some(ref app_id) = rule.app_id {
                if !app_id.is_empty() && !app_id.contains('.') {
                    issues.push(format!(
                        "App rule {} has suspicious app_id '{}' (should be bundle identifier like 'com.example.app')",
                        index, app_id
                    ));
                }
            }

            if let Some(ref title_re) = rule.title_regex {
                if title_re.is_empty() {
                    issues.push(format!("App rule {} has empty title_regex", index));
                } else if let Err(error) =
                    RegexBuilder::new(title_re).case_insensitive(true).build()
                {
                    issues.push(format!(
                        "App rule {} has invalid title_regex '{}': {}",
                        index, title_re, error
                    ));
                }
            }

            if rule.manage == Some(false)
                && (rule.workspace.is_some()
                    || rule.size.is_some()
                    || rule.focus)
            {
                issues.push(format!(
                    "App rule {} sets manage = false, so its workspace, size, and focus effects are ignored",
                    index
                ));
            }

            if rule.title_substring.as_deref() == Some("") {
                issues.push(format!("App rule {} has empty title_substring", index));
            }
            if rule.ax_role.as_deref() == Some("") {
                issues.push(format!("App rule {} has empty ax_role", index));
            }
            if rule.ax_subrole.as_deref() == Some("") {
                issues.push(format!("App rule {} has empty ax_subrole", index));
            }
        }

        issues
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ConfigSource {
    pub settings: Settings,
    pub keys: std::collections::BTreeMap<String, WmCommand>,
    #[serde(default)]
    pub binding_modes:
        std::collections::BTreeMap<String, std::collections::BTreeMap<String, WmCommand>>,
    #[serde(default)]
    pub virtual_workspaces: VirtualWorkspaceSettings,
    /// Modifier combinations that can be reused in key bindings
    /// e.g., "comb1" = "Alt + Shift" allows using "comb1 + C" in keys
    #[serde(default)]
    pub modifier_combinations: std::collections::BTreeMap<String, String>,
}

#[derive(Serialize, Clone, Debug)]
pub struct Config {
    pub settings: Settings,
    pub keys: Vec<(Hotkey, WmCommand)>,
    #[serde(default)]
    pub binding_mode_specs: BindingModeSpecs,
    pub virtual_workspaces: VirtualWorkspaceSettings,
}

pub type BindingModeSpecs = Vec<(String, Vec<(String, WmCommand)>)>;

unsafe impl Send for Config {}
unsafe impl Sync for Config {}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    #[serde(default)]
    /// Animate windows when their layout changes.
    pub animate: bool,
    #[serde(default = "default_animation_duration")]
    /// Duration of a window animation, in seconds.
    pub animation_duration: f64,
    #[serde(default = "default_animation_fps")]
    /// Maximum number of frames per second during a window animation.
    pub animation_fps: f64,
    #[serde(default)]
    /// The timing curve used for window animations.
    pub animation_easing: AnimationEasing,
    #[serde(default = "yes")]
    /// Start Wm with automatic tiling turned off.
    pub default_disable: bool,
    #[serde(default = "yes")]
    /// Move the pointer into the window when focus changes.
    pub mouse_follows_focus: bool,
    #[serde(default = "yes")]
    /// Hide the pointer after moving it into the focused window.
    pub mouse_hides_on_focus: bool,
    #[serde(default = "yes")]
    /// Focus a window when the pointer moves over it.
    pub focus_follows_mouse: bool,
    /// Treat vertically stacked displays as a horizontal pointer chain.
    /// `top-to-bottom` maps higher displays to the left; `bottom-to-top` reverses it.
    #[serde(default)]
    pub horizontal_mouse_warp: Option<HorizontalMouseWarp>,
    /// Hotkey that disables focus-follows-mouse while held.
    /// Accepts either a full hotkey (e.g. "Ctrl + A") or a modifier-only spec (e.g. "Ctrl")
    #[serde(default)]
    pub focus_follows_mouse_disable_hotkey: Option<HotkeySpec>,
    /// Apps that should not trigger automatic workspace switching when activated.
    /// List of bundle identifiers (e.g., "com.apple.Spotlight") that often
    /// inappropriately steal focus and shouldn't cause workspace switches.
    #[serde(default)]
    pub auto_focus_blacklist: Vec<String>,
    #[serde(default)]
    pub layout: LayoutSettings,
    #[serde(default)]
    pub ui: UiSettings,
    /// Trackpad gesture settings
    #[serde(default)]
    pub gestures: GestureSettings,
    /// Modifier-assisted dragging and tiled-window drop settings.
    #[serde(default)]
    pub drag_drop: DragDropSettings,

    /// Commands to run on startup (e.g., for subscribing to events)
    #[serde(default)]
    pub run_on_start: Vec<String>,

    /// Enable hot-reloading of the config file when it changes
    #[serde(default = "yes")]
    /// Reload the configuration automatically when the file changes.
    pub hot_reload: bool,
}

#[derive(Serialize, Deserialize, Debug, Copy, Clone, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum HorizontalMouseWarp {
    TopToBottom,
    BottomToTop,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct UiSettings {
    #[serde(default)]
    pub menu_bar: MenuBarSettings,
    #[serde(default)]
    pub stack_line: StackLineSettings,
    #[serde(default)]
    pub mission_control: MissionControlSettings,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[serde(deny_unknown_fields)]
pub struct GestureSettings {
    /// Enable horizontal swipes to switch virtual workspaces
    #[serde(default)]
    pub enabled: bool,
    /// If true, consume horizontal swipe events owned by Wm so macOS and the
    /// foreground app do not also handle them.
    #[serde(default = "yes")]
    pub consume_dock_swipe: bool,
    /// Invert horizontal direction (swap next/prev)
    #[serde(default)]
    pub invert_horizontal_swipe: bool,
    /// Maximum absolute Y delta allowed for the gesture to count as horizontal
    #[serde(default = "default_swipe_vertical_tolerance")]
    pub swipe_vertical_tolerance: f64,
    /// If true, attempt to skip empty workspaces on swipe (if supported)
    #[serde(default)]
    pub skip_empty: bool,
    /// Number of fingers required for swipe (default = 3)
    #[serde(default = "default_swipe_fingers")]
    pub fingers: usize,
    /// Normalized horizontal distance (0..1) required to fire a swipe
    #[serde(default = "default_distance_pct")]
    pub distance_pct: f64,
    /// Enable haptic feedback when a swipe commits
    #[serde(default = "yes")]
    pub haptics_enabled: bool,
    /// Haptic feedback pattern (generic | alignment | level_change)
    #[serde(default)]
    pub haptic_pattern: HapticPattern,
}

impl Default for GestureSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            consume_dock_swipe: true,
            invert_horizontal_swipe: false,
            swipe_vertical_tolerance: default_swipe_vertical_tolerance(),
            skip_empty: true,
            fingers: default_swipe_fingers(),
            distance_pct: default_distance_pct(),
            haptics_enabled: true,
            haptic_pattern: HapticPattern::LevelChange,
        }
    }
}

///
/// Serialized values are `cmd`, `alt`, `shift`, `ctrl`, `fn`, or a combination in
/// key-binding syntax such as `Ctrl + Alt + Shift + Meta`.
#[derive(Debug, PartialEq, Clone, Copy, Default, Eq, SerializeDisplay, DeserializeFromStr)]
pub enum MouseModifier {
    /// Globe/Fn. This is the default because it rarely conflicts with apps.
    #[default]
    Fn,
    /// One or more modifier families. One family matches while held, two or more match exactly.
    Keys(Modifiers),
}

impl std::str::FromStr for MouseModifier {
    type Err = anyhow::Error;

    /// Parses a key binding's modifiers, such as `command` or `Ctrl + Alt`. Left/right
    /// spellings stand for either side.
    fn from_str(spec: &str) -> Result<Self, Self::Err> {
        if spec.trim().eq_ignore_ascii_case("fn") {
            return Ok(Self::Fn);
        }

        Ok(Self::Keys(modifiers_from_str(spec)?.families()))
    }
}

impl std::fmt::Display for MouseModifier {
    /// Use the legacy canonical names when serializing single modifiers.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Fn => f.write_str("fn"),
            Self::Keys(Modifiers::META) => f.write_str("cmd"),
            Self::Keys(Modifiers::ALT) => f.write_str("alt"),
            Self::Keys(Modifiers::SHIFT) => f.write_str("shift"),
            Self::Keys(Modifiers::CONTROL) => f.write_str("ctrl"),
            Self::Keys(modifiers) => write!(f, "{modifiers}"),
        }
    }
}

/// Operation reserved for a modifier-plus-mouse-button gesture.
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Copy, Default, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MouseAction {
    /// Do not capture this button; the click is delivered normally.
    None,
    /// Move a window from anywhere inside it.
    #[default]
    Move,
}

/// Action used when a tiled window is released in another tile's center zone.
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Copy, Default, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MouseDropAction {
    /// Exchange the source and target's logical layout positions.
    #[default]
    Swap,
    /// Move the source into the target's stack/group and select the source.
    Stack,
}

/// Modifier mouse actions and native tiled-window drop settings.
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Copy)]
#[serde(deny_unknown_fields)]
pub struct DragDropSettings {
    /// Enable trackpad feedback after a successful window drop.
    #[serde(default = "yes")]
    pub haptics_enabled: bool,
    /// Enables native drag targeting and modifier mouse actions.
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Modifier held with `action1` or `action2`: `cmd`, `alt`, `shift`, `ctrl`, `fn`,
    /// or a combination such as `Ctrl + Alt + Shift + Meta` (`fn` can't be combined).
    /// Left/right spellings such as `AltRight` match either side. A single modifier works
    /// while held; a combination must be held exactly.
    #[serde(default)]
    pub modifier: MouseModifier,
    /// Left-button action while the configured modifier is held.
    #[serde(default)]
    pub action1: MouseAction,
    /// Right-button action while the configured modifier is held.
    #[serde(default = "default_mouse_action_none")]
    pub action2: MouseAction,
    /// Center-zone action for tiled move drops.
    #[serde(default)]
    pub drop_action: MouseDropAction,
    /// Depth of each edge zone as a fraction of the destination window's size.
    /// Valid values are `0.10..=0.45`; the default is `0.25`.
    #[serde(default = "default_drop_zone_fraction")]
    pub drop_zone_fraction: f64,
    /// Shows a translucent, rounded WindowServer overlay for the pending drop.
    /// The overlay is updated only when the target tile or drop zone changes.
    #[serde(default = "yes")]
    pub preview: bool,
}

impl Default for DragDropSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            haptics_enabled: true,
            modifier: MouseModifier::Fn,
            action1: MouseAction::Move,
            action2: MouseAction::None,
            drop_action: MouseDropAction::Swap,
            drop_zone_fraction: default_drop_zone_fraction(),
            preview: true,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Copy, Default)]
#[serde(rename_all = "snake_case")]
pub enum MenuBarDisplayMode {
    #[default]
    All,
    Active,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Copy, Default)]
#[serde(rename_all = "snake_case")]
pub enum ActiveWorkspaceLabel {
    #[default]
    Index,
    Name,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Copy, Default)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceDisplayStyle {
    None,
    #[default]
    Layout,
    Label,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[serde(deny_unknown_fields)]
pub struct MenuBarSettings {
    #[serde(default = "yes")]
    /// Show Wm and workspace information in the menu bar.
    pub enabled: bool,
    #[serde(default)]
    /// Include workspaces without windows in the menu bar.
    pub show_empty: bool,
    #[serde(default = "yes")]
    /// Show the active binding mode beside workspaces, except in default mode.
    pub show_binding_mode: bool,
    #[serde(default)]
    /// Show the active workspace's windows in layout order.
    pub show_window_list: bool,
    #[serde(default)]
    /// Combine windows from the same application into one icon.
    pub group_app_icons: bool,
    #[serde(default)]
    /// Only show workspaces from the display currently hosting the active menu bar.
    pub current_display_only: bool,
    #[serde(default)]
    /// Choose whether to show all workspaces or only the active one.
    pub mode: MenuBarDisplayMode,
    #[serde(default)]
    /// Choose workspace numbers or names when using the label display style.
    pub active_label: ActiveWorkspaceLabel,
    #[serde(default)]
    /// Show workspace labels or layout indicators.
    pub display_style: WorkspaceDisplayStyle,
    #[serde(default)]
    /// Wrap workspace labels or layout indicators in brackets when a window is fullscreen.
    pub show_fullscreen_indicator: bool,
    #[serde(default = "default_layout_folder")]
    /// Folder containing custom layout indicator images.
    pub layout_folder: PathBuf,
}

impl MenuBarSettings {
    pub fn resolved_layout_folder(&self) -> PathBuf {
        let Ok(relative) = self.layout_folder.strip_prefix("~") else {
            return self.layout_folder.clone();
        };
        dirs::home_dir()
            .map(|home| home.join(relative))
            .unwrap_or_else(|| self.layout_folder.clone())
    }
}

impl Default for MenuBarSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            show_empty: false,
            show_binding_mode: true,
            show_window_list: false,
            group_app_icons: false,
            current_display_only: false,
            mode: MenuBarDisplayMode::default(),
            active_label: ActiveWorkspaceLabel::default(),
            display_style: WorkspaceDisplayStyle::default(),
            show_fullscreen_indicator: false,
            layout_folder: default_layout_folder(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[serde(deny_unknown_fields)]
pub struct StackLineSettings {
    /// Show native Liquid Glass tabs beside stacked windows.
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    /// What hovering a tab does: just highlight it, or select it as a click would.
    pub hover: StackLineHoverMode,
    #[serde(default)]
    /// Where the tabs sit for a horizontal stack.
    pub horiz_placement: HorizontalPlacement,
    #[serde(default)]
    /// Where the tabs sit for a vertical stack; their titles run along the bar.
    pub vert_placement: VerticalPlacement,
    #[serde(default = "default_stack_line_thickness")]
    /// Thickness of the tab bar, in points (at least 20).
    pub thickness: f64,
    #[serde(default)]
    /// Space between the tab bar and its windows, in points.
    pub spacing: f64,
}

impl Default for StackLineSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            hover: StackLineHoverMode::default(),
            horiz_placement: HorizontalPlacement::default(),
            vert_placement: VerticalPlacement::default(),
            thickness: default_stack_line_thickness(),
            spacing: 0.0,
        }
    }
}

fn default_stack_line_thickness() -> f64 { cgs::tabs::TabBar::HEIGHT }

impl StackLineSettings {
    /// The native tab bar is unusable below this thickness.
    pub const MIN_THICKNESS: f64 = 20.0;

    pub fn validate(&self) -> Vec<String> {
        let mut issues = Vec::new();
        if !self.thickness.is_finite() || self.thickness < Self::MIN_THICKNESS {
            issues.push(format!(
                "ui.stack_line.thickness must be at least {}, got {}",
                Self::MIN_THICKNESS,
                self.thickness
            ));
        }
        if !self.spacing.is_finite() || self.spacing < 0.0 {
            issues.push(format!(
                "ui.stack_line.spacing must be non-negative, got {}",
                self.spacing
            ));
        }
        issues
    }
}

/// Values keep their pre-native meanings so existing configs behave as before.
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Copy, Default)]
#[serde(rename_all = "snake_case")]
pub enum StackLineHoverMode {
    /// Hovering a tab selects it, as a click would.
    Click,
    /// Hovering only highlights a tab; click to select it.
    #[default]
    Hover,
}

impl StackLineHoverMode {
    pub fn selects_on_hover(self) -> bool { self == Self::Click }
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Copy, Default)]
#[serde(rename_all = "snake_case")]
pub enum HorizontalPlacement {
    #[default]
    Top,
    Bottom,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Copy, Default)]
#[serde(rename_all = "snake_case")]
pub enum VerticalPlacement {
    #[default]
    Left,
    Right,
}

/// Space a stack gives up to its tab bar, and on which edge.
#[derive(Debug, PartialEq, Clone, Copy, Default)]
pub struct StackLineReserve {
    pub thickness: f64,
    pub spacing: f64,
    pub horiz: HorizontalPlacement,
    pub vert: VerticalPlacement,
}

impl StackLineReserve {
    /// Thickness plus spacing; nothing when the bar is absent.
    pub fn total(&self) -> f64 {
        if self.thickness > 0.0 {
            self.thickness + self.spacing
        } else {
            0.0
        }
    }

    /// The reservation along one axis of a node of `kind`. Horizontal stacks give up
    /// height for a bar above or below; vertical stacks give up width for one beside.
    pub fn on_axis(&self, kind: crate::layout_engine::LayoutKind, horizontal: bool) -> f64 {
        use crate::layout_engine::LayoutKind::*;
        match (kind, horizontal) {
            (HorizontalStack, false) | (VerticalStack, true) => self.total(),
            _ => 0.0,
        }
    }

    /// Shrinks a stack's frame to leave room for its bar.
    pub fn inset(
        &self,
        mut rect: objc2_core_foundation::CGRect,
        horizontal_stack: bool,
    ) -> objc2_core_foundation::CGRect {
        let reserve = self.total();
        if horizontal_stack {
            let reserve = reserve.min(rect.size.height);
            if self.horiz == HorizontalPlacement::Top {
                rect.origin.y += reserve;
            }
            rect.size.height -= reserve;
        } else {
            let reserve = reserve.min(rect.size.width);
            if self.vert == VerticalPlacement::Left {
                rect.origin.x += reserve;
            }
            rect.size.width -= reserve;
        }
        rect
    }

    /// The bar's own frame along the reserved edge of a stack's frame, never beyond it.
    pub fn bar(
        &self,
        rect: objc2_core_foundation::CGRect,
        horizontal_stack: bool,
    ) -> objc2_core_foundation::CGRect {
        use objc2_core_foundation::{CGPoint, CGRect, CGSize};
        let (o, s) = (rect.origin, rect.size);
        let t = self.thickness.min(if horizontal_stack { s.height } else { s.width }).max(0.0);
        match (horizontal_stack, self.horiz, self.vert) {
            (true, HorizontalPlacement::Top, _) => CGRect::new(o, CGSize::new(s.width, t)),
            (true, HorizontalPlacement::Bottom, _) => {
                CGRect::new(CGPoint::new(o.x, o.y + s.height - t), CGSize::new(s.width, t))
            }
            (false, _, VerticalPlacement::Left) => CGRect::new(o, CGSize::new(t, s.height)),
            (false, _, VerticalPlacement::Right) => {
                CGRect::new(CGPoint::new(o.x + s.width - t, o.y), CGSize::new(t, s.height))
            }
        }
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct MissionControlSettings {
    #[serde(default)]
    /// Release the shortcut key and all modifier keys to switch to the selected window or workspace.
    /// When turned off, press Return or click to switch.
    pub activate_on_hotkey_release: bool,
    /// Include workspaces without windows in Overview.
    #[serde(default = "yes")]
    pub show_empty_workspaces: bool,
    #[serde(default = "yes")]
    /// Show a preview of each window in Overview. Requires Screen Recording permission.
    pub window_previews: bool,
    #[serde(default)]
    /// Enable the workspace Overview. Requires restarting Wm.
    pub enabled: bool,
    #[serde(default)]
    /// Fade Overview labels and decorations in and out.
    pub fade_enabled: bool,
    #[serde(default = "default_mission_control_fade_duration_ms")]
    /// Duration of Overview entry and camera navigation, in milliseconds.
    pub fade_duration_ms: f64,
}

fn default_mission_control_fade_duration_ms() -> f64 { 180.0 }

fn default_drop_zone_fraction() -> f64 { 0.25 }

fn default_mouse_action_none() -> MouseAction { MouseAction::None }



fn default_scrolling_column_width_ratio() -> f64 { 0.7 }
fn default_scrolling_preset_column_widths() -> Vec<f64> { vec![1.0 / 3.0, 0.5, 2.0 / 3.0] }

fn default_scrolling_min_column_width_ratio() -> f64 { 0.3 }

fn default_scrolling_max_column_width_ratio() -> f64 { 0.9 }

impl StackLineSettings {
    pub fn reserve(&self) -> StackLineReserve {
        StackLineReserve {
            thickness: if self.enabled {
                self.thickness.max(0.0)
            } else {
                0.0
            },
            spacing: self.spacing,
            horiz: self.horiz_placement,
            vert: self.vert_placement,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Copy, Default)]
#[serde(rename_all = "snake_case")]
pub enum WindowInsertionPoint {
    /// Insert a new window immediately after the current selection.
    #[default]
    NextToSelection,
    /// Append a new window at the end of the layout tree.
    EndOfTree,
}

/// Options understood by every layout system.
///
/// These fields are flattened into both `[settings.layout]` and every
/// per-layout table. A per-layout value overrides the layout-wide value.
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct BaseLayoutSettings {
    /// Where newly managed windows are inserted.
    #[serde(default)]
    pub window_insertion_point: Option<WindowInsertionPoint>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct LayoutSettings {
    /// Settings inherited by every layout type unless overridden by its table.
    #[serde(flatten)]
    pub base: BaseLayoutSettings,
    /// Layout used by workspaces without their own layout override.
    #[serde(default)]
    pub mode: LayoutMode,
    #[serde(default)]
    pub gaps: GapSettings,
    /// Scrolling layout configuration (niri-style columns)
    #[serde(default)]
    pub scrolling: ScrollingLayoutSettings,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[serde(deny_unknown_fields)]
pub struct ScrollingLayoutSettings {
    #[serde(flatten)]
    pub base: BaseLayoutSettings,
    /// Whether to animate windows moving in the scrolling layout
    /// HIGHLY RECOMMENDED to leave this enabled.
    #[serde(default = "default_scrolling_animate")]
    pub animate: Option<bool>,
    /// Default width of the active column, as a fraction of the screen width.
    #[serde(default = "default_scrolling_column_width_ratio")]
    pub column_width_ratio: f64,
    /// Proportional column widths cycled in configured order.
    #[serde(default = "default_scrolling_preset_column_widths")]
    pub preset_column_widths: Vec<f64>,
    /// Keep a window's existing column width when it enters scrolling layout.
    #[serde(default = "yes")]
    pub preserve_window_sizes: bool,
    /// Fill the usable width when a workspace has only one scrolling column.
    /// The column's stored width is restored when another column is added.
    #[serde(default)]
    pub expand_single_column: bool,
    /// Minimum column width ratio allowed by resize commands.
    #[serde(default = "default_scrolling_min_column_width_ratio")]
    pub min_column_width_ratio: f64,
    /// Maximum column width ratio allowed by resize commands.
    #[serde(default = "default_scrolling_max_column_width_ratio")]
    pub max_column_width_ratio: f64,
    /// Sparse width overrides keyed by display UUID.
    #[serde(default)]
    pub per_display: HashMap<String, ScrollingWidthOverride>,
    /// Initial placement of a single column (left, center, right).
    #[serde(default)]
    pub alignment: ScrollingAlignment,
    /// Horizontal focus navigation behavior:
    /// - niri: reveal only as needed based on navigation direction.
    /// Gesture release follows niri snap positions.
    #[serde(default)]
    pub focus_navigation_style: ScrollingFocusNavigationStyle,
    /// Trackpad gestures for scrolling layout
    #[serde(default)]
    pub gestures: ScrollingGestureSettings,
}

fn default_scrolling_animate() -> Option<bool> { Some(true) }

impl Default for ScrollingLayoutSettings {
    fn default() -> Self {
        Self {
            base: BaseLayoutSettings::default(),
            animate: Some(true),
            column_width_ratio: default_scrolling_column_width_ratio(),
            preset_column_widths: default_scrolling_preset_column_widths(),
            preserve_window_sizes: true,
            expand_single_column: false,
            min_column_width_ratio: default_scrolling_min_column_width_ratio(),
            max_column_width_ratio: default_scrolling_max_column_width_ratio(),
            per_display: HashMap::default(),
            alignment: ScrollingAlignment::default(),
            focus_navigation_style: ScrollingFocusNavigationStyle::default(),
            gestures: ScrollingGestureSettings::default(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct ScrollingWidthOverride {
    pub column_width_ratio: Option<f64>,
    pub min_column_width_ratio: Option<f64>,
    pub max_column_width_ratio: Option<f64>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Copy, Default)]
#[serde(rename_all = "snake_case")]
pub enum ScrollingAlignment {
    Left,
    #[default]
    Center,
    Right,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Copy, Default)]
#[serde(rename_all = "snake_case")]
pub enum ScrollingFocusNavigationStyle {
    #[default]
    Niri,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub struct ScrollingGestureSettings {
    /// Enable continuous horizontal viewport gestures
    #[serde(default)]
    pub enabled: bool,
    /// Legacy value accepted for compatibility; scrolling animation controls gesture release.
    #[serde(default, skip_serializing)]
    pub animate: Option<bool>,
    /// Invert horizontal direction (swap left/right)
    #[serde(default)]
    pub invert_horizontal: bool,
    /// Maximum absolute Y delta allowed for the gesture to count as horizontal
    #[serde(default = "default_swipe_vertical_tolerance")]
    pub vertical_tolerance: f64,
    /// Number of fingers required for scroll gesture
    #[serde(default = "default_swipe_fingers")]
    pub fingers: usize,
    /// Retained for config compatibility; continuous scrolling uses a small intent dead zone
    #[deprecated(since = "0.6.3")]
    #[serde(default = "default_distance_pct")]
    pub distance_pct: f64,
    /// If true, scrolling past the end of the strip will trigger a workspace switch
    #[serde(default)]
    pub propagate_to_workspace_swipe: bool,
    /// Edge travel in working-area widths required on release for one workspace switch.
    /// Measured directly as a fraction of the working-area width.
    #[serde(default = "default_overscroll_threshold")]
    pub workspace_switch_threshold: f64,
}

impl Default for ScrollingGestureSettings {
    #[allow(deprecated)]
    fn default() -> Self {
        Self {
            enabled: false,
            animate: None,
            invert_horizontal: false,
            vertical_tolerance: default_swipe_vertical_tolerance(),
            fingers: default_swipe_fingers(),
            distance_pct: default_distance_pct(),
            propagate_to_workspace_swipe: false,
            workspace_switch_threshold: default_overscroll_threshold(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum StackDefaultOrientation {
    Perpendicular,
    Same,
    Horizontal,
    Vertical,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct GapSettings {
    #[serde(default)]
    pub outer: OuterGaps,
    #[serde(default)]
    pub inner: InnerGaps,
    #[serde(default)]
    pub per_display: HashMap<String, GapOverride>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct OuterGaps {
    #[serde(default)]
    pub top: f64,
    #[serde(default)]
    pub left: f64,
    #[serde(default)]
    pub bottom: f64,
    #[serde(default)]
    pub right: f64,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct InnerGaps {
    #[serde(default)]
    pub horizontal: f64,
    #[serde(default)]
    pub vertical: f64,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct GapOverride {
    #[serde(default)]
    pub outer: Option<OuterGaps>,
    #[serde(default)]
    pub inner: Option<InnerGaps>,
}





impl Settings {
    pub fn validate(&self) -> Vec<String> {
        let mut issues = Vec::new();

        if self.animation_duration < 0.0 {
            issues.push(format!(
                "animation_duration must be non-negative, got {}",
                self.animation_duration
            ));
        }

        if self.animation_fps <= 0.0 {
            issues.push(format!(
                "animation_fps must be positive, got {}",
                self.animation_fps
            ));
        }

        issues.extend(self.layout.validate());
        issues.extend(self.ui.stack_line.validate());

        if !(0.10..=0.45).contains(&self.drag_drop.drop_zone_fraction) {
            issues.push(format!(
                "drag_drop.drop_zone_fraction must be between 0.10 and 0.45, got {}",
                self.drag_drop.drop_zone_fraction
            ));
        }

        if self.gestures.swipe_vertical_tolerance < 0.0 {
            issues.push(format!(
                "gestures.swipe_vertical_tolerance must be non-negative, got {}",
                self.gestures.swipe_vertical_tolerance
            ));
        }

        issues
    }
}

impl LayoutSettings {
    pub fn base_for(&self, mode: LayoutMode) -> &BaseLayoutSettings {
        match mode {
            LayoutMode::Scrolling => &self.scrolling.base,
        }
    }

    pub fn window_insertion_point_for(&self, mode: LayoutMode) -> WindowInsertionPoint {
        self.base_for(mode)
            .window_insertion_point
            .or(self.base.window_insertion_point)
            .unwrap_or_default()
    }

    pub fn resolved_base_for(&self, mode: LayoutMode) -> BaseLayoutSettings {
        BaseLayoutSettings {
            window_insertion_point: Some(self.window_insertion_point_for(mode)),
        }
    }

    pub fn validate(&self) -> Vec<String> {
        let mut issues = Vec::new();



        issues.extend(self.gaps.validate());

        issues.extend(self.scrolling.validate());

        issues
    }
}

impl ScrollingLayoutSettings {
    pub fn widths_for_display(&self, display_uuid: Option<&str>) -> (f64, f64, f64) {
        let override_ = display_uuid.and_then(|uuid| self.per_display.get(uuid));
        (
            override_.and_then(|o| o.column_width_ratio).unwrap_or(self.column_width_ratio),
            override_
                .and_then(|o| o.min_column_width_ratio)
                .unwrap_or(self.min_column_width_ratio),
            override_
                .and_then(|o| o.max_column_width_ratio)
                .unwrap_or(self.max_column_width_ratio),
        )
    }

    pub fn effective_for_display(&self, display_uuid: Option<&str>) -> Self {
        let mut resolved = self.clone();
        (
            resolved.column_width_ratio,
            resolved.min_column_width_ratio,
            resolved.max_column_width_ratio,
        ) = self.widths_for_display(display_uuid);
        resolved.per_display.clear();
        resolved
    }

    pub fn validate(&self) -> Vec<String> {
        let mut issues = Vec::new();
        Self::validate_widths("layout.scrolling", self.widths_for_display(None), &mut issues);
        for uuid in self.per_display.keys() {
            Self::validate_widths(
                &format!("layout.scrolling.per_display[{uuid}]"),
                self.widths_for_display(Some(uuid)),
                &mut issues,
            );
        }

        if self.gestures.vertical_tolerance < 0.0 {
            issues.push(format!(
                "layout.scrolling.gestures.vertical_tolerance must be non-negative, got {}",
                self.gestures.vertical_tolerance
            ));
        }

        issues
    }

    fn validate_widths(path: &str, (ratio, min, max): (f64, f64, f64), issues: &mut Vec<String>) {
        if !(0.0..=1.0).contains(&ratio) {
            issues.push(format!(
                "{path}.column_width_ratio must be between 0.0 and 1.0, got {ratio}",
            ));
        }
        if !(0.0..=1.0).contains(&min) {
            issues.push(format!(
                "{path}.min_column_width_ratio must be between 0.0 and 1.0, got {min}",
            ));
        }
        if !(0.0..=1.0).contains(&max) {
            issues.push(format!(
                "{path}.max_column_width_ratio must be between 0.0 and 1.0, got {max}",
            ));
        }
        if min > max {
            issues.push(format!(
                "{path}.min_column_width_ratio ({min}) must be <= max_column_width_ratio ({max})",
            ));
        }
        if !(min..=max).contains(&ratio) {
            issues.push(format!(
                "{path}.column_width_ratio ({ratio}) must be within min/max bounds",
            ));
        }
    }
}





impl GapSettings {
    pub fn validate(&self) -> Vec<String> {
        let mut issues = Vec::new();

        issues.extend(self.outer.validate());

        issues.extend(self.inner.validate());

        for (uuid, overrides) in &self.per_display {
            if let Some(outer) = &overrides.outer {
                for issue in outer.validate() {
                    issues.push(format!("per_display[{uuid}] {issue}"));
                }
            }
            if let Some(inner) = &overrides.inner {
                for issue in inner.validate() {
                    issues.push(format!("per_display[{uuid}] {issue}"));
                }
            }
        }

        issues
    }

    pub fn effective_for_display(&self, display_uuid: Option<&str>) -> GapSettings {
        let mut resolved = GapSettings {
            outer: self.outer.clone(),
            inner: self.inner.clone(),
            per_display: HashMap::default(),
        };
        if let Some(uuid) = display_uuid {
            if let Some(overrides) = self.per_display.get(uuid) {
                if let Some(outer_override) = &overrides.outer {
                    resolved.outer = outer_override.clone();
                }
                if let Some(inner_override) = &overrides.inner {
                    resolved.inner = inner_override.clone();
                }
            }
        }
        resolved
    }
}

impl OuterGaps {
    pub fn validate(&self) -> Vec<String> {
        [
            ("top", self.top),
            ("left", self.left),
            ("bottom", self.bottom),
            ("right", self.right),
        ]
        .into_iter()
        .filter(|(_, value)| *value < 0.0)
        .map(|(name, value)| format!("outer.{name} gap must be non-negative, got {value}"))
        .collect()
    }
}

impl InnerGaps {
    pub fn validate(&self) -> Vec<String> {
        [("horizontal", self.horizontal), ("vertical", self.vertical)]
            .into_iter()
            .filter(|(_, value)| *value < 0.0)
            .map(|(name, value)| format!("inner.{name} gap must be non-negative, got {value}"))
            .collect()
    }
}

fn yes() -> bool { true }


pub fn default_stack_orientation() -> StackDefaultOrientation {
    StackDefaultOrientation::Perpendicular
}



fn default_animation_duration() -> f64 { 0.3 }

fn default_animation_fps() -> f64 { 100.0 }

fn default_layout_folder() -> PathBuf { PathBuf::from("~/.config/wm/layouts") }

fn default_workspace_count() -> usize { 4 }

fn default_workspace_names() -> Vec<String> {
    vec![
        "Main".to_string(),
        "Development".to_string(),
        "Communication".to_string(),
        "Utilities".to_string(),
    ]
}

// Interpreted as normalized fraction when <= 1.0. If > 1.0 and <= 100.0,
// it is treated as a percentage (e.g. 40.0 -> 0.40).
fn default_swipe_vertical_tolerance() -> f64 { 0.4 }
fn default_swipe_fingers() -> usize { 3 }
fn default_distance_pct() -> f64 { 0.08 }
fn default_overscroll_threshold() -> f64 { 0.55 }

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Copy, Default)]
#[serde(rename_all = "snake_case")]
pub enum HapticPattern {
    Generic,
    Alignment,
    #[default]
    LevelChange,
}

impl ConfigSource {
    pub fn keymap(&self, mode: &str) -> Option<&BTreeMap<String, WmCommand>> {
        if mode == "default" {
            Some(&self.keys)
        } else {
            self.binding_modes.get(mode)
        }
    }

    pub fn keymap_mut(&mut self, mode: &str) -> Result<&mut BTreeMap<String, WmCommand>, String> {
        if mode == "default" {
            Ok(&mut self.keys)
        } else {
            self.binding_modes.get_mut(mode).ok_or_else(|| "Keymap no longer exists".into())
        }
    }

    /// The hotkey a binding key triggers, with this source's modifier combinations.
    pub fn hotkey(&self, key: &str) -> Option<Hotkey> {
        Config::binding_hotkey(key, &self.modifier_combinations).map(|(_, hotkey)| hotkey)
    }

    /// The binding in `mode`, other than `except`, that the keys of `key` would also trigger.
    pub fn conflicting_binding(
        &self,
        mode: &str,
        key: &str,
        except: Option<&str>,
    ) -> Option<(&String, &WmCommand)> {
        let hotkey = self.hotkey(key)?;
        self.keymap(mode)?.iter().find(|(other, _)| {
            Some(other.as_str()) != except
                && self.hotkey(other).is_some_and(|other| other.overlaps(&hotkey))
        })
    }

    /// Binds `key` in `mode`, replacing the binding at `old`. Never overwrites another
    /// shortcut, and fails if `old` no longer holds `expected`, such as after a reload.
    pub fn bind(
        &mut self,
        mode: &str,
        old: Option<(&str, &WmCommand)>,
        key: String,
        command: WmCommand,
    ) -> Result<(), String> {
        if self.hotkey(&key).is_none() {
            return Err(format!("“{key}” is not a valid shortcut"));
        }
        if let Some((other, _)) = self.conflicting_binding(mode, &key, old.map(|(old, _)| old)) {
            return Err(format!("“{other}” already uses this shortcut"));
        }
        let bindings = self.keymap_mut(mode)?;
        if let Some((old, expected)) = old {
            if bindings.get(old) != Some(expected) {
                return Err("This shortcut changed elsewhere; try again".into());
            }
            bindings.remove(old);
        }
        bindings.insert(key, command);
        Ok(())
    }

    /// Replace exactly the conflicts the user reviewed, as one source transaction.
    /// Validate the entire snapshot before touching any binding, including `old`.
    pub fn bind_replacing(
        &mut self,
        mode: &str,
        old: Option<(&str, &WmCommand)>,
        key: String,
        command: WmCommand,
        expected_conflicts: &[(String, WmCommand)],
    ) -> Result<(), String> {
        let hotkey = self.hotkey(&key).ok_or_else(|| format!("“{key}” is not a valid shortcut"))?;
        let bindings = self.keymap(mode).ok_or("Shortcut mode no longer exists")?;
        if let Some((old, expected)) = old {
            if bindings.get(old) != Some(expected) {
                return Err("This shortcut changed elsewhere; try again".into());
            }
        }
        let conflicts: Vec<_> = bindings
            .iter()
            .filter(|(other, _)| {
                Some(other.as_str()) != old.map(|(old, _)| old)
                    && self.hotkey(other).is_some_and(|other| other.overlaps(&hotkey))
            })
            .map(|(key, command)| (key.clone(), command.clone()))
            .collect();
        if conflicts != expected_conflicts {
            return Err(
                "The conflicting shortcuts changed elsewhere. Record the keys again.".into(),
            );
        }
        let bindings = self.keymap_mut(mode)?;
        for (key, _) in expected_conflicts {
            bindings.remove(key);
        }
        if let Some((old, _)) = old {
            bindings.remove(old);
        }
        bindings.insert(key, command);
        Ok(())
    }

    fn check_keymap_name(&self, name: &str, old: Option<&str>) -> Result<(), String> {
        if name.is_empty()
            || name == "default"
            || self.binding_modes.contains_key(name) && old != Some(name)
        {
            Err("Choose a unique keymap name".into())
        } else {
            Ok(())
        }
    }

    pub fn create_keymap(&mut self, name: String) -> Result<(), String> {
        self.check_keymap_name(&name, None)?;
        self.binding_modes.insert(name, Default::default());
        Ok(())
    }

    pub fn rename_keymap(&mut self, old: &str, name: String) -> Result<(), String> {
        self.check_keymap_name(&name, Some(old))?;
        let bindings = self.binding_modes.remove(old).ok_or("Mode no longer exists")?;
        self.binding_modes.insert(name.clone(), bindings);
        self.rewrite_keymap_references(old, &name);
        Ok(())
    }

    pub fn delete_keymap(&mut self, name: &str) -> Result<(), String> {
        self.binding_modes.remove(name).ok_or("Mode no longer exists")?;
        self.rewrite_keymap_references(name, "default");
        Ok(())
    }

    fn rewrite_keymap_references(&mut self, old: &str, target: &str) {
        use wm_protocol::ReactorCommand;

        use crate::actor::reactor::Command;
        use crate::actor::wm_controller::WmCmd;
        fn rewrite(command: &mut WmCommand, old: &str, target: &str) {
            match command {
                WmCommand::Wm(WmCmd::BindingMode(mode))
                | WmCommand::ReactorCommand(Command::Reactor(ReactorCommand::BindingMode(mode)))
                    if mode == old =>
                {
                    target.clone_into(mode)
                }
                WmCommand::Sequence(commands) => {
                    commands.iter_mut().for_each(|command| rewrite(command, old, target))
                }
                _ => {}
            }
        }
        for command in self
            .keys
            .values_mut()
            .chain(self.binding_modes.values_mut().flat_map(|map| map.values_mut()))
        {
            rewrite(command, old, target);
        }
    }
}
