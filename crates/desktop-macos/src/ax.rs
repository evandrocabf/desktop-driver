//! Safe wrappers over `AXUIElement`.
//!
//! All of this crate's `unsafe` lives here and in [`crate::input`]. The AX API
//! is a C API over `CFTypeRef`s with copy semantics; every call below hands
//! back an owned `CFRetained`, so nothing escapes as a raw pointer.
//!
//! Two behaviours are deliberate rather than incidental:
//!
//! * A messaging timeout is set on every element. The AX default is generous,
//!   and a hung target application would otherwise block the CLI forever
//!   instead of failing.
//! * Attributes are read individually but only after the element's attribute
//!   *names* are known, so a missing attribute costs nothing.

use std::ptr::NonNull;

use objc2_application_services::{AXError, AXUIElement, AXValue, AXValueType};
use objc2_core_foundation::{
    CFArray, CFBoolean, CFNumber, CFRetained, CFString, CFType, CGPoint, CGSize,
};

use desktop_core::{
    errors::{DesktopError, Result},
    models::geometry::Bounds,
};

/// How long to wait for one AX message before giving up.
///
/// Without this a single unresponsive application hangs the whole command. Two
/// seconds is long enough for a busy app to answer and short enough that a
/// wedged one is reported rather than waited on.
const MESSAGING_TIMEOUT_SECONDS: f32 = 2.0;

/// An accessibility element.
pub struct Element {
    inner: CFRetained<AXUIElement>,
}

impl Element {
    /// The element for a running application, by pid.
    #[must_use]
    pub fn for_application(pid: i32) -> Self {
        // SAFETY: `AXUIElementCreateApplication` accepts any pid and returns a
        // retained element; an invalid pid yields an element whose queries all
        // fail, which the callers handle.
        let inner = unsafe { AXUIElement::new_application(pid) };
        Self::from_retained(inner)
    }

    fn from_retained(inner: CFRetained<AXUIElement>) -> Self {
        // The timeout belongs to one AXUIElementRef, not to the target process.
        // Child and focused-window references therefore need it just as much as
        // the application root; otherwise a single hung control can still
        // block the CLI indefinitely.
        // SAFETY: `inner` is live and the timeout is finite and positive.
        let _ = unsafe { inner.set_messaging_timeout(MESSAGING_TIMEOUT_SECONDS) };
        Self { inner }
    }

    /// Reads an attribute as an untyped Core Foundation value.
    fn attribute(&self, name: &str) -> Option<CFRetained<CFType>> {
        let key = CFString::from_str(name);
        let mut raw: *const CFType = std::ptr::null();
        // SAFETY: `raw` is a valid, properly aligned pointer to a nullable
        // `*const CFType`, which is exactly what the API writes through.
        let error = unsafe {
            self.inner
                .copy_attribute_value(&key, NonNull::from(&mut raw))
        };
        if error != AXError::Success || raw.is_null() {
            return None;
        }
        // SAFETY: the API returned success with a non-null pointer, so `raw`
        // is a +1 reference we now own.
        Some(unsafe { CFRetained::from_raw(NonNull::new(raw.cast_mut())?) })
    }

    /// Reads a string attribute.
    #[must_use]
    pub fn string(&self, name: &str) -> Option<String> {
        let value = self.attribute(name)?;
        value.downcast_ref::<CFString>().map(CFString::to_string)
    }

    /// Reads a boolean attribute.
    ///
    /// Absent is `None` rather than `false`: "this element does not report
    /// whether it is enabled" is different from "it is disabled", and
    /// collapsing them marks whole applications unusable.
    #[must_use]
    pub fn boolean(&self, name: &str) -> Option<bool> {
        let value = self.attribute(name)?;
        if let Some(flag) = value.downcast_ref::<CFBoolean>() {
            return Some(flag.value());
        }
        value
            .downcast_ref::<CFNumber>()
            .and_then(CFNumber::as_i64)
            .map(|number| number != 0)
    }

    /// Reads a value attribute as a display string, whatever its underlying
    /// type. AX values are variously strings, numbers or booleans depending on
    /// the control.
    #[must_use]
    pub fn value_string(&self, name: &str) -> Option<String> {
        let value = self.attribute(name)?;
        if let Some(text) = value.downcast_ref::<CFString>() {
            return Some(text.to_string());
        }
        if let Some(flag) = value.downcast_ref::<CFBoolean>() {
            return Some(flag.value().to_string());
        }
        if let Some(number) = value.downcast_ref::<CFNumber>() {
            return number
                .as_i64()
                .map(|n| n.to_string())
                .or_else(|| number.as_f64().map(|n| format!("{n}")));
        }
        None
    }

    /// The element's on-screen rectangle, in points.
    ///
    /// macOS reports position and size separately, each boxed in an `AXValue`.
    #[must_use]
    pub fn bounds(&self) -> Option<Bounds> {
        let position = self.point(crate::ax_constants::attribute::POSITION)?;
        let size = self.size(crate::ax_constants::attribute::SIZE)?;
        Some(Bounds::new(
            position.x as i32,
            position.y as i32,
            size.width as i32,
            size.height as i32,
        ))
    }

