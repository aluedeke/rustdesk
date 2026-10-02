//! Headless display for macOS.
//!
//! A MacBook with its lid closed and no external monitor has no usable display: macOS
//! keeps the built-in panel in the online list but renders nothing to it, so the
//! controlling side gets no frames. When that happens during a session, plug in a
//! virtual display with the private `CGVirtualDisplay` API (the one BetterDisplay and
//! DeskPad use) and remove it again once a real display appears or the last remote
//! session ends.
//!
//! The display lives as long as the `CGVirtualDisplay` object, so it is only created
//! by a process that is serving a remote session: usually `--server`, but the main
//! process hosts the server itself when the LaunchAgent is not running.

use hbb_common::{bail, log, throttled_log, ResultType};
use objc::{
    msg_send,
    runtime::{Class, Object, BOOL, YES},
    sel, sel_impl,
};
use scrap::{quartz, Display};
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
const LINGER: Duration = Duration::from_secs(10 * 60);
const LOG_INTERVAL: Duration = Duration::from_secs(60);
// Reverted by macOS when the configuring process exits.
const CG_CONFIGURE_FOR_APP_ONLY: u32 = 0;

struct VirtualDisplay {
    object: usize,
    display_id: u32,
    is_main: bool,
}

static VIRTUAL_DISPLAY: Mutex<Option<VirtualDisplay>> = Mutex::new(None);
static PLUG_OUT_GENERATION: AtomicU64 = AtomicU64::new(0);

#[repr(C)]
#[derive(Clone, Copy)]
struct CGSize {
    width: f64,
    height: f64,
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGBeginDisplayConfiguration(config: *mut *mut c_void) -> i32;
    fn CGConfigureDisplayOrigin(config: *mut c_void, display: u32, x: i32, y: i32) -> i32;
    fn CGConfigureDisplayMirrorOfDisplay(config: *mut c_void, display: u32, master: u32) -> i32;
    fn CGCompleteDisplayConfiguration(config: *mut c_void, option: u32) -> i32;
    fn CGCancelDisplayConfiguration(config: *mut c_void) -> i32;
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
    update_headless_display(lid_closed);
    capturable_displays_(lid_closed)
}

/// The list the video service indexes into; it has to match `try_get_displays()`.
pub fn capturable_displays() -> ResultType<Vec<Display>> {
    capturable_displays_(is_lid_closed())
}

/// Called when the last remote session ends. Keeps the headless display for a while:
/// the iOS client drops the connection whenever it goes to the background, and removing
/// the only display makes macOS lock the screen, so every app switch would end at the lock
/// screen. A real display showing up still removes it right away.
pub fn plug_out_later() {
    if VIRTUAL_DISPLAY.lock().unwrap().is_none() {
        return;
    }
    let generation = PLUG_OUT_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    std::thread::spawn(move || {
        std::thread::sleep(LINGER);
        if PLUG_OUT_GENERATION.load(Ordering::SeqCst) == generation && !has_remote_session() {
            if let Some(vd) = VIRTUAL_DISPLAY.lock().unwrap().take() {
                release(vd);
            }
        }
    });
}

/// Plugs in, promotes or removes the headless display. The lock is held across the check
/// and the change so concurrent callers cannot create a second display.
fn update_headless_display(lid_closed: bool) {
    let mut guard = VIRTUAL_DISPLAY.lock().unwrap();
    let own = guard.as_ref().map(|vd| vd.display_id);
    let online = online_display_ids();
    let usable = online
        .iter()
        .filter(|&&id| Some(id) != own)
        .filter(|&&id| !(lid_closed && is_builtin(id)))
        .count();

    if usable > 0 {
        if let Some(vd) = guard.take() {
            log::info!("a real display is available, plug out headless display");
            release(vd);
        }
        return;
    }
    match guard.as_mut() {
        None => {
            if !has_remote_session() {
                return;
            }
            throttled_log!(
                LOG_INTERVAL,
                info,
                "no usable display (lid closed: {}), plug in headless display",
                lid_closed
            );
            match plug_in() {
                Ok(vd) => *guard = Some(vd),
                Err(e) => throttled_log!(LOG_INTERVAL, error, "plug in headless display failed: {}", e),
            }
        }
        // Promoted on a later poll, once macOS lists the new display as online.
        Some(vd) if !vd.is_main && online.contains(&vd.display_id) => {
            vd.is_main = true;
            make_main_display(vd.display_id, &online);
        }
        Some(_) => {}
    }
}

