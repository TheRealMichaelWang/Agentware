//! Kernel `uapi` definitions for the DRM ioctls the haimanager uses.
//!
//! Transcribed from `include/uapi/drm/drm.h` and `drm_mode.h`. This is stable
//! ABI: the layouts below cannot change without breaking every DRM program ever
//! compiled, so hand-declaring them carries no maintenance risk and keeps the
//! process that owns the screen free of a dependency whose failure mode is a
//! black display and no message.
//!
//! Every struct here is plain data with no padding surprises, which is why
//! [`zeroed`] is a safe way to make one.

#![allow(dead_code)]

use std::os::fd::{AsRawFd, BorrowedFd};

/// `'d'`, the DRM ioctl type.
const DRM_IOCTL_BASE: u32 = 0x64;

/// `_IOWR(type, nr, size)` as the kernel encodes it: direction in the top two
/// bits, then the payload size, the type, and the command number.
const fn iowr(nr: u32, size: usize) -> u32 {
    const READ_WRITE: u32 = 3;
    (READ_WRITE << 30) | ((size as u32) << 16) | (DRM_IOCTL_BASE << 8) | nr
}

/// `_IO(type, nr)`, for commands that pass no payload.
const fn io(nr: u32) -> u32 {
    (DRM_IOCTL_BASE << 8) | nr
}

