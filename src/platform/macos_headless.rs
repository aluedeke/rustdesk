//! Headless display for macOS.
//!
//! A MacBook with its lid closed and no external monitor has no active display: macOS
//! keeps the built-in panel in the online list but renders nothing to it, so the
//! controlling side gets no frames. When that happens during a session, plug in a
//! virtual display with the private `CGVirtualDisplay` API (the one BetterDisplay and
//! DeskPad use) and remove it again once a real display appears or the last remote
//! session ends.
//!
//! The display lives as long as the `CGVirtualDisplay` object, so it is only created
//! by a process that is serving a remote session: usually `--server`, but the main
//! process hosts the server itself when the LaunchAgent is not running.

use hbb_common::{bail, log, ResultType};
use objc::{
    class, msg_send,
    runtime::{Object, BOOL, YES},
    sel, sel_impl,
};
use scrap::Display;
use std::{
    ffi::c_void,
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
    time::Duration,
};

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
static PLUG_OUT_GENERATION: AtomicU64 = AtomicU64::new(0);
const LINGER: Duration = Duration::from_secs(10 * 60);

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
    fn CGDisplayPixelsWide(display: u32) -> usize;
    fn CGBeginDisplayConfiguration(config: *mut *mut c_void) -> i32;
    fn CGConfigureDisplayOrigin(config: *mut c_void, display: u32, x: i32, y: i32) -> i32;
    fn CGCompleteDisplayConfiguration(config: *mut c_void, option: u32) -> i32;
    fn CGCancelDisplayConfiguration(config: *mut c_void) -> i32;
}

// Reverted by macOS when the configuring process exits.
const CG_CONFIGURE_FOR_APP_ONLY: u32 = 0;

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
    if usable == 0 && has_remote_session() && !is_plugged_in() {
        log::info!("no usable display (lid closed: {}), plug in headless display", lid_closed);
        if let Err(e) = plug_in() {
            log::error!("plug in headless display failed: {}", e);
        }
    } else if usable > 0 && is_plugged_in() {
        log::info!("a real display is available, plug out headless display");
        plug_out();
    }

    capturable_displays()
}

/// `Display::all()` without the built-in panel while the lid is closed: it stays "online"
/// but shows nothing, so it must never be picked when something else can be captured.
/// The video service indexes into this list, so it has to match `try_get_displays()`.
pub fn capturable_displays() -> ResultType<Vec<Display>> {
    let mut displays = Display::all()?;
    if displays.len() > 1 && is_lid_closed() {
        displays.retain(|d| {
            d.name()
                .parse::<u32>()
                .map_or(true, |id| unsafe { CGDisplayIsBuiltin(id) } == 0)
        });
    }
    Ok(displays)
}

/// Called when the last remote session ends. Keeps the headless display for a while:
/// the iOS client drops the connection whenever it goes to the background, and removing
/// the only display makes macOS lock the screen, so every app switch would end at the lock
/// screen. A real display showing up still removes it right away (`try_get_displays`).
pub fn plug_out_later() {
    let generation = PLUG_OUT_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    std::thread::spawn(move || {
        std::thread::sleep(LINGER);
        if PLUG_OUT_GENERATION.load(Ordering::SeqCst) == generation && !has_remote_session() {
            plug_out();
        }
    });
}

fn plug_out() {
    let Some(vd) = VIRTUAL_DISPLAY.lock().unwrap().take() else {
        return;
    };
    let object = vd.object as *mut Object;
    unsafe {
        let _: () = msg_send![object, release];
    }
    log::info!("headless display {} removed", vd.display_id);
}

fn has_remote_session() -> bool {
    use crate::server::{AuthConnType, AUTHED_CONNS};
    AUTHED_CONNS
        .lock()
        .unwrap()
        .iter()
        .any(|c| c.conn_type == AuthConnType::Remote)
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
    make_main_display(display_id);
    Ok(())
}

/// macOS draws the lock screen and login window controls (clock, password field) only on
/// the main display, which stays the sleeping built-in panel when the lid is closed. Put
/// the virtual display at (0, 0) so it becomes the main display.
fn make_main_display(display_id: u32) {
    for _ in 0..20 {
        if online_display_ids().contains(&display_id) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let others: Vec<u32> = online_display_ids()
        .into_iter()
        .filter(|&id| id != display_id)
        .collect();
    unsafe {
        let mut config = std::ptr::null_mut();
        if CGBeginDisplayConfiguration(&mut config) != 0 {
            log::error!("CGBeginDisplayConfiguration failed");
            return;
        }
        let mut err = CGConfigureDisplayOrigin(config, display_id, 0, 0);
        let mut x = WIDTH as i32;
        for id in others {
            if err != 0 {
                break;
            }
            err = CGConfigureDisplayOrigin(config, id, x, 0);
            x += CGDisplayPixelsWide(id) as i32;
        }
        if err != 0 {
            CGCancelDisplayConfiguration(config);
            log::error!("configure display origin failed: {}", err);
            return;
        }
        let err = CGCompleteDisplayConfiguration(config, CG_CONFIGURE_FOR_APP_ONLY);
        if err != 0 {
            log::error!("make headless display {} main failed: {}", display_id, err);
        } else {
            log::info!("headless display {} is now the main display", display_id);
        }
    }
}
