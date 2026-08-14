//! The normalized role vocabulary.
//!
//! AT-SPI publishes ~125 roles and macOS ~60 roles plus subroles. Agents should
//! reason about one vocabulary, so both are folded into the set below. Anything
//! unrecognised survives as [`Role::Other`] carrying the platform string — a
//! normalization layer that discards information is worse than none.

use std::borrow::Cow;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A normalized role.
///
/// Serialized as the same bare token [`Role::as_str`] renders and
/// [`Role::parse`] accepts, so the vocabulary an agent reads out of `--json`
/// is exactly the one it can pass back to `--role`. A derived
/// representation would drift: `TextBox` renders as `textbox` for humans but
/// would serialize as `text_box`, and `Other` would become a wrapper object
/// rather than the platform string it stands for.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum Role {
    Application,
    Window,
    Dialog,

    Button,
    ToggleButton,
    Switch,
    Link,
    MenuBar,
    Menu,
    MenuItem,
    CheckBox,
    RadioButton,
    ComboBox,
    ListBox,
    ListItem,
    Tab,
    TabList,
    TextBox,
    /// Values for this role are withheld unconditionally. See
    /// [`Role::is_secure`].
    PasswordField,
    SearchField,
    Slider,
    SpinButton,
    ProgressBar,
    ScrollBar,

    Table,
    Row,
    Cell,
    ColumnHeader,
    RowHeader,
    Tree,
    TreeItem,

    Toolbar,
    StatusBar,
    Label,
    Heading,
    Paragraph,
    Image,
    Icon,
    Canvas,
    Separator,
    Panel,
    ScrollArea,
    Group,
    Document,
    Terminal,
    Tooltip,
    Alert,
    Notification,
    DateField,
    ColorPicker,
    Calendar,
    Splitter,
    Filler,
    Unknown,

    /// A platform role with no normalized equivalent. The payload is the
    /// verbatim platform string.
    Other(String),
}