    fn point(&self, name: &str) -> Option<CGPoint> {
        let value = self.attribute(name)?;
        let boxed = value.downcast_ref::<AXValue>()?;
        let mut out = CGPoint { x: 0.0, y: 0.0 };
        // SAFETY: the out-pointer matches the requested `CGPoint` type, and the
        // call writes only on success.
        let ok = unsafe {
            boxed.value(
                AXValueType::CGPoint,
                NonNull::new(&mut out as *mut CGPoint)
                    .expect("address of a local is never null")
                    .cast(),
            )
        };
        ok.then_some(out)
    }

    fn size(&self, name: &str) -> Option<CGSize> {
        let value = self.attribute(name)?;
        let boxed = value.downcast_ref::<AXValue>()?;
        let mut out = CGSize {
            width: 0.0,
            height: 0.0,
        };
        // SAFETY: as above, with the matching `CGSize` type.
        let ok = unsafe {
            boxed.value(
                AXValueType::CGSize,
                NonNull::new(&mut out as *mut CGSize)
                    .expect("address of a local is never null")
                    .cast(),
            )
        };
        ok.then_some(out)
    }

    /// Child elements.
    #[must_use]
    pub fn children(&self) -> Vec<Self> {
        self.element_array(crate::ax_constants::attribute::CHILDREN)
    }

    /// Child elements, capped before the target application constructs the
    /// result array.
    ///
    /// Large tables can expose hundreds of thousands of AX children. Reading
    /// the whole `AXChildren` value and truncating it afterwards defeats the
    /// walk budget and can stall or exhaust the CLI. Apple's ranged API keeps
    /// the budget effective at the process boundary. Custom AX providers that
    /// do not implement the ranged call retain the old full-array fallback.
    #[must_use]
    pub fn children_limited(&self, limit: usize) -> Vec<Self> {
        self.element_array_limited(crate::ax_constants::attribute::CHILDREN, limit)
    }

    /// Top-level windows, for an application element.
    #[must_use]
    pub fn windows(&self) -> Vec<Self> {
        self.element_array(crate::ax_constants::attribute::WINDOWS)
    }

    fn element_array(&self, name: &str) -> Vec<Self> {
        let Some(value) = self.attribute(name) else {
            return Vec::new();
        };
        let Some(array) = value.downcast_ref::<CFArray>() else {
            return Vec::new();
        };

        Self::elements_from_array(array)
    }

    fn element_array_limited(&self, name: &str, limit: usize) -> Vec<Self> {
        if limit == 0 {
            return Vec::new();
        }

        let key = CFString::from_str(name);
        let mut raw: *const CFArray = std::ptr::null();
        let max_values = isize::try_from(limit).unwrap_or(isize::MAX);
        // SAFETY: `raw` is a valid out pointer, the range starts at zero, and
        // `max_values` is positive. The returned array, on success, is +1.
        let error = unsafe {
            self.inner
                .copy_attribute_values(&key, 0, max_values, NonNull::from(&mut raw))
        };
        if error != AXError::Success || raw.is_null() {
            return self.element_array(name).into_iter().take(limit).collect();
        }
        // SAFETY: success with a non-null pointer transfers one owned
        // reference to the caller.
        let Some(pointer) = NonNull::new(raw.cast_mut()) else {
            return Vec::new();
        };
        let array = unsafe { CFRetained::<CFArray>::from_raw(pointer) };
        Self::elements_from_array(&array)
    }

    fn elements_from_array(array: &CFArray) -> Vec<Self> {
        let count = array.count();
        let mut out = Vec::with_capacity(count.max(0) as usize);
        for index in 0..count {
            // SAFETY: `index` is within `0..count` and the array is live.
            let raw = unsafe { array.value_at_index(index) };
            let Some(pointer) = NonNull::new(raw.cast_mut()) else {
                continue;
            };
            // SAFETY: CFArray gives back a borrowed (+0) CFType reference, so
            // it is retained before the checked downcast takes ownership.
            let value = unsafe { CFRetained::retain(pointer.cast::<CFType>()) };
            let Ok(element) = value.downcast::<AXUIElement>() else {
                continue;
            };
            out.push(Self::from_retained(element));
        }
        out
    }

    /// A single element-valued attribute, such as the focused window.
    #[must_use]
    pub fn element(&self, name: &str) -> Option<Self> {
        let value = self.attribute(name)?;
        let element = value.downcast::<AXUIElement>().ok()?;
        Some(Self::from_retained(element))
    }