/// `Display::all()` without the built-in panel while the lid is closed: it stays "online"
/// but shows nothing, so it must never be picked when something else can be captured.
fn capturable_displays_(lid_closed: bool) -> ResultType<Vec<Display>> {
    let mut displays = Display::all()?;
    if lid_closed && displays.len() > 1 {
        displays.retain(|d| {
            d.name()
                .parse::<u32>()
                .map_or(true, |id| !is_builtin(id))
        });
    }
    Ok(displays)
}

fn has_remote_session() -> bool {
    use crate::server::{AuthConnType, AUTHED_CONNS};
    AUTHED_CONNS
        .lock()
        .unwrap()
        .iter()
        .any(|c| c.conn_type == AuthConnType::Remote)
}

fn is_builtin(id: u32) -> bool {
    unsafe { quartz::ffi::CGDisplayIsBuiltin(id) != 0 }
}

fn online_display_ids() -> Vec<u32> {
    quartz::Display::online()
        .map(|displays| displays.into_iter().map(|d| d.id()).collect())
        .unwrap_or_default()
}

fn is_lid_closed() -> bool {
    use core_foundation::{
        base::{CFType, TCFType},
        boolean::CFBoolean,
        string::CFString,
    };
    unsafe {
        let service =
            IOServiceGetMatchingService(0, IOServiceMatching(b"IOPMrootDomain\0".as_ptr() as _));
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

fn release(vd: VirtualDisplay) {
    let object = vd.object as *mut Object;
    unsafe {
        let _: () = msg_send![object, release];
    }
    log::info!("headless display {} removed", vd.display_id);
}

fn plug_in() -> ResultType<VirtualDisplay> {
    use cocoa::foundation::{NSArray, NSString};
    use objc::rc::autoreleasepool;

    // Private API: missing on older macOS releases.
    let (Some(descriptor_class), Some(display_class), Some(mode_class), Some(settings_class)) = (
        Class::get("CGVirtualDisplayDescriptor"),
        Class::get("CGVirtualDisplay"),
        Class::get("CGVirtualDisplayMode"),
        Class::get("CGVirtualDisplaySettings"),
    ) else {
        bail!("CGVirtualDisplay is not available on this macOS version");
    };

    let (object, display_id) = autoreleasepool(|| unsafe {
        let descriptor: *mut Object = msg_send![descriptor_class, alloc];
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

        let display: *mut Object = msg_send![display_class, alloc];
        let display: *mut Object = msg_send![display, initWithDescriptor: descriptor];
        let _: () = msg_send![descriptor, release];
        if display.is_null() {
            return (std::ptr::null_mut(), 0);
        }

        let mode: *mut Object = msg_send![mode_class, alloc];
        let mode: *mut Object =
            msg_send![mode, initWithWidth: WIDTH height: HEIGHT refreshRate: REFRESH_RATE];
        let settings: *mut Object = msg_send![settings_class, alloc];
        let settings: *mut Object = msg_send![settings, init];
        let _: () = msg_send![settings, setHiDPI: 0u32];
        let modes = NSArray::arrayWithObject(cocoa::base::nil, mode as _);
        let _: () = msg_send![settings, setModes: modes];
        let applied: BOOL = msg_send![display, applySettings: settings];
        let _: () = msg_send![settings, release];
        let _: () = msg_send![mode, release];
        let display_id: u32 = if applied == YES {
            msg_send![display, displayID]
        } else {
            0
        };
        if display_id == 0 {
            let _: () = msg_send![display, release];
            return (std::ptr::null_mut(), 0);
        }
        (display, display_id)
    });
    if object.is_null() {
        bail!("CGVirtualDisplay could not be created");
    }
    log::info!("headless display {} plugged in ({}x{})", display_id, WIDTH, HEIGHT);
    Ok(VirtualDisplay {
        object: object as usize,
        display_id,
        is_main: false,
    })
}

/// macOS draws the lock screen and login window controls (clock, password field) only on
/// the main display, which stays the sleeping built-in panel when the lid is closed. Put
/// the virtual display at (0, 0) so it becomes the main display, and mirror the remaining
/// (unusable) displays onto it so no window is left on a screen the peer cannot see.
fn make_main_display(display_id: u32, online: &[u32]) {
    unsafe {
        let mut config = std::ptr::null_mut();
        if CGBeginDisplayConfiguration(&mut config) != 0 {
            log::error!("CGBeginDisplayConfiguration failed");
            return;
        }
        let mut err = CGConfigureDisplayOrigin(config, display_id, 0, 0);
        for &id in online.iter().filter(|&&id| id != display_id) {
            if err != 0 {
                break;
            }
            err = CGConfigureDisplayMirrorOfDisplay(config, id, display_id);
        }
        if err != 0 {
            CGCancelDisplayConfiguration(config);
            log::error!("configure headless display {} failed: {}", display_id, err);
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
