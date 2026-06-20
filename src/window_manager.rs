//! Linking macOS windows to workspaces and raising them on demand.
//!
//! macOS has no public, serializable handle for "a specific window of another
//! app", so we identify windows by stable attributes — the owning application
//! name, the window title, and (for document apps like Xcode) the open
//! document path — and re-resolve them against the live windows whenever a
//! workspace is activated.
//!
//! Enumeration uses the **CoreGraphics window list** (front-to-back z-order,
//! owner PID + name; no Screen Recording permission needed for those fields)
//! combined with the **Accessibility API** for titles, document paths, and the
//! actual raise. Raising a specific window is `AXRaise` + `kAXMain` on the
//! window plus `kAXFrontmost` on its application. All of this requires the app
//! to be a trusted Accessibility client.

use crate::workspace::LinkedWindow;

/// A live, enumerable window belonging to some running application.
#[derive(Debug, Clone)]
pub struct WindowInfo {
    pub pid: i32,
    pub app_name: String,
    pub title: String,
    pub document_path: Option<String>,
}

impl WindowInfo {
    /// Whether this live window is the one a stored link describes.
    pub fn matches(&self, link: &LinkedWindow) -> bool {
        self.app_name == link.app_name
            && self.title == link.title
            && self.document_path == link.document_path
    }

    /// A persistable descriptor for this window.
    pub fn to_link(&self) -> LinkedWindow {
        LinkedWindow {
            app_name: self.app_name.clone(),
            title: self.title.clone(),
            document_path: self.document_path.clone(),
        }
    }
}

/// Outcome of attempting to raise a workspace's linked windows.
#[derive(Debug, Default)]
pub struct RaiseReport {
    pub raised: usize,
    /// Human-readable labels of links that no live window matched.
    pub unresolved: Vec<String>,
}

/// Is this process a trusted Accessibility client? When `prompt` is true and it
/// isn't, macOS shows the system "grant access" dialog.
pub fn accessibility_trusted(prompt: bool) -> bool {
    imp::accessibility_trusted(prompt)
}

/// Open System Settings at Privacy & Security ▸ Accessibility.
pub fn open_accessibility_settings() {
    imp::open_accessibility_settings();
}

/// All linkable windows, ordered frontmost-app first.
pub fn list_windows() -> Vec<WindowInfo> {
    imp::list_windows()
}

/// Re-resolve each link against live windows and raise the matches.
pub fn raise_windows(links: &[LinkedWindow]) -> RaiseReport {
    imp::raise_windows(links)
}

#[cfg(target_os = "macos")]
mod imp {
    use std::collections::{HashMap, HashSet};
    use std::os::raw::c_void;
    use std::ptr;

    use accessibility_sys::{
        kAXDocumentAttribute, kAXErrorSuccess, kAXFrontmostAttribute, kAXMainAttribute,
        kAXRaiseAction, kAXTitleAttribute, kAXTrustedCheckOptionPrompt, kAXWindowsAttribute,
        AXIsProcessTrusted, AXIsProcessTrustedWithOptions, AXUIElementCopyAttributeValue,
        AXUIElementCreateApplication, AXUIElementPerformAction, AXUIElementRef,
        AXUIElementSetAttributeValue, AXUIElementSetMessagingTimeout,
    };
    use core_foundation::base::{CFType, TCFType};
    use core_foundation::boolean::CFBoolean;
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::number::CFNumber;
    use core_foundation::string::CFString;
    use core_foundation_sys::array::{CFArrayGetCount, CFArrayGetValueAtIndex, CFArrayRef};
    use core_foundation_sys::base::{CFRelease, CFTypeRef};
    use core_foundation_sys::dictionary::{CFDictionaryGetValue, CFDictionaryRef};
    use core_foundation_sys::number::CFNumberRef;
    use core_foundation_sys::string::CFStringRef;
    use core_graphics::window::{
        copy_window_info, kCGNullWindowID, kCGWindowLayer, kCGWindowListExcludeDesktopElements,
        kCGWindowListOptionOnScreenOnly, kCGWindowOwnerName, kCGWindowOwnerPID,
    };

    use super::{RaiseReport, WindowInfo};
    use crate::workspace::LinkedWindow;

    /// Bound AX requests so an unresponsive app can't hang the UI.
    const AX_TIMEOUT_SECS: f32 = 0.25;

