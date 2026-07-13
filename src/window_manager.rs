//! Linking macOS windows to workspaces and raising them on demand.
//!
//! macOS has no public, serializable handle for "a specific window of another
//! app", so we identify windows by stable attributes — the owning application
//! name, the window title, and (for document apps like Xcode) the open
//! document path — and re-resolve them against the live windows whenever a
//! workspace is activated.
//!
//! Enumeration uses the **CoreGraphics window list** (owner PID + name; no
//! Screen Recording permission needed for those fields) combined with the
//! **Accessibility API** for titles, document paths, and the actual raise.
//! Raising a specific window is `AXRaise` + `kAXMain` on the window plus
//! `kAXFrontmost` on its application. All of this requires the app to be a
//! trusted Accessibility client.
//!
//! Windows on **other Spaces (Mission Control desktops)** are included: the
//! CoreGraphics list is queried across all Spaces (not just on-screen), and
//! when a raised window lives on another Space we switch to it first via the
//! private CGS/SkyLight Space APIs (`_AXUIElementGetWindow`,
//! `SLSCopySpacesForWindows`, `SLSManagedDisplaySetCurrentSpace`) — the same
//! undocumented symbols AltTab and yabai use. Those are macOS-only and the
//! SkyLight framework is linked via `build.rs`.

use crate::workspace::LinkedWindow;

/// A live, enumerable window belonging to some running application.
#[derive(Debug, Clone)]
pub struct WindowInfo {
    pub pid: i32,
    pub app_name: String,
    pub title: String,
    pub document_path: Option<String>,
    /// CoreGraphics window id, when resolvable (macOS only).
    pub window_id: Option<u32>,
}

