#![cfg(fb_platform)]

//! The framebuffer platform: the whole screen is one window.
//!
//! Screen = `/dev/fb0` (opened once here and shared with the wgpu backend
//! through `AKUMA_FB_FD`, because the kernel allows a single open of the
//! device — a second open gets EBUSY). Input = the console tty on stdin,
//! decoded from the kernel console's escape sequences. There is no window
//! system: no resize, no move, no cursor, one monitor at scale 1.
//!
//! Modelled on the Redox `orbital` platform (same file layout and method
//! surface), with the socket layer replaced by fbdev + tty.

use std::fmt::{self, Display, Formatter};
use std::fs::File;
use std::io;
use std::os::unix::io::AsRawFd;
use std::sync::{Arc, Mutex, OnceLock};

use smol_str::SmolStr;

use crate::dpi::{PhysicalPosition, PhysicalSize};
use crate::keyboard::Key;

pub(crate) use self::event_loop::{
    ActiveEventLoop, EventLoop, EventLoopProxy, OwnedDisplayHandle,
};
mod event_loop;

pub use self::window::Window;
mod window;

/// The screen this process owns: the kept-open `/dev/fb0` fd and its
/// geometry. The fd must stay open for the process lifetime (closing it
/// hands the screen back to the console); the wgpu surface gets its own
/// copy through `AKUMA_FB_FD` (set at event-loop creation, before the
/// surface exists).
pub struct Screen {
    #[allow(dead_code)]
    dev: File,
    pub width: u32,
    pub height: u32,
}

static SCREEN: OnceLock<Arc<Screen>> = OnceLock::new();

// ---------------------------------------------------------------------------
// Hand-declared fbdev ABI: only what the screen query needs. Byte-for-byte
// <linux/fb.h>; see akuma-cli-wgpu src/fb.rs for the annotated version.
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Default)]
struct FbBitfield {
    offset: u32,
    length: u32,
    msb_right: u32,
}

#[repr(C)]
#[derive(Default)]
struct FbVarScreeninfo {
    xres: u32,
    yres: u32,
    xres_virtual: u32,
    yres_virtual: u32,
    xoffset: u32,
    yoffset: u32,
    bits_per_pixel: u32,
    grayscale: u32,
    red: FbBitfield,
    green: FbBitfield,
    blue: FbBitfield,
    transp: FbBitfield,
    nonstd: u32,
    activate: u32,
    height_mm: u32,
    width_mm: u32,
    accel_flags: u32,
    // timing (unused, but the struct size must match the ABI)
    pixclock: u32,
    left_margin: u32,
    right_margin: u32,
    upper_margin: u32,
    lower_margin: u32,
    hsync_len: u32,
    vsync_len: u32,
    sync: u32,
    vmode: u32,
    rotate: u32,
    colorspace: u32,
    reserved: [u32; 4],
}

const FBIOGET_VSCREENINFO: libc::c_ulong = 0x4600;

const _: () = {
    // the kernel copies exactly this many bytes; a size mismatch would
    // corrupt the ioctl both ways
    assert!(std::mem::size_of::<FbVarScreeninfo>() == 160);
};

impl Screen {
    /// Open the framebuffer (exactly once per process) and publish the fd
    /// for the wgpu surface. Fails if the device is missing or already
    /// taken (by us or anyone else).
    fn open() -> io::Result<Arc<Screen>> {
        let dev = File::options().read(true).write(true).open("/dev/fb0")?;
        let mut var = FbVarScreeninfo::default();
        // SAFETY: ioctl with a pointer to exactly the struct the fbdev ABI
        // expects; the kernel copies out `size_of` bytes.
        if unsafe { libc::ioctl(dev.as_raw_fd(), FBIOGET_VSCREENINFO as libc::c_int, &mut var) } == -1 {
            return Err(io::Error::last_os_error());
        }
        if var.bits_per_pixel != 32 {
            return Err(io::Error::other(format!(
                "/dev/fb0 is {} bpp; the fb platform needs 32",
                var.bits_per_pixel
            )));
        }
        // Publish the fd for the akuma wgpu surface before any thread that
        // could create one exists (EventLoop::new runs before sugarloaf).
        // SAFETY: formatting an fd number; the env var is read in the same
        // process by the wgpu backend's surface creation.
        unsafe {
            std::env::set_var("AKUMA_FB_FD", dev.as_raw_fd().to_string());
        }
        Ok(Arc::new(Screen {
            width: var.xres,
            height: var.yres,
            dev,
        }))
    }

