use crate::ns_string;
use anyhow::{Result, anyhow};
use block::ConcreteBlock;
use cocoa::{
    base::{YES, id, nil},
    foundation::NSArray,
};
use collections::HashMap;
use core_foundation::base::TCFType;
use core_graphics::display::{
    CGDirectDisplayID, CGDisplayCopyDisplayMode, CGDisplayModeGetPixelHeight,
    CGDisplayModeGetPixelWidth, CGDisplayModeRelease,
};
use ctor::ctor;
use futures::channel::oneshot;
use gpui::{
    DevicePixels, ForegroundExecutor, ScreenCaptureFrame, ScreenCaptureSource, ScreenCaptureStream,
    SharedString, SourceKind, SourceMetadata, size,
};
use media::core_media::{CMSampleBuffer, CMSampleBufferRef};
use metal::NSInteger;
use objc::{
    class,
    declare::ClassDecl,
    msg_send,
    runtime::{Class, Object, Sel},
    sel, sel_impl,
};
use std::{cell::RefCell, ffi::c_void, mem, ptr, rc::Rc};

use crate::NSStringExt;
mod screen_capture_window;

#[derive(Clone)]
enum MacCaptureTarget {
    Display {
        sc_display: id,
        meta: Option<ScreenMeta>,
    },
    Window {
        sc_window: id,
        window_id: u32,
        title: Option<SharedString>,
        app_name: Option<SharedString>,
        is_own_app: bool,
        resolution: gpui::Size<DevicePixels>,
    },
}

#[derive(Clone)]
pub struct MacScreenCaptureSource {
    target: MacCaptureTarget,
}

pub struct MacScreenCaptureStream {
    sc_stream: id,
    sc_stream_output: id,
    meta: SourceMetadata,
}

static mut DELEGATE_CLASS: *const Class = ptr::null();
static mut OUTPUT_CLASS: *const Class = ptr::null();
const FRAME_CALLBACK_IVAR: &str = "frame_callback";

#[allow(non_upper_case_globals)]
const SCStreamOutputTypeScreen: NSInteger = 0;

impl ScreenCaptureSource for MacScreenCaptureSource {
    fn metadata(&self) -> Result<SourceMetadata> {
        match &self.target {
            MacCaptureTarget::Display { sc_display, meta } => {
                let (display_id, resolution) = unsafe {
                    let display_id: CGDirectDisplayID = msg_send![*sc_display, displayID];
                    let display_mode_ref = CGDisplayCopyDisplayMode(display_id);
                    let width = CGDisplayModeGetPixelWidth(display_mode_ref);
                    let height = CGDisplayModeGetPixelHeight(display_mode_ref);
                    CGDisplayModeRelease(display_mode_ref);

                    (
                        display_id,
                        size(DevicePixels(width as i32), DevicePixels(height as i32)),
                    )
                };
                let (label, is_main) = meta.clone().map(|meta| (meta.label, meta.is_main)).unzip();

                Ok(SourceMetadata {
                    id: display_id as u64,
                    label,
                    is_main,
                    resolution,
                    kind: SourceKind::Display,
                })
            }
            MacCaptureTarget::Window {
                window_id,
                title,
                app_name,
                is_own_app,
                resolution,
                ..
            } => {
                let label = title
                    .clone()
                    .filter(|s| !s.is_empty())
                    .or_else(|| app_name.clone());
                Ok(SourceMetadata {
                    id: *window_id as u64,
                    label,
                    is_main: None,
                    resolution: *resolution,
                    kind: SourceKind::Window {
                        app_name: app_name.clone(),
                        is_own_app: *is_own_app,
                    },
                })
            }
        }
    }

