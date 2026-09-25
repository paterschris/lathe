use gpui::{DevicePixels, SharedString, size};
use objc2::rc::Retained;
use objc2_foundation::{NSBundle, NSString};
use objc2_screen_capture_kit::SCWindow;

use super::{MacCaptureTarget, MacScreenCaptureSource};

fn ns_string_to_shared(string: Option<Retained<NSString>>) -> Option<SharedString> {
    let string = string?.to_string();
    if string.is_empty() {
        None
    } else {
        Some(string.into())
    }
}

pub(super) fn own_bundle_identifier() -> Option<String> {
    NSBundle::mainBundle()
        .bundleIdentifier()
        .map(|bundle_id| bundle_id.to_string())
}

pub(super) unsafe fn window_source(
    sc_window: Retained<SCWindow>,
    own_bundle: Option<&str>,
    backing_scale: f64,
) -> Option<MacScreenCaptureSource> {
    let window_id = unsafe { sc_window.windowID() };
    let title = ns_string_to_shared(unsafe { sc_window.title() });
    let (app_name, is_own_app) = match unsafe { sc_window.owningApplication() } {
        Some(owning_app) => {
            let app_name = ns_string_to_shared(Some(unsafe { owning_app.applicationName() }));
            let bundle_id = unsafe { owning_app.bundleIdentifier() }.to_string();
            (app_name, own_bundle == Some(bundle_id.as_str()))
        }
        None => (None, false),
    };

    // Skip windows that are both untitled and from an unknown app. They're
    // usually helper surfaces (popovers, tooltips) the user never wants to share.
    if title.is_none() && app_name.is_none() {
        return None;
    }

    // SCShareableContent exposes frames in points. Multiply by the main
    // screen's backing scale factor to produce a reasonable pixel resolution
    // for the capture configuration.
    let frame = unsafe { sc_window.frame() };
    let width = (frame.size.width * backing_scale).round().max(1.0) as i32;
    let height = (frame.size.height * backing_scale).round().max(1.0) as i32;

    Some(MacScreenCaptureSource {
        target: MacCaptureTarget::Window {
            sc_window,
            window_id,
            title,
            app_name,
            is_own_app,
            resolution: size(DevicePixels(width), DevicePixels(height)),
        },
    })
}