    fn get() -> Option<&'static Arc<Screen>> {
        SCREEN.get()
    }
}

/// The screen geometry, or a sane fallback when queried before the event
/// loop exists (rioterm asks for monitor info very early).
pub fn screen_size() -> (u32, u32) {
    Screen::get().map(|s| (s.width, s.height)).unwrap_or((0, 0))
}

#[derive(Default, Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub(crate) struct PlatformSpecificEventLoopAttributes {}

static NEXT_WINDOW_ID: Mutex<u64> = Mutex::new(0);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WindowId(u64);

impl WindowId {
    fn next() -> Self {
        let mut id = NEXT_WINDOW_ID.lock().unwrap();
        let out = WindowId(*id);
        *id += 1;
        out
    }

    pub const fn dummy() -> Self {
        WindowId(u64::MAX)
    }
}

impl From<WindowId> for u64 {
    fn from(id: WindowId) -> Self {
        id.0
    }
}

impl From<u64> for WindowId {
    fn from(id: u64) -> Self {
        Self(id)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DeviceId;

impl DeviceId {
    pub const fn dummy() -> Self {
        DeviceId
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PlatformSpecificWindowAttributes;

#[derive(Clone, Debug)]
pub struct OsError(Arc<io::Error>);

impl OsError {
    fn new(error: io::Error) -> Self {
        Self(Arc::new(error))
    }
}

impl Display for OsError {
    fn fmt(&self, fmt: &mut Formatter<'_>) -> fmt::Result {
        self.0.fmt(fmt)
    }
}

pub(crate) use crate::cursor::{
    NoCustomCursor as PlatformCustomCursor, NoCustomCursor as PlatformCustomCursorSource,
};
pub(crate) use crate::icon::NoIcon as PlatformIcon;

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct MonitorHandle;

impl MonitorHandle {
    pub fn name(&self) -> Option<String> {
        Some("akuma-fb".to_owned())
    }

    pub fn size(&self) -> PhysicalSize<u32> {
        screen_size().into()
    }

    pub fn position(&self) -> PhysicalPosition<i32> {
        (0, 0).into()
    }

    pub fn scale_factor(&self) -> f64 {
        1.0
    }

    pub fn refresh_rate_millihertz(&self) -> Option<u32> {
        None
    }

    pub fn video_modes(&self) -> impl Iterator<Item = VideoModeHandle> {
        let size = self.size().into();
        std::iter::once(VideoModeHandle {
            size,
            bit_depth: 32,
            refresh_rate_millihertz: 60000,
            monitor: self.clone(),
        })
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct VideoModeHandle {
    size: (u32, u32),
    bit_depth: u16,
    refresh_rate_millihertz: u32,
    monitor: MonitorHandle,
}

impl VideoModeHandle {
    pub fn size(&self) -> PhysicalSize<u32> {
        self.size.into()
    }

    pub fn bit_depth(&self) -> u16 {
        self.bit_depth
    }

    pub fn refresh_rate_millihertz(&self) -> u32 {
        self.refresh_rate_millihertz
    }

    pub fn monitor(&self) -> MonitorHandle {
        self.monitor.clone()
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct KeyEventExtra {
    pub key_without_modifiers: Key,
    pub text_with_all_modifiers: Option<SmolStr>,
}