impl WindowInfo {
    /// Whether this live window is the one a stored link describes. A matching
    /// window id is decisive (it survives title/tab changes); otherwise fall
    /// back to the app + title + document descriptor.
    pub fn matches(&self, link: &LinkedWindow) -> bool {
        if let (Some(a), Some(b)) = (self.window_id, link.window_id) {
            if a == b {
                return true;
            }
        }
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
            window_id: self.window_id,
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
    use core_foundation::array::CFArray;
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
        kCGWindowOwnerName, kCGWindowOwnerPID,
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
            for (title, document_path, window_id) in unsafe { app_window_infos(pid) } {
                // Skip chrome with neither a title nor a document (palettes, sheets).
                if title.trim().is_empty() && document_path.is_none() {
                    continue;
                }
                out.push(WindowInfo {
                    pid,
                    app_name: app_name.clone(),
                    title,
                    document_path,
                    window_id,
                });
            }
        }
        out
    }

    pub fn raise_windows(links: &[LinkedWindow]) -> RaiseReport {
        let self_pid = std::process::id() as i32;
        let name_to_pids = cg_name_to_pids(self_pid);
        let mut report = RaiseReport::default();

        // The Space we most recently switched to. A group of links on the same
        // Space then triggers only one switch, and — since we raise in reverse —
        // the first-listed link is processed last and decides the Space we end
        // on.
        let mut last_switched: Option<u64> = None;

        // Raise in reverse so the first-listed link ends up frontmost.
        for link in links.iter().rev() {
            let raised = name_to_pids
                .get(&link.app_name)
                .into_iter()
                .flatten()
                .any(|&pid| unsafe { raise_in_app(pid, link, &mut last_switched) });

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

    /// Visit each normal (layer 0) window's `(owner_pid, owner_name)`, across
    /// **all** Spaces (not just the current one), roughly front-to-back.
    fn for_each_window(mut f: impl FnMut(i32, String)) {
        unsafe {
            // `kCGWindowListOptionAll` is 0, so excluding desktop elements alone
            // lists every app window on every Space. (Adding `OnScreenOnly` would
            // restrict it to the current Space — the old, single-Space behavior.)
            let Some(array) =
                copy_window_info(kCGWindowListExcludeDesktopElements, kCGNullWindowID)
            else {
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

    /// Title, document path, and CoreGraphics window id of every AX window of
    /// `pid`, in AX order.
    unsafe fn app_window_infos(pid: i32) -> Vec<(String, Option<String>, Option<u32>)> {
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
                let wid = ax_window_id(window);
                out.push((title, document, wid));
            }
        }
        CFRelease(app as CFTypeRef);
        out
    }

    /// Find the best-matching window of `pid` for `link` and raise it, first
    /// switching to its Space if it lives on another one. `last_switched` tracks
    /// the Space id we've already switched to this pass, so a run of links on the
    /// same Space doesn't re-trigger the switch animation.
    unsafe fn raise_in_app(
        pid: i32,
        link: &LinkedWindow,
        last_switched: &mut Option<u64>,
    ) -> bool {
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
                let wid = ax_window_id(window);
                let score = match_score(link, &title, &document, wid);
                if score > best_score {
                    best_score = score;
                    best = Some(window);
                }
            }

            let raised = if let Some(window) = best {
                // Follow the window to its Space if it's on another one.
                if let Some(wid) = ax_window_id(window) {
                    switch_to_window_space(wid, last_switched);
                }
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

    /// Score a candidate window against a link: 4 = window id match,
    /// 3 = document path match, 2 = exact title, 1 = title prefix, 0 = no match.
    ///
    /// The id match is gated on the documents not actively disagreeing, so a
    /// stale id that was reassigned to a *different* document after an app
    /// restart doesn't win over a correct document-path match.
    fn match_score(
        link: &LinkedWindow,
        title: &str,
        document: &Option<String>,
        wid: Option<u32>,
    ) -> u32 {
        if let (Some(want), Some(have)) = (link.window_id, wid) {
            if want == have && !documents_conflict(&link.document_path, document) {
                return 4;
            }
        }
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

    /// Whether two document paths are both present and different.
    fn documents_conflict(a: &Option<String>, b: &Option<String>) -> bool {
        matches!((a, b), (Some(a), Some(b)) if a != b)
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

    // --- Private CGS/SkyLight: Spaces ---------------------------------------

    /// Undocumented CGS/SkyLight symbols (plus `_AXUIElementGetWindow` from
    /// HIServices) for reading a window's Space and switching the active Space.
    /// SkyLight is linked via `build.rs`; the AX symbol resolves through
    /// ApplicationServices, already linked by `accessibility-sys`.
    mod sls {
        use accessibility_sys::AXUIElementRef;
        use core_foundation_sys::array::CFArrayRef;
        use core_foundation_sys::string::CFStringRef;
        use std::os::raw::c_int;

        pub type CGSConnectionID = c_int;

        /// `SLSCopySpacesForWindows` selector meaning "all Spaces".
        pub const SPACE_SELECTOR_ALL: c_int = 0x7;

        extern "C" {
            pub fn SLSMainConnectionID() -> CGSConnectionID;
            pub fn SLSCopySpacesForWindows(
                cid: CGSConnectionID,
                selector: c_int,
                window_ids: CFArrayRef,
            ) -> CFArrayRef;
            /// Per-display Space layout: an array of dicts, each with a
            /// "Display Identifier" and a "Spaces" array of `{ id64, … }`.
            pub fn SLSCopyManagedDisplaySpaces(cid: CGSConnectionID) -> CFArrayRef;
            pub fn SLSManagedDisplayGetCurrentSpace(
                cid: CGSConnectionID,
                display: CFStringRef,
            ) -> u64;
            pub fn SLSManagedDisplaySetCurrentSpace(
                cid: CGSConnectionID,
                display: CFStringRef,
                sid: u64,
            );
            /// Bridge an AX window element to its CoreGraphics window id.
            pub fn _AXUIElementGetWindow(element: AXUIElementRef, out_wid: *mut u32) -> i32;
        }
    }

    /// The CoreGraphics window id backing an AX window element, if resolvable.
    unsafe fn ax_window_id(window: AXUIElementRef) -> Option<u32> {
        let mut wid: u32 = 0;
        if sls::_AXUIElementGetWindow(window, &mut wid) == kAXErrorSuccess && wid != 0 {
            Some(wid)
        } else {
            None
        }
    }

    /// The Space id the window `wid` currently lives on, if any.
    unsafe fn window_space(cid: sls::CGSConnectionID, wid: u32) -> Option<u64> {
        let input = CFArray::from_CFTypes(&[CFNumber::from(wid as i64)]);
        let spaces = sls::SLSCopySpacesForWindows(
            cid,
            sls::SPACE_SELECTOR_ALL,
            input.as_concrete_TypeRef(),
        );
        if spaces.is_null() {
            return None;
        }
        let result = if CFArrayGetCount(spaces) > 0 {
            let v = CFArrayGetValueAtIndex(spaces, 0) as CFNumberRef;
            CFNumber::wrap_under_get_rule(v).to_i64().map(|x| x as u64)
        } else {
            None
        };
        CFRelease(spaces as CFTypeRef);
        result
    }

    /// A dictionary value looked up by a string key (owned by the dict; not
    /// retained).
    unsafe fn dict_value(dict: CFDictionaryRef, key: &str) -> *const c_void {
        let key = CFString::new(key);
        CFDictionaryGetValue(dict, key.as_concrete_TypeRef() as *const c_void)
    }

    /// The identifier of the display that owns Space `sid`, found by walking the
    /// managed-display Space layout. Owned (safe to use after the layout array is
    /// released).
    unsafe fn display_for_space(cid: sls::CGSConnectionID, sid: u64) -> Option<CFString> {
        let displays = sls::SLSCopyManagedDisplaySpaces(cid);
        if displays.is_null() {
            return None;
        }
        let mut found: Option<CFString> = None;
        'outer: for i in 0..CFArrayGetCount(displays) {
            let ddict = CFArrayGetValueAtIndex(displays, i) as CFDictionaryRef;
            if ddict.is_null() {
                continue;
            }
            let spaces = dict_value(ddict, "Spaces") as CFArrayRef;
            if spaces.is_null() {
                continue;
            }
            for j in 0..CFArrayGetCount(spaces) {
                let sdict = CFArrayGetValueAtIndex(spaces, j) as CFDictionaryRef;
                if sdict.is_null() {
                    continue;
                }
                let id64 = dict_value(sdict, "id64");
                if id64.is_null() {
                    continue;
                }
                let this = CFNumber::wrap_under_get_rule(id64 as CFNumberRef)
                    .to_i64()
                    .map(|x| x as u64);
                if this == Some(sid) {
                    let ident = dict_value(ddict, "Display Identifier") as CFStringRef;
                    if !ident.is_null() {
                        found = Some(CFString::wrap_under_get_rule(ident));
                    }
                    break 'outer;
                }
            }
        }
        CFRelease(displays as CFTypeRef);
        found
    }

    /// If `wid` is on a Space other than the one currently shown on its display,
    /// switch that display to the window's Space. `last_switched` de-dups repeat
    /// switches to the same Space within one raise pass.
    unsafe fn switch_to_window_space(wid: u32, last_switched: &mut Option<u64>) {
        let cid = sls::SLSMainConnectionID();
        let Some(sid) = window_space(cid, wid) else {
            return;
        };
        if *last_switched == Some(sid) {
            return;
        }
        let Some(display) = display_for_space(cid, sid) else {
            return;
        };
        let display_ref = display.as_concrete_TypeRef();
        if sls::SLSManagedDisplayGetCurrentSpace(cid, display_ref) != sid {
            sls::SLSManagedDisplaySetCurrentSpace(cid, display_ref, sid);
        }
        *last_switched = Some(sid);
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
