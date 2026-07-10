use cocoa::{
    base::{id, nil},
    foundation::NSBundle,
};
use core_graphics::geometry::CGRect;
use gpui::{DevicePixels, SharedString, size};
use objc::{class, msg_send, sel, sel_impl};

pub(super) unsafe fn ns_string_to_shared(string: id) -> Option<SharedString> {
    if string == nil {
        return None;
    }
    let cstr: *const std::os::raw::c_char = unsafe { msg_send![string, UTF8String] };
    if cstr.is_null() {
        return None;
    }
    let rust_str = unsafe { std::ffi::CStr::from_ptr(cstr) }
        .to_string_lossy()
        .into_owned();
    if rust_str.is_empty() {
        None
    } else {
        Some(rust_str.into())
    }
}

pub(super) unsafe fn own_bundle_identifier() -> Option<String> {
    let bundle: id = unsafe { NSBundle::mainBundle() };
    if bundle == nil {
        return None;
    }
    let bundle_id: id = unsafe { msg_send![bundle, bundleIdentifier] };
    unsafe { ns_string_to_shared(bundle_id) }.map(|s| s.to_string())
}

pub(super) unsafe fn window_resolution(sc_window: id) -> gpui::Size<DevicePixels> {
    let frame: CGRect = msg_send![sc_window, frame];
    // SCShareableContent exposes frames in points. Multiply by the main
    // screen's backing scale factor to produce a reasonable pixel resolution
    // for the capture configuration. On non-retina displays this resolves
    // to the point dimensions; on retina it doubles them.
    let main_screen: id = msg_send![class!(NSScreen), mainScreen];
    let scale: f64 = if main_screen == nil {
        1.0
    } else {
        msg_send![main_screen, backingScaleFactor]
    };
    let width = (frame.size.width * scale).round().max(1.0) as i32;
    let height = (frame.size.height * scale).round().max(1.0) as i32;
    size(DevicePixels(width), DevicePixels(height))
}