    pub fn accessibility_trusted(prompt: bool) -> bool {
        unsafe {
            if !prompt {
                return AXIsProcessTrusted();
            }
            let key = CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt);
            let val = CFBoolean::true_value();
            let options = CFDictionary::from_CFType_pairs(&[(key.as_CFType(), val.as_CFType())]);
            AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef())
        }
    }

    pub fn open_accessibility_settings() {
        let _ = std::process::Command::new("open")
            .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
            .spawn();
    }

    pub fn list_windows() -> Vec<WindowInfo> {
        let self_pid = std::process::id() as i32;
        let ordered = cg_apps_front_to_back(self_pid);

        let mut out = Vec::new();
        for (pid, app_name) in ordered {
            for (title, document_path) in unsafe { app_window_infos(pid) } {
                // Skip chrome with neither a title nor a document (palettes, sheets).
                if title.trim().is_empty() && document_path.is_none() {
                    continue;
                }
                out.push(WindowInfo {
                    pid,
                    app_name: app_name.clone(),
                    title,
                    document_path,
                });
            }
        }
        out
    }

    pub fn raise_windows(links: &[LinkedWindow]) -> RaiseReport {
        let self_pid = std::process::id() as i32;
        let name_to_pids = cg_name_to_pids(self_pid);
        let mut report = RaiseReport::default();

        // Raise in reverse so the first-listed link ends up frontmost.
        for link in links.iter().rev() {
            let raised = name_to_pids
                .get(&link.app_name)
                .into_iter()
                .flatten()
                .any(|&pid| unsafe { raise_in_app(pid, link) });

            if raised {
                report.raised += 1;
            } else {
                let title = if link.title.is_empty() {
                    "(untitled)"
                } else {
                    link.title.as_str()
                };
                report.unresolved.push(format!("{} — {}", link.app_name, title));
            }
        }
        report.unresolved.reverse();
        report
    }

    // --- CoreGraphics: ordering + pid discovery -----------------------------

    /// Apps with on-screen normal windows, in front-to-back order of first
    /// appearance: `(pid, owner_name)`.
    fn cg_apps_front_to_back(self_pid: i32) -> Vec<(i32, String)> {
        let mut ordered = Vec::new();
        let mut seen = HashSet::new();
        for_each_window(|pid, name| {
            if pid != self_pid && seen.insert(pid) {
                ordered.push((pid, name));
            }
        });
        ordered
    }

    /// Map of owner name -> distinct pids (an app can run multiple processes).
    fn cg_name_to_pids(self_pid: i32) -> HashMap<String, Vec<i32>> {
        let mut map: HashMap<String, Vec<i32>> = HashMap::new();
        for_each_window(|pid, name| {
            if pid != self_pid {
                let pids = map.entry(name).or_default();
                if !pids.contains(&pid) {
                    pids.push(pid);
                }
            }
        });
        map
    }

    /// Visit each on-screen normal (layer 0) window's `(owner_pid, owner_name)`
    /// in front-to-back order.
    fn for_each_window(mut f: impl FnMut(i32, String)) {
        unsafe {
            let Some(array) = copy_window_info(
                kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements,
                kCGNullWindowID,
            ) else {
                return;
            };
            let array_ref = array.as_concrete_TypeRef() as CFArrayRef;
            let count = CFArrayGetCount(array_ref);
            for i in 0..count {
                let dict = CFArrayGetValueAtIndex(array_ref, i) as CFDictionaryRef;
                if dict.is_null() {
                    continue;
                }
                if dict_i64(dict, kCGWindowLayer).unwrap_or(1) != 0 {
                    continue;
                }
                let Some(pid) = dict_i64(dict, kCGWindowOwnerPID) else {
                    continue;
                };
                let name = dict_string(dict, kCGWindowOwnerName).unwrap_or_default();
                f(pid as i32, name);
            }
        }
    }

    unsafe fn dict_i64(dict: CFDictionaryRef, key: CFStringRef) -> Option<i64> {
        let value = CFDictionaryGetValue(dict, key as *const c_void);
        if value.is_null() {
            return None;
        }
        CFNumber::wrap_under_get_rule(value as CFNumberRef).to_i64()
    }

    unsafe fn dict_string(dict: CFDictionaryRef, key: CFStringRef) -> Option<String> {
        let value = CFDictionaryGetValue(dict, key as *const c_void);
        if value.is_null() {
            return None;
        }
        Some(CFString::wrap_under_get_rule(value as CFStringRef).to_string())
    }

    // --- Accessibility: per-app windows + raising ---------------------------

    /// Titles + document paths of every AX window of `pid`, in AX order.
    unsafe fn app_window_infos(pid: i32) -> Vec<(String, Option<String>)> {
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            return Vec::new();
        }
        AXUIElementSetMessagingTimeout(app, AX_TIMEOUT_SECS);

        let mut out = Vec::new();
        if let Some(windows) = copy_attr(app, kAXWindowsAttribute) {
            let windows_ref = windows.as_concrete_TypeRef() as CFArrayRef;
            let count = CFArrayGetCount(windows_ref);
            for i in 0..count {
                let window = CFArrayGetValueAtIndex(windows_ref, i) as AXUIElementRef;
                if window.is_null() {
                    continue;
                }
                let title = copy_attr_string(window, kAXTitleAttribute).unwrap_or_default();
                let document = document_attr(window);
                out.push((title, document));
            }
        }
        CFRelease(app as CFTypeRef);
        out
    }

    /// Find the best-matching window of `pid` for `link` and raise it.
    unsafe fn raise_in_app(pid: i32, link: &LinkedWindow) -> bool {
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            return false;
        }
        AXUIElementSetMessagingTimeout(app, AX_TIMEOUT_SECS);

        let mut best: Option<AXUIElementRef> = None;
        let mut best_score = 0u32;
        if let Some(windows) = copy_attr(app, kAXWindowsAttribute) {
            let windows_ref = windows.as_concrete_TypeRef() as CFArrayRef;
            let count = CFArrayGetCount(windows_ref);
            for i in 0..count {
                let window = CFArrayGetValueAtIndex(windows_ref, i) as AXUIElementRef;
                if window.is_null() {
                    continue;
                }
                let title = copy_attr_string(window, kAXTitleAttribute).unwrap_or_default();
                let document = document_attr(window);
                let score = match_score(link, &title, &document);
                if score > best_score {
                    best_score = score;
                    best = Some(window);
                }
            }

            let raised = if let Some(window) = best {
                set_true(window, kAXMainAttribute);
                let action = CFString::new(kAXRaiseAction);
                AXUIElementPerformAction(window, action.as_concrete_TypeRef());
                true
            } else {
                false
            };

            if raised {
                set_true(app, kAXFrontmostAttribute);
            }
            CFRelease(app as CFTypeRef);
            return raised;
        }

        CFRelease(app as CFTypeRef);
        false
    }

    /// Score a candidate window against a link: 3 = document path match,
    /// 2 = exact title, 1 = title prefix, 0 = no match.
    fn match_score(link: &LinkedWindow, title: &str, document: &Option<String>) -> u32 {
        if let (Some(want), Some(have)) = (&link.document_path, document) {
            if want == have {
                return 3;
            }
        }
        if !link.title.is_empty() {
            if link.title == title {
                return 2;
            }
            if !title.is_empty()
                && (title.starts_with(&link.title) || link.title.starts_with(title))
            {
                return 1;
            }
        }
        0
    }

    unsafe fn set_true(element: AXUIElementRef, attribute: &str) {
        let attr = CFString::new(attribute);
        let value = CFBoolean::true_value();
        AXUIElementSetAttributeValue(element, attr.as_concrete_TypeRef(), value.as_CFTypeRef());
    }

    /// Copy an AX attribute as an owned CFType, or None if missing/error.
    unsafe fn copy_attr(element: AXUIElementRef, attribute: &str) -> Option<CFType> {
        let attr = CFString::new(attribute);
        let mut value: CFTypeRef = ptr::null();
        let err = AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value);
        if err != kAXErrorSuccess || value.is_null() {
            return None;
        }
        Some(CFType::wrap_under_create_rule(value))
    }

    unsafe fn copy_attr_string(element: AXUIElementRef, attribute: &str) -> Option<String> {
        copy_attr(element, attribute)?
            .downcast::<CFString>()
            .map(|s| s.to_string())
    }

    /// The window's open document, treating an empty/missing value as None so it
    /// isn't used as a (meaningless) match key.
    unsafe fn document_attr(window: AXUIElementRef) -> Option<String> {
        copy_attr_string(window, kAXDocumentAttribute).filter(|s| !s.is_empty())
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::{RaiseReport, WindowInfo};
    use crate::workspace::LinkedWindow;

    pub fn accessibility_trusted(_prompt: bool) -> bool {
        false
    }
    pub fn open_accessibility_settings() {}
    pub fn list_windows() -> Vec<WindowInfo> {
        Vec::new()
    }
    pub fn raise_windows(_links: &[LinkedWindow]) -> RaiseReport {
        RaiseReport::default()
    }
}