    /// The action names this element advertises.
    #[must_use]
    pub fn action_names(&self) -> Vec<String> {
        let mut raw: *const CFArray = std::ptr::null();
        // SAFETY: `raw` is a valid pointer to a nullable `*const CFArray`.
        let error = unsafe { self.inner.copy_action_names(NonNull::from(&mut raw)) };
        if error != AXError::Success || raw.is_null() {
            return Vec::new();
        }
        // SAFETY: success with a non-null pointer means we own a +1 reference.
        let array = unsafe {
            match NonNull::new(raw.cast_mut()) {
                Some(pointer) => CFRetained::<CFArray>::from_raw(pointer),
                None => return Vec::new(),
            }
        };

        let count = array.count();
        let mut out = Vec::with_capacity(count.max(0) as usize);
        for index in 0..count {
            // SAFETY: `index` is in range and the array holds CFStrings.
            let raw = unsafe { array.value_at_index(index) };
            let Some(pointer) = NonNull::new(raw.cast_mut()) else {
                continue;
            };
            // SAFETY: borrowed reference from CFArray, retained before a
            // checked downcast. A malformed provider cannot make us treat an
            // arbitrary CF object as a string.
            let value = unsafe { CFRetained::retain(pointer.cast::<CFType>()) };
            let Ok(text) = value.downcast::<CFString>() else {
                continue;
            };
            out.push(text.to_string());
        }
        out
    }

    /// Whether an attribute can be changed on this exact element.
    #[must_use]
    pub fn is_settable(&self, name: &str) -> bool {
        let key = CFString::from_str(name);
        let mut settable = 0;
        // SAFETY: `settable` is a valid out pointer and `key` is live.
        let result = unsafe {
            self.inner
                .is_attribute_settable(&key, NonNull::from(&mut settable))
        };
        result == AXError::Success && settable != 0
    }

    /// Performs an action by its `AX` name.
    pub fn perform(&self, action: &str) -> Result<()> {
        let name = CFString::from_str(action);
        // SAFETY: `name` is a live CFString for the duration of the call.
        let error = unsafe { self.inner.perform_action(&name) };
        match error {
            AXError::Success => Ok(()),
            AXError::CannotComplete => Err(DesktopError::backend(
                "the application did not respond to the accessibility action",
            )),
            AXError::ActionUnsupported => Err(DesktopError::invalid_argument(format!(
                "element does not support the {action} action"
            ))),
            other => Err(DesktopError::backend(format!(
                "accessibility action failed ({other:?})"
            ))),
        }
    }

    /// Sets a string attribute, such as `AXValue` on a text field.
    pub fn set_string(&self, name: &str, value: &str) -> Result<()> {
        if !self.is_settable(name) {
            return Err(DesktopError::invalid_argument(format!(
                "this element does not accept text through {name}"
            )));
        }
        let key = CFString::from_str(name);
        let text = CFString::from_str(value);
        // SAFETY: both the key and the value are live for the call.
        let error = unsafe { self.inner.set_attribute_value(&key, text.as_ref()) };
        match error {
            AXError::Success => Ok(()),
            AXError::AttributeUnsupported | AXError::IllegalArgument => {
                Err(DesktopError::invalid_argument(format!(
                    "this element does not accept text through {name}"
                )))
            }
            other => Err(DesktopError::backend(format!(
                "cannot set {name} ({other:?})"
            ))),
        }
    }

    /// Sets a boolean attribute, such as `AXFrontmost`.
    pub fn set_boolean(&self, name: &str, value: bool) -> Result<()> {
        let key = CFString::from_str(name);
        let flag = CFBoolean::new(value);
        // SAFETY: both the key and the value are live for the call.
        let error = unsafe { self.inner.set_attribute_value(&key, flag.as_ref()) };
        match error {
            AXError::Success => Ok(()),
            AXError::AttributeUnsupported => Err(DesktopError::invalid_argument(format!(
                "element does not support the {name} attribute"
            ))),
            other => Err(DesktopError::backend(format!(
                "cannot set {name} ({other:?})"
            ))),
        }
    }
}

/// Whether this process may use the accessibility API.
///
/// Never cached: the trust state changes when the user toggles the switch in
/// System Settings, and a long-lived answer would go stale in the direction
/// that reports a working setup as broken.
#[must_use]
pub fn is_trusted() -> bool {
    // SAFETY: passing no options performs a check without prompting.
    unsafe { objc2_application_services::AXIsProcessTrusted() }
}

/// Checks trust, optionally showing the system prompt.
#[must_use]
pub fn is_trusted_with_prompt(prompt: bool) -> bool {
    if !prompt {
        return is_trusted();
    }
    use objc2_core_foundation::CFDictionary;

    let key = unsafe { objc2_application_services::kAXTrustedCheckOptionPrompt };
    let value = CFBoolean::new(true);
    let options =
        CFDictionary::from_slices(&[key.as_ref() as &CFType], &[value.as_ref() as &CFType]);
    // SAFETY: the dictionary is live for the duration of the call.
    unsafe { objc2_application_services::AXIsProcessTrustedWithOptions(Some(options.as_ref())) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_messaging_timeout_is_bounded_so_a_hung_app_cannot_wedge_the_cli() {
        const { assert!(MESSAGING_TIMEOUT_SECONDS > 0.0) };
        const { assert!(MESSAGING_TIMEOUT_SECONDS <= 5.0) };
    }
}