    fn stream(
        &self,
        _foreground_executor: &ForegroundExecutor,
        frame_callback: Box<dyn Fn(ScreenCaptureFrame) + Send>,
    ) -> oneshot::Receiver<Result<Box<dyn ScreenCaptureStream>>> {
        unsafe {
            let stream: id = msg_send![class!(SCStream), alloc];
            let filter: id = msg_send![class!(SCContentFilter), alloc];
            let configuration: id = msg_send![class!(SCStreamConfiguration), alloc];
            let delegate: id = msg_send![DELEGATE_CLASS, alloc];
            let output: id = msg_send![OUTPUT_CLASS, alloc];

            let filter: id = match &self.target {
                MacCaptureTarget::Display { sc_display, .. } => {
                    let excluded_windows = NSArray::array(nil);
                    msg_send![filter, initWithDisplay:*sc_display excludingWindows:excluded_windows]
                }
                MacCaptureTarget::Window { sc_window, .. } => {
                    msg_send![filter, initWithDesktopIndependentWindow:*sc_window]
                }
            };
            let configuration: id = msg_send![configuration, init];
            let _: id = msg_send![configuration, setScalesToFit: true];
            let _: id = msg_send![configuration, setPixelFormat: 0x42475241];
            // let _: id = msg_send![configuration, setShowsCursor: false];
            // let _: id = msg_send![configuration, setCaptureResolution: 3];
            let delegate: id = msg_send![delegate, init];
            let output: id = msg_send![output, init];

            output.as_mut().unwrap().set_ivar(
                FRAME_CALLBACK_IVAR,
                Box::into_raw(Box::new(frame_callback)) as *mut c_void,
            );

            let meta = self.metadata().unwrap();
            let _: id = msg_send![configuration, setWidth: meta.resolution.width.0 as i64];
            let _: id = msg_send![configuration, setHeight: meta.resolution.height.0 as i64];
            let stream: id = msg_send![stream, initWithFilter:filter configuration:configuration delegate:delegate];

            // Stream contains filter, configuration, and delegate internally so we release them here
            // to prevent a memory leak when steam is dropped
            let _: () = msg_send![filter, release];
            let _: () = msg_send![configuration, release];
            let _: () = msg_send![delegate, release];

            let (tx, rx) = oneshot::channel();

            let mut error: id = nil;
            let _: () = msg_send![stream, addStreamOutput:output type:SCStreamOutputTypeScreen sampleHandlerQueue:0 error:&mut error as *mut id];
            if error != nil {
                let message: id = msg_send![error, localizedDescription];
                let _: () = msg_send![stream, release];
                let _: () = msg_send![output, release];
                tx.send(Err(anyhow!("failed to add stream output {message:?}")))
                    .ok();
                return rx;
            }

            let tx = Rc::new(RefCell::new(Some(tx)));
            let handler = ConcreteBlock::new({
                move |error: id| {
                    let result = if error == nil {
                        let stream = MacScreenCaptureStream {
                            meta: meta.clone(),
                            sc_stream: stream,
                            sc_stream_output: output,
                        };
                        Ok(Box::new(stream) as Box<dyn ScreenCaptureStream>)
                    } else {
                        let _: () = msg_send![stream, release];
                        let _: () = msg_send![output, release];
                        let message: id = msg_send![error, localizedDescription];
                        Err(anyhow!("failed to start screen capture stream {message:?}"))
                    };
                    if let Some(tx) = tx.borrow_mut().take() {
                        tx.send(result).ok();
                    }
                }
            });
            let handler = handler.copy();
            let _: () = msg_send![stream, startCaptureWithCompletionHandler:handler];
            rx
        }
    }
}

impl Drop for MacScreenCaptureSource {
    fn drop(&mut self) {
        unsafe {
            match &self.target {
                MacCaptureTarget::Display { sc_display, .. } => {
                    let _: () = msg_send![*sc_display, release];
                }
                MacCaptureTarget::Window { sc_window, .. } => {
                    let _: () = msg_send![*sc_window, release];
                }
            }
        }
    }
}

impl ScreenCaptureStream for MacScreenCaptureStream {
    fn metadata(&self) -> Result<SourceMetadata> {
        Ok(self.meta.clone())
    }
}