impl Role {
    /// The lowercase snake-case token used in snapshots and `--role` selectors.
    #[must_use]
    pub fn as_str(&self) -> Cow<'_, str> {
        match self {
            Self::Application => Cow::Borrowed("application"),
            Self::Window => Cow::Borrowed("window"),
            Self::Dialog => Cow::Borrowed("dialog"),
            Self::Button => Cow::Borrowed("button"),
            Self::ToggleButton => Cow::Borrowed("toggle_button"),
            Self::Switch => Cow::Borrowed("switch"),
            Self::Link => Cow::Borrowed("link"),
            Self::MenuBar => Cow::Borrowed("menubar"),
            Self::Menu => Cow::Borrowed("menu"),
            Self::MenuItem => Cow::Borrowed("menuitem"),
            Self::CheckBox => Cow::Borrowed("checkbox"),
            Self::RadioButton => Cow::Borrowed("radio"),
            Self::ComboBox => Cow::Borrowed("combobox"),
            Self::ListBox => Cow::Borrowed("list"),
            Self::ListItem => Cow::Borrowed("listitem"),
            Self::Tab => Cow::Borrowed("tab"),
            Self::TabList => Cow::Borrowed("tablist"),
            Self::TextBox => Cow::Borrowed("textbox"),
            Self::PasswordField => Cow::Borrowed("password"),
            Self::SearchField => Cow::Borrowed("searchbox"),
            Self::Slider => Cow::Borrowed("slider"),
            Self::SpinButton => Cow::Borrowed("spinbutton"),
            Self::ProgressBar => Cow::Borrowed("progressbar"),
            Self::ScrollBar => Cow::Borrowed("scrollbar"),
            Self::Table => Cow::Borrowed("table"),
            Self::Row => Cow::Borrowed("row"),
            Self::Cell => Cow::Borrowed("cell"),
            Self::ColumnHeader => Cow::Borrowed("columnheader"),
            Self::RowHeader => Cow::Borrowed("rowheader"),
            Self::Tree => Cow::Borrowed("tree"),
            Self::TreeItem => Cow::Borrowed("treeitem"),
            Self::Toolbar => Cow::Borrowed("toolbar"),
            Self::StatusBar => Cow::Borrowed("status"),
            Self::Label => Cow::Borrowed("label"),
            Self::Heading => Cow::Borrowed("heading"),
            Self::Paragraph => Cow::Borrowed("paragraph"),
            Self::Image => Cow::Borrowed("image"),
            Self::Icon => Cow::Borrowed("icon"),
            Self::Canvas => Cow::Borrowed("canvas"),
            Self::Separator => Cow::Borrowed("separator"),
            Self::Panel => Cow::Borrowed("panel"),
            Self::ScrollArea => Cow::Borrowed("scrollarea"),
            Self::Group => Cow::Borrowed("group"),
            Self::Document => Cow::Borrowed("document"),
            Self::Terminal => Cow::Borrowed("terminal"),
            Self::Tooltip => Cow::Borrowed("tooltip"),
            Self::Alert => Cow::Borrowed("alert"),
            Self::Notification => Cow::Borrowed("notification"),
            Self::DateField => Cow::Borrowed("datefield"),
            Self::ColorPicker => Cow::Borrowed("colorpicker"),
            Self::Calendar => Cow::Borrowed("calendar"),
            Self::Splitter => Cow::Borrowed("splitter"),
            Self::Filler => Cow::Borrowed("filler"),
            Self::Unknown => Cow::Borrowed("unknown"),
            Self::Other(raw) => Cow::Owned(raw.clone()),
        }
    }

    /// Roles an agent can act on. Drives snapshot retention: a tree is mostly
    /// layout scaffolding, and these are the nodes worth spending tokens on.
    #[must_use]
    pub const fn is_interactive(&self) -> bool {
        matches!(
            self,
            Self::Button
                | Self::ToggleButton
                | Self::Switch
                | Self::Link
                | Self::MenuItem
                | Self::CheckBox
                | Self::RadioButton
                | Self::ComboBox
                | Self::ListItem
                | Self::Tab
                | Self::TextBox
                | Self::PasswordField
                | Self::SearchField
                | Self::Slider
                | Self::SpinButton
                | Self::TreeItem
                | Self::Cell
                | Self::DateField
                | Self::ColorPicker
        )
    }

    /// Roles whose value must never leave the process, regardless of policy.
    #[must_use]
    pub const fn is_secure(&self) -> bool {
        matches!(self, Self::PasswordField)
    }

    /// Pure layout containers. Kept only when they carry a name or an action.
    #[must_use]
    pub const fn is_structural(&self) -> bool {
        matches!(
            self,
            Self::Panel
                | Self::Group
                | Self::Filler
                | Self::Separator
                | Self::ScrollArea
                | Self::Splitter
                | Self::Unknown
        )
    }

    /// Roles that carry meaning through their text content rather than a name.
    #[must_use]
    pub const fn is_textual(&self) -> bool {
        matches!(
            self,
            Self::Label | Self::Heading | Self::Paragraph | Self::StatusBar | Self::Terminal
        )
    }

    /// Parses the token emitted by [`Role::as_str`], for `--role` selectors.
    ///
    /// Shares `role_key` with the platform tables, so `--role "text box"`,
    /// `--role textbox` and `--role text_box` are the same query. Unknown
    /// tokens become [`Role::Other`] so a selector can still target a platform
    /// role the normalizer has not folded.
    #[must_use]
    pub fn parse(token: &str) -> Self {
        match role_key(token).as_str() {
            "application" => Self::Application,
            "window" => Self::Window,
            "dialog" => Self::Dialog,
            "button" | "pushbutton" => Self::Button,
            "togglebutton" => Self::ToggleButton,
            "switch" => Self::Switch,
            "link" => Self::Link,
            "menubar" => Self::MenuBar,
            "menu" => Self::Menu,
            "menuitem" => Self::MenuItem,
            "checkbox" => Self::CheckBox,
            "radio" | "radiobutton" => Self::RadioButton,
            "combobox" => Self::ComboBox,
            "list" | "listbox" => Self::ListBox,
            "listitem" => Self::ListItem,
            "tab" => Self::Tab,
            "tablist" => Self::TabList,
            "textbox" | "text" | "entry" | "textfield" | "textarea" => Self::TextBox,
            "password" | "passwordtext" | "passwordfield" => Self::PasswordField,
            "searchbox" | "searchfield" => Self::SearchField,
            "slider" => Self::Slider,
            "spinbutton" => Self::SpinButton,
            "progressbar" => Self::ProgressBar,
            "scrollbar" => Self::ScrollBar,
            "table" => Self::Table,
            "row" => Self::Row,
            "cell" => Self::Cell,
            "columnheader" => Self::ColumnHeader,
            "rowheader" => Self::RowHeader,
            "tree" => Self::Tree,
            "treeitem" => Self::TreeItem,
            "toolbar" => Self::Toolbar,
            "status" | "statusbar" => Self::StatusBar,
            "label" | "static" | "statictext" => Self::Label,
            "heading" => Self::Heading,
            "paragraph" => Self::Paragraph,
            "image" => Self::Image,
            "icon" => Self::Icon,
            "canvas" => Self::Canvas,
            "separator" => Self::Separator,
            "panel" | "tabpanel" => Self::Panel,
            "scrollarea" | "scrollpane" => Self::ScrollArea,
            "group" | "grouping" | "generic" => Self::Group,
            "document" => Self::Document,
            "terminal" => Self::Terminal,
            "tooltip" => Self::Tooltip,
            "alert" => Self::Alert,
            "notification" => Self::Notification,
            "datefield" | "dateeditor" => Self::DateField,
            "colorpicker" | "colorchooser" => Self::ColorPicker,
            "calendar" => Self::Calendar,
            "splitter" | "splitpane" => Self::Splitter,
            "filler" => Self::Filler,
            "unknown" => Self::Unknown,
            _ => Self::Other(token.trim().to_owned()),
        }
    }
}

