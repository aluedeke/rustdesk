//! Headless display for macOS.
//!
//! A MacBook with its lid closed and no external monitor has no active display: macOS
//! keeps the built-in panel in the online list but renders nothing to it, so the
//! controlling side gets no frames. When that happens during a session, plug in a
//! virtual display with the private `CGVirtualDisplay` API (the one BetterDisplay and
//! DeskPad use) and remove it again once a real display appears or the last remote
//! session ends.
//!
//! The display lives as long as the `CGVirtualDisplay` object, so it is created and
//! owned by the `--server` process.

use hbb_common::{bail, log, ResultType};
use objc::{
    class, msg_send,
    runtime::{Object, BOOL, YES},
    sel, sel_impl,
};
use scrap::Display;
use std::{ffi::c_void, sync::Mutex};

const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;
const REFRESH_RATE: f64 = 60.0;
// "RD" / "HL", only used to recognize our own display.
const VENDOR_ID: u32 = 0x5244;
const PRODUCT_ID: u32 = 0x484c;
const SERIAL_NUM: u32 = 0x0001;

struct VirtualDisplay {
    object: usize,
    display_id: u32,
}

static VIRTUAL_DISPLAY: Mutex<Option<VirtualDisplay>> = Mutex::new(None);

#[repr(C)]
#[derive(Clone, Copy)]
struct CGSize {
    width: f64,
    height: f64,
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGGetOnlineDisplayList(max: u32, displays: *mut u32, count: *mut u32) -> i32;
    fn CGDisplayIsBuiltin(display: u32) -> i32;
    fn CGDisplayIsActive(display: u32) -> i32;
}

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOServiceMatching(name: *const std::os::raw::c_char) -> *mut c_void;
    fn IOServiceGetMatchingService(main_port: u32, matching: *mut c_void) -> u32;
    fn IORegistryEntryCreateCFProperty(
        entry: u32,
        key: *const c_void,
        allocator: *const c_void,
        options: u32,
    ) -> *const c_void;
    fn IOObjectRelease(object: u32) -> i32;
}

extern "C" {
    fn dispatch_get_global_queue(identifier: isize, flags: usize) -> *mut c_void;
}

/// `Display::all()` plus the headless handling described in the module docs.
pub fn try_get_displays() -> ResultType<Vec<Display>> {
    let lid_closed = is_lid_closed();
    let usable = usable_display_count(lid_closed);
    if usable == 0 && crate::is_server() && !is_plugged_in() {
        log::info!("no usable display (lid closed: {}), plug in headless display", lid_closed);
        if let Err(e) = plug_in() {
            log::error!("plug in headless display failed: {}", e);
        }
    } else if usable > 0 && is_plugged_in() {
        log::info!("a real display is available, plug out headless display");
        plug_out();
    }

    let mut displays = Display::all()?;
    // With the lid closed the built-in panel stays "online" but shows nothing. Leave it
    // out whenever there is something else to capture, so it is never picked first.
    if lid_closed && displays.len() > 1 {
        displays.retain(|d| {
            d.name()
                .parse::<u32>()
                .map_or(true, |id| unsafe { CGDisplayIsBuiltin(id) } == 0)
        });
    }
    Ok(displays)
}

/// Removes the headless display, if any. Called when the last remote session ends.
pub fn plug_out() {
    let Some(vd) = VIRTUAL_DISPLAY.lock().unwrap().take() else {
        return;
    };
    let object = vd.object as *mut Object;
    unsafe {
        let _: () = msg_send![object, release];
    }
    log::info!("headless display {} removed", vd.display_id);
}

fn is_plugged_in() -> bool {
    VIRTUAL_DISPLAY.lock().unwrap().is_some()
}

fn own_display_id() -> Option<u32> {
    VIRTUAL_DISPLAY.lock().unwrap().as_ref().map(|vd| vd.display_id)
}

fn online_display_ids() -> Vec<u32> {
    let mut ids = [0u32; 16];
    let mut count = 0u32;
    if unsafe { CGGetOnlineDisplayList(ids.len() as _, ids.as_mut_ptr(), &mut count) } != 0 {
        return Vec::new();
    }
    ids[..count as usize].to_vec()
}