impl Drop for MacScreenCaptureStream {
    fn drop(&mut self) {
        unsafe {
            let mut error: id = nil;
            let _: () = msg_send![self.sc_stream, removeStreamOutput:self.sc_stream_output type:SCStreamOutputTypeScreen error:&mut error as *mut _];
            if error != nil {
                let message: id = msg_send![error, localizedDescription];
                log::error!("failed to add stream  output {message:?}");
            }

            let handler = ConcreteBlock::new(move |error: id| {
                if error != nil {
                    let message: id = msg_send![error, localizedDescription];
                    log::error!("failed to stop screen capture stream {message:?}");
                }
            });
            let block = handler.copy();
            let _: () = msg_send![self.sc_stream, stopCaptureWithCompletionHandler:block];
            let _: () = msg_send![self.sc_stream, release];
            let _: () = msg_send![self.sc_stream_output, release];
        }
    }
}

#[derive(Clone)]
struct ScreenMeta {
    label: SharedString,
    // Is this the screen with menu bar?
    is_main: bool,
}

unsafe fn screen_id_to_human_label() -> HashMap<CGDirectDisplayID, ScreenMeta> {
    let screens: id = msg_send![class!(NSScreen), screens];
    let count: usize = msg_send![screens, count];
    let mut map = HashMap::default();
    let screen_number_key = unsafe { ns_string("NSScreenNumber") };
    for i in 0..count {
        let screen: id = msg_send![screens, objectAtIndex: i];
        let device_desc: id = msg_send![screen, deviceDescription];
        if device_desc == nil {
            continue;
        }

        let nsnumber: id = msg_send![device_desc, objectForKey: screen_number_key];
        if nsnumber == nil {
            continue;
        }

        let screen_id: u32 = msg_send![nsnumber, unsignedIntValue];

        let name: id = msg_send![screen, localizedName];
        if name != nil {
            let cstr: *const std::os::raw::c_char = msg_send![name, UTF8String];
            let rust_str = unsafe {
                std::ffi::CStr::from_ptr(cstr)
                    .to_string_lossy()
                    .into_owned()
            };
            map.insert(
                screen_id,
                ScreenMeta {
                    label: rust_str.into(),
                    is_main: i == 0,
                },
            );
        }
    }
    map
}

pub(crate) fn get_sources() -> oneshot::Receiver<Result<Vec<Rc<dyn ScreenCaptureSource>>>> {
    unsafe {
        let (tx, rx) = oneshot::channel();
        let tx = Rc::new(RefCell::new(Some(tx)));
        let screen_id_to_label = screen_id_to_human_label();
        let own_bundle = screen_capture_window::own_bundle_identifier();
        let block = ConcreteBlock::new(move |shareable_content: id, error: id| {
            let Some(tx) = tx.borrow_mut().take() else {
                return;
            };

            let result = if error == nil {
                let mut result: Vec<Rc<dyn ScreenCaptureSource>> = Vec::new();

                let displays: id = msg_send![shareable_content, displays];
                for i in 0..displays.count() {
                    let display = displays.objectAtIndex(i);
                    let id: CGDirectDisplayID = msg_send![display, displayID];
                    let meta = screen_id_to_label.get(&id).cloned();
                    let source = MacScreenCaptureSource {
                        target: MacCaptureTarget::Display {
                            sc_display: msg_send![display, retain],
                            meta,
                        },
                    };
                    result.push(Rc::new(source) as Rc<dyn ScreenCaptureSource>);
                }

                let windows: id = msg_send![shareable_content, windows];
                for i in 0..windows.count() {
                    let window = windows.objectAtIndex(i);
                    let window_id: u32 = msg_send![window, windowID];
                    let title_ns: id = msg_send![window, title];
                    let title = screen_capture_window::ns_string_to_shared(title_ns);

                    let owning_app: id = msg_send![window, owningApplication];
                    let (app_name, is_own_app) = if owning_app == nil {
                        (None, false)
                    } else {
                        let app_name_ns: id = msg_send![owning_app, applicationName];
                        let app_name = screen_capture_window::ns_string_to_shared(app_name_ns);
                        let bundle_id_ns: id = msg_send![owning_app, bundleIdentifier];
                        let bundle_id = screen_capture_window::ns_string_to_shared(bundle_id_ns)
                            .map(|s| s.to_string());
                        let is_own = match (&own_bundle, &bundle_id) {
                            (Some(own), Some(other)) => own == other,
                            _ => false,
                        };
                        (app_name, is_own)
                    };

                    // Skip windows that are both untitled and from an unknown
                    // app — they're usually helper surfaces (popovers,
                    // tooltips) the user never wants to share.
                    if title.is_none() && app_name.is_none() {
                        continue;
                    }

                    let resolution = screen_capture_window::window_resolution(window);
                    let source = MacScreenCaptureSource {
                        target: MacCaptureTarget::Window {
                            sc_window: msg_send![window, retain],
                            window_id,
                            title,
                            app_name,
                            is_own_app,
                            resolution,
                        },
                    };
                    result.push(Rc::new(source) as Rc<dyn ScreenCaptureSource>);
                }

                Ok(result)
            } else {
                let msg: id = msg_send![error, localizedDescription];
                Err(anyhow!(
                    "Screen share failed: {:?}",
                    NSStringExt::to_str(&msg)
                ))
            };
            tx.send(result).ok();
        });
        let block = block.copy();

        let _: () = msg_send![
            class!(SCShareableContent),
            getShareableContentExcludingDesktopWindows:YES
                                   onScreenWindowsOnly:YES
                                     completionHandler:block];
        rx
    }
}