impl Serialize for Role {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.as_str())
    }
}

impl<'de> Deserialize<'de> for Role {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let token = String::deserialize(deserializer)?;
        Ok(Self::parse(&token))
    }
}

impl schemars::JsonSchema for Role {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("Role")
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "description": "A normalized element role, e.g. `button`, `textbox`, \
                            `menuitem`. Unrecognised platform roles appear verbatim.",
        })
    }
}

/// Collapses a platform role name to a comparison key.
///
/// Toolkits spell the same role differently: the legacy ATK vocabulary says
/// `push button` and `page tab`, while GTK 4 emits ARIA-style names like
/// `text box` and `tab panel`. Stripping separators makes those the same key,
/// so a new toolkit spelling does not silently fall through to
/// [`Role::Other`] — which is how gnome-text-editor's editing area came back
/// as an unrecognised container.
fn role_key(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Maps an AT-SPI role name (as returned by `GetRoleName`) to a normalized role.
///
/// GTK 4 reports its editing areas as `text box`, and every ARIA landmark
/// collapses to a plain container: an agent gains nothing from distinguishing
/// `banner` from `contentinfo`.
#[must_use]
pub fn from_atspi(role_name: &str) -> Role {
    match role_key(role_name).as_str() {
        "application" => Role::Application,
        "frame" | "window" => Role::Window,
        "dialog" | "alertdialog" | "filechooser" | "colorchooser" | "fontchooser" => Role::Dialog,
        "pushbutton" | "button" | "pushbuttonmenu" => Role::Button,
        "togglebutton" => Role::ToggleButton,
        "switch" => Role::Switch,
        "link" => Role::Link,
        "menubar" => Role::MenuBar,
        "menu" | "popupmenu" => Role::Menu,
        "menuitem" | "checkmenuitem" | "radiomenuitem" | "tearoffmenuitem" => Role::MenuItem,
        "checkbox" => Role::CheckBox,
        "radiobutton" => Role::RadioButton,
        "combobox" => Role::ComboBox,
        "list" | "listbox" => Role::ListBox,
        "listitem" | "option" => Role::ListItem,
        "pagetab" | "tab" => Role::Tab,
        "pagetablist" | "tablist" => Role::TabList,
        "text" | "entry" | "editbar" | "documenttext" | "textbox" | "textarea" => Role::TextBox,
        "passwordtext" | "password" | "passwordbox" => Role::PasswordField,
        "searchbox" | "searchfield" => Role::SearchField,
        "slider" => Role::Slider,
        "spinbutton" | "dial" | "spinner" => Role::SpinButton,
        "progressbar" | "levelbar" | "meter" => Role::ProgressBar,
        "scrollbar" => Role::ScrollBar,
        "table" | "treetable" | "treegrid" | "grid" => Role::Table,
        "tablerow" | "row" => Role::Row,
        "tablecell" | "cell" | "gridcell" => Role::Cell,
        "tablecolumnheader" | "columnheader" => Role::ColumnHeader,
        "tablerowheader" | "rowheader" => Role::RowHeader,
        "tree" => Role::Tree,
        "treeitem" => Role::TreeItem,
        "toolbar" => Role::Toolbar,
        "statusbar" => Role::StatusBar,
        "label" | "static" | "acceleratorlabel" | "caption" => Role::Label,
        "heading" => Role::Heading,
        "paragraph" | "blockquote" => Role::Paragraph,
        "image" | "img" => Role::Image,
        "icon" | "desktopicon" => Role::Icon,
        "canvas" | "drawingarea" => Role::Canvas,
        "separator" => Role::Separator,
        "panel" | "rootpane" | "layeredpane" | "optionpane" | "glasspane" | "tabpanel" => {
            Role::Panel
        }
        "scrollpane" | "viewport" | "scrollarea" => Role::ScrollArea,
        "filler" => Role::Filler,
        "splitpane" => Role::Splitter,
        "grouping" | "group" | "section" | "generic" | "form" | "landmark" | "header"
        | "footer" | "banner" | "main" | "navigation" | "region" | "complementary"
        | "contentinfo" => Role::Group,
        "documentframe" | "documentweb" | "document" | "htmlcontainer" => Role::Document,
        "terminal" => Role::Terminal,
        "tooltip" => Role::Tooltip,
        "alert" => Role::Alert,
        "notification" => Role::Notification,
        "dateeditor" | "datetime" => Role::DateField,
        "calendar" => Role::Calendar,
        "unknown" | "invalid" | "redundantobject" => Role::Unknown,
        _ => Role::Other(role_name.trim().to_owned()),
    }
}

/// Maps a macOS `AXRole` (plus optional `AXSubrole`) to a normalized role.
///
/// The subrole is consulted first because it is what distinguishes a secure
/// text field from an ordinary one — the single most security-relevant
/// distinction in the whole mapping.
#[must_use]
pub fn from_ax(role: &str, subrole: Option<&str>) -> Role {
    if let Some(sub) = subrole.map(str::trim).filter(|s| !s.is_empty()) {
        match sub {
            "AXSecureTextField" => return Role::PasswordField,
            "AXSearchField" => return Role::SearchField,
            "AXToggle" => return Role::ToggleButton,
            "AXSwitch" => return Role::Switch,
            "AXTabButton" => return Role::Tab,
            "AXOutlineRow" => return Role::TreeItem,
            "AXTableRow" => return Role::Row,
            "AXDialog" | "AXSystemDialog" | "AXFloatingWindow" => return Role::Dialog,
            "AXStandardWindow" => return Role::Window,
            _ => {}
        }
    }

    match role.trim() {
        "AXApplication" => Role::Application,
        "AXWindow" | "AXSystemWide" => Role::Window,
        "AXSheet" | "AXDrawer" => Role::Dialog,
        "AXButton"
        | "AXToolbarButton"
        | "AXCloseButton"
        | "AXMinimizeButton"
        | "AXZoomButton"
        | "AXFullScreenButton"
        | "AXSortButton"
        | "AXDisclosureTriangle" => Role::Button,
        "AXPopUpButton" | "AXMenuButton" => Role::ComboBox,
        "AXSwitch" => Role::Switch,
        "AXLink" => Role::Link,
        "AXMenuBar" => Role::MenuBar,
        "AXMenu" => Role::Menu,
        "AXMenuItem" | "AXMenuBarItem" => Role::MenuItem,
        "AXCheckBox" => Role::CheckBox,
        "AXRadioButton" => Role::RadioButton,
        "AXComboBox" => Role::ComboBox,
        "AXList" => Role::ListBox,
        "AXTabGroup" => Role::TabList,
        "AXTextField" | "AXTextArea" => Role::TextBox,
        "AXSlider" => Role::Slider,
        "AXIncrementor" | "AXStepper" => Role::SpinButton,
        "AXProgressIndicator"
        | "AXLevelIndicator"
        | "AXBusyIndicator"
        | "AXRelevanceIndicator"
        | "AXValueIndicator" => Role::ProgressBar,
        "AXScrollBar" => Role::ScrollBar,
        "AXTable" | "AXGrid" => Role::Table,
        "AXRow" => Role::Row,
        "AXCell" => Role::Cell,
        "AXColumn" => Role::ColumnHeader,
        "AXOutline" => Role::Tree,
        "AXToolbar" => Role::Toolbar,
        "AXStaticText" => Role::Label,
        "AXHeading" => Role::Heading,
        "AXImage" => Role::Image,
        "AXSeparator" | "AXSplitter" => Role::Separator,
        "AXGroup" | "AXRadioGroup" | "AXMatte" | "AXGrowArea" => Role::Group,
        "AXScrollArea" => Role::ScrollArea,
        "AXSplitGroup" => Role::Splitter,
        "AXWebArea" => Role::Document,
        "AXHelpTag" => Role::Tooltip,
        "AXDateField" | "AXTimeField" => Role::DateField,
        "AXColorWell" => Role::ColorPicker,
        "AXRuler" | "AXUnknown" => Role::Unknown,
        other => Role::Other(other.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atspi_password_text_maps_to_the_secure_role() {
        assert_eq!(from_atspi("password text"), Role::PasswordField);
        assert!(from_atspi("password text").is_secure());
    }

    #[test]
    fn macos_secure_text_field_subrole_overrides_its_ordinary_role() {
        // The role alone says AXTextField; only the subrole reveals it is secure.
        assert_eq!(
            from_ax("AXTextField", Some("AXSecureTextField")),
            Role::PasswordField
        );
        assert_eq!(from_ax("AXTextField", None), Role::TextBox);
    }

    #[test]
    fn only_the_password_role_is_secure() {
        let secure: Vec<Role> = [
            Role::Button,
            Role::TextBox,
            Role::SearchField,
            Role::PasswordField,
            Role::Label,
        ]
        .into_iter()
        .filter(Role::is_secure)
        .collect();
        assert_eq!(secure, vec![Role::PasswordField]);
    }

    #[test]
    fn both_platforms_agree_on_the_button_role() {
        assert_eq!(from_atspi("push button"), Role::Button);
        assert_eq!(from_ax("AXButton", None), Role::Button);
    }

    #[test]
    fn both_platforms_agree_on_the_core_interactive_vocabulary() {
        let pairs: &[(&str, &str, Role)] = &[
            ("push button", "AXButton", Role::Button),
            ("check box", "AXCheckBox", Role::CheckBox),
            ("radio button", "AXRadioButton", Role::RadioButton),
            ("combo box", "AXComboBox", Role::ComboBox),
            ("slider", "AXSlider", Role::Slider),
            ("menu item", "AXMenuItem", Role::MenuItem),
            ("menu bar", "AXMenuBar", Role::MenuBar),
            ("page tab list", "AXTabGroup", Role::TabList),
            ("tool bar", "AXToolbar", Role::Toolbar),
            ("label", "AXStaticText", Role::Label),
            ("entry", "AXTextField", Role::TextBox),
            ("password text", "AXSecureTextField", Role::PasswordField),
        ];
        for (atspi, ax, expected) in pairs {
            assert_eq!(from_atspi(atspi), *expected, "at-spi role {atspi}");
            let ax_role = if *ax == "AXSecureTextField" {
                from_ax("AXTextField", Some(ax))
            } else {
                from_ax(ax, None)
            };
            assert_eq!(ax_role, *expected, "ax role {ax}");
        }
    }

    #[test]
    fn unrecognised_platform_roles_survive_verbatim_rather_than_collapsing() {
        assert_eq!(
            from_atspi("content insertion"),
            Role::Other("content insertion".to_owned())
        );
        assert_eq!(
            from_ax("AXBrandNewControl", None),
            Role::Other("AXBrandNewControl".to_owned())
        );
    }

    #[test]
    fn atspi_role_names_are_matched_case_insensitively() {
        assert_eq!(from_atspi("PUSH BUTTON"), Role::Button);
        assert_eq!(from_atspi("  push button  "), Role::Button);
    }

    #[test]
    fn gtk4_aria_style_role_names_are_recognised() {
        // Probed on gnome-text-editor under GNOME 49: GTK 4 emits ARIA-style
        // names, not the legacy ATK ones. `text box` falling through to
        // Role::Other made the editing area invisible to snapshots.
        assert_eq!(from_atspi("text box"), Role::TextBox);
        assert_eq!(from_atspi("tab panel"), Role::Panel);
        assert_eq!(from_atspi("toggle button"), Role::ToggleButton);
    }

    #[test]
    fn legacy_and_modern_spellings_of_the_same_role_agree() {
        for (legacy, modern) in [
            ("push button", "button"),
            ("page tab", "tab"),
            ("page tab list", "tab list"),
            ("text", "text box"),
            ("tree table", "tree grid"),
        ] {
            assert_eq!(
                from_atspi(legacy),
                from_atspi(modern),
                "{legacy:?} and {modern:?} should normalize alike"
            );
        }
    }

    #[test]
    fn role_matching_ignores_separators_entirely() {
        for spelling in ["text box", "textbox", "text_box", "Text-Box", "TEXTBOX"] {
            assert_eq!(from_atspi(spelling), Role::TextBox, "{spelling:?}");
            assert_eq!(Role::parse(spelling), Role::TextBox, "{spelling:?}");
        }
    }

    #[test]
    fn generic_gtk4_containers_normalize_to_group() {
        // Probed on GNOME 49: GTK4 apps emit nested unnamed `generic` nodes.
        assert_eq!(from_atspi("generic"), Role::Group);
        assert!(Role::Group.is_structural());
    }

    #[test]
    fn as_str_round_trips_through_parse_for_every_normalized_role() {
        let all = [
            Role::Application,
            Role::Window,
            Role::Dialog,
            Role::Button,
            Role::ToggleButton,
            Role::Switch,
            Role::Link,
            Role::MenuBar,
            Role::Menu,
            Role::MenuItem,
            Role::CheckBox,
            Role::RadioButton,
            Role::ComboBox,
            Role::ListBox,
            Role::ListItem,
            Role::Tab,
            Role::TabList,
            Role::TextBox,
            Role::PasswordField,
            Role::SearchField,
            Role::Slider,
            Role::SpinButton,
            Role::ProgressBar,
            Role::ScrollBar,
            Role::Table,
            Role::Row,
            Role::Cell,
            Role::ColumnHeader,
            Role::RowHeader,
            Role::Tree,
            Role::TreeItem,
            Role::Toolbar,
            Role::StatusBar,
            Role::Label,
            Role::Heading,
            Role::Paragraph,
            Role::Image,
            Role::Icon,
            Role::Canvas,
            Role::Separator,
            Role::Panel,
            Role::ScrollArea,
            Role::Group,
            Role::Document,
            Role::Terminal,
            Role::Tooltip,
            Role::Alert,
            Role::Notification,
            Role::DateField,
            Role::ColorPicker,
            Role::Calendar,
            Role::Splitter,
            Role::Filler,
            Role::Unknown,
        ];
        for role in all {
            let token = role.as_str().into_owned();
            assert_eq!(Role::parse(&token), role, "token {token}");
        }
    }

    #[test]
    fn selector_role_parsing_accepts_the_spellings_an_agent_would_guess() {
        assert_eq!(Role::parse("button"), Role::Button);
        assert_eq!(Role::parse("Button"), Role::Button);
        assert_eq!(Role::parse("push button"), Role::Button);
        assert_eq!(Role::parse("check-box"), Role::CheckBox);
        assert_eq!(Role::parse("text_field"), Role::TextBox);
    }

    #[test]
    fn json_uses_the_same_vocabulary_the_selector_accepts() {
        // An agent reads a role out of --json and passes it straight back to
        // --role; a derived representation would give it `text_box` for
        // something it must ask for as `textbox`.
        for role in [
            Role::TextBox,
            Role::PasswordField,
            Role::MenuItem,
            Role::StatusBar,
            Role::ToggleButton,
        ] {
            let json = serde_json::to_string(&role).expect("serializes");
            let token = json.trim_matches('"');
            assert_eq!(token, role.as_str(), "wire form must match as_str");
            assert_eq!(Role::parse(token), role, "wire form must parse back");
        }
    }

    #[test]
    fn an_unrecognised_role_serializes_as_the_platform_string_itself() {
        let json =
            serde_json::to_string(&Role::Other("AXBrandNew".to_owned())).expect("serializes");
        assert_eq!(json, r#""AXBrandNew""#);
    }

    #[test]
    fn every_role_survives_a_json_round_trip() {
        for role in [
            Role::Button,
            Role::TextBox,
            Role::PasswordField,
            Role::Other("weird thing".to_owned()),
        ] {
            let json = serde_json::to_string(&role).expect("serializes");
            let back: Role = serde_json::from_str(&json).expect("deserializes");
            assert_eq!(back, role);
        }
    }
}