fn usable_display_count(lid_closed: bool) -> usize {
    let own = own_display_id();
    online_display_ids()
        .into_iter()
        .filter(|&id| Some(id) != own)
        .filter(|&id| unsafe { CGDisplayIsActive(id) } != 0)
        .filter(|&id| !(lid_closed && unsafe { CGDisplayIsBuiltin(id) } != 0))
        .count()
}

fn is_lid_closed() -> bool {
    use core_foundation::{
        base::{CFType, TCFType},
        boolean::CFBoolean,
        string::CFString,
    };
    unsafe {
        let service = IOServiceGetMatchingService(0, IOServiceMatching(b"IOPMrootDomain\0".as_ptr() as _));
        if service == 0 {
            return false;
        }
        let key = CFString::from_static_string("AppleClamshellState");
        let value = IORegistryEntryCreateCFProperty(
            service,
            key.as_concrete_TypeRef() as _,
            std::ptr::null(),
            0,
        );
        IOObjectRelease(service);
        if value.is_null() {
            return false;
        }
        CFType::wrap_under_create_rule(value as _)
            .downcast::<CFBoolean>()
            .map_or(false, bool::from)
    }
}

fn plug_in() -> ResultType<()> {
    use cocoa::foundation::{NSArray, NSString};
    use objc::rc::autoreleasepool;

    let (object, display_id) = autoreleasepool(|| unsafe {
        let descriptor: *mut Object = msg_send![class!(CGVirtualDisplayDescriptor), alloc];
        let descriptor: *mut Object = msg_send![descriptor, init];
        if descriptor.is_null() {
            return (std::ptr::null_mut::<Object>(), 0u32);
        }
        let name = NSString::alloc(cocoa::base::nil).init_str("RustDesk Headless");
        let _: () = msg_send![descriptor, setName: name];
        let _: () = msg_send![name, release];
        let _: () = msg_send![descriptor, setQueue: dispatch_get_global_queue(0, 0)];
        let _: () = msg_send![descriptor, setMaxPixelsWide: WIDTH];
        let _: () = msg_send![descriptor, setMaxPixelsHigh: HEIGHT];
        // ~24" at 1920x1080, so macOS does not pick a HiDPI scale.
        let _: () = msg_send![descriptor, setSizeInMillimeters: CGSize { width: 530.0, height: 300.0 }];
        let _: () = msg_send![descriptor, setVendorID: VENDOR_ID];
        let _: () = msg_send![descriptor, setProductID: PRODUCT_ID];
        let _: () = msg_send![descriptor, setSerialNum: SERIAL_NUM];

        let display: *mut Object = msg_send![class!(CGVirtualDisplay), alloc];
        let display: *mut Object = msg_send![display, initWithDescriptor: descriptor];
        let _: () = msg_send![descriptor, release];
        if display.is_null() {
            return (std::ptr::null_mut(), 0);
        }

        let mode: *mut Object = msg_send![class!(CGVirtualDisplayMode), alloc];
        let mode: *mut Object =
            msg_send![mode, initWithWidth: WIDTH height: HEIGHT refreshRate: REFRESH_RATE];
        let settings: *mut Object = msg_send![class!(CGVirtualDisplaySettings), alloc];
        let settings: *mut Object = msg_send![settings, init];
        let _: () = msg_send![settings, setHiDPI: 0u32];
        let modes = NSArray::arrayWithObject(cocoa::base::nil, mode as _);
        let _: () = msg_send![settings, setModes: modes];
        let applied: BOOL = msg_send![display, applySettings: settings];
        let _: () = msg_send![settings, release];
        let _: () = msg_send![mode, release];
        if applied != YES {
            let _: () = msg_send![display, release];
            return (std::ptr::null_mut(), 0);
        }
        let display_id: u32 = msg_send![display, displayID];
        (display, display_id)
    });
    if object.is_null() || display_id == 0 {
        bail!("CGVirtualDisplay could not be created");
    }
    *VIRTUAL_DISPLAY.lock().unwrap() = Some(VirtualDisplay {
        object: object as usize,
        display_id,
    });
    log::info!("headless display {} plugged in ({}x{})", display_id, WIDTH, HEIGHT);
    Ok(())
}