pub fn set_master(fd: BorrowedFd<'_>) -> std::io::Result<()> {
    // SAFETY: an argument-less ioctl on a descriptor we own.
    let result = unsafe { libc::ioctl(fd.as_raw_fd(), io(0x1e) as _, 0) };
    if result < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

pub fn drop_master(fd: BorrowedFd<'_>) {
    // SAFETY: as above. Failure is not actionable during teardown.
    unsafe { libc::ioctl(fd.as_raw_fd(), io(0x1f) as _, 0) };
}

/// Issue an ioctl whose payload is a single struct, updated in place.
///
/// # Safety
/// `nr` must be the command matching `T`, or the kernel will read or write the
/// wrong number of bytes.
unsafe fn call<T>(fd: BorrowedFd<'_>, nr: u32, arg: &mut T) -> std::io::Result<()> {
    let request = iowr(nr, size_of::<T>());
    // SAFETY: caller guarantees nr matches T, and `arg` is a valid, sized,
    // writable pointer for the duration of the call.
    let result = unsafe { libc::ioctl(fd.as_raw_fd(), request as _, arg as *mut T) };
    if result < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Make a zeroed struct.
///
/// Sound for every type in this module: all of them are `repr(C)` aggregates of
/// integers and byte arrays, for which all-zero is a valid value. The kernel
/// also requires unused fields to be zero, so this is the correct initial state
/// rather than merely a convenient one.
pub fn zeroed<T>() -> T {
    // SAFETY: see above.
    unsafe { std::mem::zeroed() }
}

#[repr(C)]
pub struct CardRes {
    pub fb_id_ptr: u64,
    pub crtc_id_ptr: u64,
    pub connector_id_ptr: u64,
    pub encoder_id_ptr: u64,
    pub count_fbs: u32,
    pub count_crtcs: u32,
    pub count_connectors: u32,
    pub count_encoders: u32,
    pub min_width: u32,
    pub max_width: u32,
    pub min_height: u32,
    pub max_height: u32,
}

pub fn get_resources(fd: BorrowedFd<'_>, res: &mut CardRes) -> std::io::Result<()> {
    // SAFETY: 0xA0 is MODE_GETRESOURCES, whose payload is drm_mode_card_res.
    unsafe { call(fd, 0xA0, res) }
}

/// One display mode. `hdisplay` and `vdisplay` are the visible resolution; the
/// rest describe blanking intervals the hardware needs and we never touch.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ModeInfo {
    pub clock: u32,
    pub hdisplay: u16,
    pub hsync_start: u16,
    pub hsync_end: u16,
    pub htotal: u16,
    pub hskew: u16,
    pub vdisplay: u16,
    pub vsync_start: u16,
    pub vsync_end: u16,
    pub vtotal: u16,
    pub vscan: u16,
    pub vrefresh: u32,
    pub flags: u32,
    pub kind: u32,
    pub name: [u8; 32],
}

impl ModeInfo {
    pub fn name(&self) -> String {
        let end = self.name.iter().position(|&b| b == 0).unwrap_or(self.name.len());
        String::from_utf8_lossy(&self.name[..end]).into_owned()
    }
}

#[repr(C)]
pub struct GetConnector {
    pub encoders_ptr: u64,
    pub modes_ptr: u64,
    pub props_ptr: u64,
    pub prop_values_ptr: u64,
    pub count_modes: u32,
    pub count_props: u32,
    pub count_encoders: u32,
    pub encoder_id: u32,
    pub connector_id: u32,
    pub connector_type: u32,
    pub connector_type_id: u32,
    pub connection: u32,
    pub mm_width: u32,
    pub mm_height: u32,
    pub subpixel: u32,
    pub pad: u32,
}

/// `connection` values. Anything that is not `CONNECTED` may still be worth
/// trying: virtual hardware is inconsistent about reporting status.
pub const CONNECTED: u32 = 1;

pub fn get_connector(fd: BorrowedFd<'_>, conn: &mut GetConnector) -> std::io::Result<()> {
    // SAFETY: 0xA7 is MODE_GETCONNECTOR, payload drm_mode_get_connector.
    unsafe { call(fd, 0xA7, conn) }
}

#[repr(C)]
pub struct GetEncoder {
    pub encoder_id: u32,
    pub encoder_type: u32,
    pub crtc_id: u32,
    pub possible_crtcs: u32,
    pub possible_clones: u32,
}

pub fn get_encoder(fd: BorrowedFd<'_>, enc: &mut GetEncoder) -> std::io::Result<()> {
    // SAFETY: 0xA6 is MODE_GETENCODER, payload drm_mode_get_encoder.
    unsafe { call(fd, 0xA6, enc) }
}

/// A "dumb" buffer: plain linear memory the CPU can map and write. No GPU
/// acceleration, which is exactly what a software renderer wants.
#[repr(C)]
pub struct CreateDumb {
    pub height: u32,
    pub width: u32,
    pub bpp: u32,
    pub flags: u32,
    pub handle: u32,
    pub pitch: u32,
    pub size: u64,
}

pub fn create_dumb(fd: BorrowedFd<'_>, req: &mut CreateDumb) -> std::io::Result<()> {
    // SAFETY: 0xB2 is MODE_CREATE_DUMB, payload drm_mode_create_dumb.
    unsafe { call(fd, 0xB2, req) }
}

#[repr(C)]
pub struct MapDumb {
    pub handle: u32,
    pub pad: u32,
    pub offset: u64,
}

pub fn map_dumb(fd: BorrowedFd<'_>, req: &mut MapDumb) -> std::io::Result<()> {
    // SAFETY: 0xB3 is MODE_MAP_DUMB, payload drm_mode_map_dumb.
    unsafe { call(fd, 0xB3, req) }
}

#[repr(C)]
pub struct DestroyDumb {
    pub handle: u32,
}

pub fn destroy_dumb(fd: BorrowedFd<'_>, handle: u32) {
    let mut req = DestroyDumb { handle };
    // SAFETY: 0xB4 is MODE_DESTROY_DUMB, payload drm_mode_destroy_dumb.
    let _ = unsafe { call(fd, 0xB4, &mut req) };
}

/// Binds a buffer to a scanout-capable framebuffer object.
#[repr(C)]
pub struct FbCmd {
    pub fb_id: u32,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub bpp: u32,
    pub depth: u32,
    pub handle: u32,
}

pub fn add_fb(fd: BorrowedFd<'_>, req: &mut FbCmd) -> std::io::Result<()> {
    // SAFETY: 0xAE is MODE_ADDFB, payload drm_mode_fb_cmd.
    unsafe { call(fd, 0xAE, req) }
}

pub fn rm_fb(fd: BorrowedFd<'_>, mut fb_id: u32) {
    // SAFETY: 0xAF is MODE_RMFB, whose payload is a bare unsigned int.
    let _ = unsafe { call(fd, 0xAF, &mut fb_id) };
}

/// The scanout configuration: which framebuffer a CRTC shows, on which
/// connectors, at which mode.
#[repr(C)]
pub struct Crtc {
    pub set_connectors_ptr: u64,
    pub count_connectors: u32,
    pub crtc_id: u32,
    pub fb_id: u32,
    pub x: u32,
    pub y: u32,
    pub gamma_size: u32,
    pub mode_valid: u32,
    pub mode: ModeInfo,
}

pub fn set_crtc(fd: BorrowedFd<'_>, req: &mut Crtc) -> std::io::Result<()> {
    // SAFETY: 0xA2 is MODE_SETCRTC, payload drm_mode_crtc.
    unsafe { call(fd, 0xA2, req) }
}

pub fn get_crtc(fd: BorrowedFd<'_>, req: &mut Crtc) -> std::io::Result<()> {
    // SAFETY: 0xA1 is MODE_GETCRTC, payload drm_mode_crtc.
    unsafe { call(fd, 0xA1, req) }
}

/// Tells the driver a framebuffer's contents changed.
///
/// Not optional on virtual hardware. A physical card scans out of memory the
/// CPU wrote directly, so writing the mapping is enough. virtio-gpu instead
/// holds its own copy on the host side and only transfers when asked, so
/// without this the picture never leaves the guest and the screen stays exactly
/// as it was when the mode was set.
#[repr(C)]
pub struct FbDirty {
    pub fb_id: u32,
    pub flags: u32,
    pub color: u32,
    pub num_clips: u32,
    pub clips_ptr: u64,
}

/// Mark an entire framebuffer as changed. Zero clips at a null pointer is the
/// kernel's idiom for "all of it".
pub fn dirty_fb(fd: BorrowedFd<'_>, fb_id: u32) -> std::io::Result<()> {
    let mut req = FbDirty { fb_id, flags: 0, color: 0, num_clips: 0, clips_ptr: 0 };
    // SAFETY: 0xB1 is MODE_DIRTYFB, payload drm_mode_fb_dirty_cmd.
    unsafe { call(fd, 0xB1, &mut req) }
}

/// Swap the displayed framebuffer at the next vertical blank.
#[repr(C)]
pub struct PageFlip {
    pub crtc_id: u32,
    pub fb_id: u32,
    pub flags: u32,
    pub reserved: u32,
    pub user_data: u64,
}

/// Ask for a completion event on the DRM fd when the flip lands.
pub const PAGE_FLIP_EVENT: u32 = 0x01;

pub fn page_flip(fd: BorrowedFd<'_>, req: &mut PageFlip) -> std::io::Result<()> {
    // SAFETY: 0xB0 is MODE_PAGE_FLIP, payload drm_mode_crtc_page_flip.
    unsafe { call(fd, 0xB0, req) }
}