#[ctor(unsafe)]
unsafe fn build_classes() {
    let mut decl = ClassDecl::new("GPUIStreamDelegate", class!(NSObject)).unwrap();
    unsafe {
        decl.add_method(
            sel!(outputVideoEffectDidStartForStream:),
            output_video_effect_did_start_for_stream as extern "C" fn(&Object, Sel, id),
        );
        decl.add_method(
            sel!(outputVideoEffectDidStopForStream:),
            output_video_effect_did_stop_for_stream as extern "C" fn(&Object, Sel, id),
        );
        decl.add_method(
            sel!(stream:didStopWithError:),
            stream_did_stop_with_error as extern "C" fn(&Object, Sel, id, id),
        );
        DELEGATE_CLASS = decl.register();

        let mut decl = ClassDecl::new("GPUIStreamOutput", class!(NSObject)).unwrap();
        decl.add_method(
            sel!(stream:didOutputSampleBuffer:ofType:),
            stream_did_output_sample_buffer_of_type
                as extern "C" fn(&Object, Sel, id, id, NSInteger),
        );
        decl.add_ivar::<*mut c_void>(FRAME_CALLBACK_IVAR);

        OUTPUT_CLASS = decl.register();
    }
}

extern "C" fn output_video_effect_did_start_for_stream(_this: &Object, _: Sel, _stream: id) {}

extern "C" fn output_video_effect_did_stop_for_stream(_this: &Object, _: Sel, _stream: id) {}

extern "C" fn stream_did_stop_with_error(_this: &Object, _: Sel, _stream: id, _error: id) {}

extern "C" fn stream_did_output_sample_buffer_of_type(
    this: &Object,
    _: Sel,
    _stream: id,
    sample_buffer: id,
    buffer_type: NSInteger,
) {
    if buffer_type != SCStreamOutputTypeScreen {
        return;
    }

    unsafe {
        let sample_buffer = sample_buffer as CMSampleBufferRef;
        let sample_buffer = CMSampleBuffer::wrap_under_get_rule(sample_buffer);
        if let Some(buffer) = sample_buffer.image_buffer() {
            let callback: Box<Box<dyn Fn(ScreenCaptureFrame)>> =
                Box::from_raw(*this.get_ivar::<*mut c_void>(FRAME_CALLBACK_IVAR) as *mut _);
            callback(ScreenCaptureFrame(buffer));
            mem::forget(callback);
        }
    }
}
