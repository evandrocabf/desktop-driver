//! The `kAX*` attribute, role and action names.
//!
//! Apple defines these as `CFSTR("…")` preprocessor macros, which objc2's
//! header translator cannot emit — the generated `AXAttributeConstants.rs`,
//! `AXRoleConstants.rs` and `AXActionConstants.rs` in
//! `objc2-application-services` 0.3.2 contain zero constants between them.
//!
//! They are plain ASCII strings with a stable public contract, so redeclaring
//! them here is safe. Keeping them in one place also means a typo shows up as
//! a compile error at the call site rather than as an attribute that silently
//! never matches.
//!
//! `AXSecureTextField` is deliberately *not* declared here. The subrole is the
//! only thing distinguishing a password field from an ordinary text field, and
//! the decision to redact belongs in one place — `desktop_core::models::role`,
//! where both platforms converge on `Role::PasswordField`.

/// Accessibility attribute names.
pub mod attribute {
    pub const ROLE: &str = "AXRole";
    pub const SUBROLE: &str = "AXSubrole";
    pub const TITLE: &str = "AXTitle";
    pub const DESCRIPTION: &str = "AXDescription";
    pub const HELP: &str = "AXHelp";
    pub const VALUE: &str = "AXValue";
    pub const PLACEHOLDER_VALUE: &str = "AXPlaceholderValue";
    pub const TITLE_UI_ELEMENT: &str = "AXTitleUIElement";
    pub const CHILDREN: &str = "AXChildren";
    pub const WINDOWS: &str = "AXWindows";
    pub const MAIN_WINDOW: &str = "AXMainWindow";
    pub const FOCUSED_WINDOW: &str = "AXFocusedWindow";
    pub const FOCUSED: &str = "AXFocused";
    pub const ENABLED: &str = "AXEnabled";
    pub const SELECTED: &str = "AXSelected";
    pub const EXPANDED: &str = "AXExpanded";
    pub const DISCLOSING: &str = "AXDisclosing";
    pub const POSITION: &str = "AXPosition";
    pub const SIZE: &str = "AXSize";
    pub const FRONTMOST: &str = "AXFrontmost";
    pub const MINIMIZED: &str = "AXMinimized";
    pub const HIDDEN: &str = "AXHidden";
}

/// Accessibility action names.
///
/// Only actions this crate names directly live here. The rest are matched by
/// string in `desktop_core::models::element::ElementAction::from_platform`,
/// which owns the whole platform-to-normalized action mapping.
pub mod action {
    pub const RAISE: &str = "AXRaise";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attribute_names_carry_the_ax_prefix_apple_uses() {
        for name in [
            attribute::ROLE,
            attribute::TITLE,
            attribute::CHILDREN,
            attribute::VALUE,
            attribute::PLACEHOLDER_VALUE,
            attribute::TITLE_UI_ELEMENT,
            attribute::POSITION,
            attribute::SIZE,
        ] {
            assert!(name.starts_with("AX"), "{name} is not an AX attribute name");
            assert!(name.is_ascii(), "{name} must be ASCII");
        }
    }

    #[test]
    fn action_names_carry_the_ax_prefix_apple_uses() {
        assert!(action::RAISE.starts_with("AX"));
    }

    #[test]
    fn no_two_attributes_share_a_name() {
        let names = [
            attribute::ROLE,
            attribute::SUBROLE,
            attribute::TITLE,
            attribute::DESCRIPTION,
            attribute::VALUE,
            attribute::PLACEHOLDER_VALUE,
            attribute::TITLE_UI_ELEMENT,
            attribute::CHILDREN,
            attribute::WINDOWS,
            attribute::POSITION,
            attribute::SIZE,
            attribute::EXPANDED,
            attribute::DISCLOSING,
        ];
        let mut sorted = names.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "duplicate attribute name");
    }
}
