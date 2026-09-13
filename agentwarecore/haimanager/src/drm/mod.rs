//! Getting pixels onto a screen.
//!
//! Agentware renders in software into a "dumb" buffer: plain linear memory the
//! CPU maps and writes, which the display controller scans out directly. There
//! is no GPU acceleration and no 3D driver, which suits a renderer whose entire
//! job is filled rectangles and glyphs.
//!
//! The bring-up sequence is fixed and every step depends on the last:
//!
//!   1. Open the card and take DRM master, without which no mode can be set.
//!   2. Enumerate resources: framebuffers, CRTCs, connectors, encoders.
//!   3. Find a connector with modes on it. That is a physical output.
//!   4. Follow it to an encoder and from there to a CRTC, the scanout engine.
//!   5. Allocate a dumb buffer the size of the chosen mode and map it.
//!   6. Wrap it in a framebuffer object and point the CRTC at it.

pub mod uapi;

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};

use uapi::{CONNECTED, ModeInfo, zeroed};

const CARD: &str = "/dev/dri/card0";

/// A mapped scanout buffer.
pub struct Framebuffer {
    /// The mapping, as 32-bit pixels in `XRGB8888`: `0x00RRGGBB`.
    pixels: *mut u32,
    bytes: usize,
    /// Pixels per row, which is **not** always `width`. The driver may demand
    /// a wider stride for alignment, and treating pitch as width produces a
    /// picture that shears diagonally across the screen.
    stride: usize,
    width: usize,
    height: usize,
    handle: u32,
    fb_id: u32,
}

impl Framebuffer {
    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    /// The whole mapping as pixels, including any padding at the end of each
    /// row. Callers must index with [`Framebuffer::stride`], never `width`.
    pub fn pixels(&mut self) -> &mut [u32] {
        // SAFETY: the mapping is valid, writable and this large for as long as
        // `self` lives, and nothing else holds a reference to it.
        unsafe { std::slice::from_raw_parts_mut(self.pixels, self.bytes / 4) }
    }

}

/// A display, held open for as long as the haimanager runs.
pub struct Display {
    card: File,
    crtc_id: u32,
    connector_id: u32,
    mode: ModeInfo,
    front: Framebuffer,
    /// The CRTC configuration found at startup, restored on the way out so a
    /// clean exit hands the console back rather than leaving a dead screen.
    saved: uapi::Crtc,
    /// Whether this driver takes dirty calls at all. virtio-gpu needs them;
    /// amdgpu answers every one from userspace with `ENOSYS` (its
    /// `amdgpu_dirtyfb` serves only the kernel's own fbdev client), and a
    /// scanout engine reading the buffer the CPU wrote needs no telling.
    /// Learned from the first refusal, so a real card costs one log line
    /// rather than one per frame.
    dirty_supported: bool,
}

impl Display {
    /// Open the card, pick an output, and bring up a framebuffer on it.
    pub fn open() -> io::Result<Self> {
        let card = OpenOptions::new().read(true).write(true).open(CARD)?;
        let fd = card.as_fd();

        // The first process to open a card usually becomes master implicitly.
        // Asking explicitly is harmless when we already are, and the failure is
        // worth reporting rather than discovering later as a silent refusal to
        // change modes.
        if let Err(err) = uapi::set_master(fd) {
            return Err(io::Error::other(format!(
                "could not become DRM master on {CARD} ({err}); another process may own the display"
            )));
        }

        let (connector_id, mode) = pick_output(fd)?;
        let crtc_id = find_crtc(fd, connector_id)?;

        let mut saved = zeroed::<uapi::Crtc>();
        saved.crtc_id = crtc_id;
        uapi::get_crtc(fd, &mut saved)?;

        let front = allocate(fd, mode.hdisplay as u32, mode.vdisplay as u32)?;

        let mut display =
            Self { card, crtc_id, connector_id, mode, front, saved, dirty_supported: true };
        display.present()?;
        Ok(display)
    }

    /// Run one dirty call, and stop making them if the driver has none.
    ///
    /// `ENOSYS` is the kernel's answer when a framebuffer has no dirty
    /// handler for userspace, which on amdgpu is every framebuffer. That is
    /// not a failed present: the card scans out of the mapping directly, so
    /// the frame is already on screen. Any other error is reported as before.
    fn dirty(&mut self, call: impl FnOnce(BorrowedFd<'_>, u32) -> io::Result<()>) -> io::Result<()> {
        if !self.dirty_supported {
            return Ok(());
        }
        match call(self.card.as_fd(), self.front.fb_id) {
            Err(err) if err.raw_os_error() == Some(libc::ENOSYS) => {
                self.dirty_supported = false;
                crate::log("the driver takes no dirty calls; presenting by scanout alone");
                Ok(())
            }
            other => other,
        }
    }

    pub fn mode_name(&self) -> String {
        self.mode.name()
    }

    pub fn framebuffer(&mut self) -> &mut Framebuffer {
        &mut self.front
    }

    /// Copy a finished frame to the screen.
    ///
    /// One pass over the canvas, row by row, because the scanout buffer's rows
    /// are `stride` pixels apart and the canvas's are `width` apart. Then the
    /// framebuffer is marked dirty, without which virtual hardware never
    /// transfers it; see [`uapi::FbDirty`].
    pub fn present_canvas(&mut self, canvas: &crate::paint::Canvas) -> io::Result<()> {
        self.blit_full(canvas);
        self.flush()
    }

    /// Copy the whole canvas into the framebuffer without telling the host.
    /// The caller flushes once everything for the frame, cursors included, is
    /// in place; a flush in the middle is a frame the host may show.
    pub fn blit_full(&mut self, canvas: &crate::paint::Canvas) {
        let stride = self.front.stride;
        let width = self.front.width.min(canvas.width() as usize);
        let height = self.front.height.min(canvas.height() as usize);
        let pixels = self.front.pixels();

        for (y, row) in canvas.rows().take(height).enumerate() {
            let start = y * stride;
            pixels[start..start + width].copy_from_slice(&row[..width]);
        }
    }

    /// Copy one region of a full-screen canvas into the framebuffer, again
    /// without flushing: the partial-repaint path for window drags.
    pub fn blit_region(&mut self, canvas: &crate::paint::Canvas, rect: crate::paint::Rect) {
        let stride = self.front.stride;
        let fb_w = self.front.width as i32;
        let fb_h = self.front.height as i32;
        let x0 = rect.x.clamp(0, fb_w);
        let y0 = rect.y.clamp(0, fb_h);
        let x1 = (rect.x + rect.w).clamp(0, fb_w);
        let y1 = (rect.y + rect.h).clamp(0, fb_h);
        if x0 >= x1 || y0 >= y1 {
            return;
        }

        let pixels = self.front.pixels();
        for (y, row) in canvas.rows().enumerate().take(y1 as usize).skip(y0 as usize) {
            let start = y * stride + x0 as usize;
            pixels[start..start + (x1 - x0) as usize]
                .copy_from_slice(&row[x0 as usize..x1 as usize]);
        }
    }

    /// Push what has been drawn to the screen.
    ///
    /// Required on virtual hardware; refused by real hardware, which is
    /// already showing it. See [`uapi::FbDirty`] for why writing the mapping
    /// is not by itself enough on the former, and [`Display::dirty`] for the
    /// latter.
    pub fn flush(&mut self) -> io::Result<()> {
        self.dirty(uapi::dirty_fb)
    }

    /// Copy a small canvas into the framebuffer at a position, without marking
    /// anything dirty.
    ///
    /// The dirty call is separate on purpose. A frame's overlay may be several
    /// patches, and every one of them has to be in the framebuffer before the
    /// host is told anything changed, or the host can present the moment
    /// between an erase and a stamp: a cursor that exists on every frame we
    /// composed but flickers on screen anyway.
    pub fn blit_patch(&mut self, patch: &crate::paint::Canvas, dst_x: i32, dst_y: i32) {
        let stride = self.front.stride;
        let fb_w = self.front.width as i32;
        let fb_h = self.front.height as i32;

        let x0 = dst_x.max(0);
        let y0 = dst_y.max(0);
        let x1 = (dst_x + patch.width()).min(fb_w);
        let y1 = (dst_y + patch.height()).min(fb_h);
        if x0 >= x1 || y0 >= y1 {
            return;
        }

        let pixels = self.front.pixels();
        for (row_index, row) in patch.rows().enumerate() {
            let y = dst_y + row_index as i32;
            if y < y0 || y >= y1 {
                continue;
            }
            let src_from = (x0 - dst_x) as usize;
            let src_to = (x1 - dst_x) as usize;
            let start = y as usize * stride + x0 as usize;
            pixels[start..start + (src_to - src_from)].copy_from_slice(&row[src_from..src_to]);
        }
    }

    /// Tell the host which rectangles changed, once, after all of them have.
    pub fn flush_rects(&mut self, rects: &[crate::paint::Rect]) -> io::Result<()> {
        if rects.is_empty() {
            return Ok(());
        }
        let fb_w = self.front.width as i32;
        let fb_h = self.front.height as i32;
        let clips: Vec<uapi::ClipRect> = rects
            .iter()
            .filter_map(|rect| {
                let x0 = rect.x.clamp(0, fb_w);
                let y0 = rect.y.clamp(0, fb_h);
                let x1 = (rect.x + rect.w).clamp(0, fb_w);
                let y1 = (rect.y + rect.h).clamp(0, fb_h);
                (x0 < x1 && y0 < y1).then_some(uapi::ClipRect {
                    x1: x0 as u16,
                    y1: y0 as u16,
                    x2: x1 as u16,
                    y2: y1 as u16,
                })
            })
            .collect();
        if clips.is_empty() {
            return Ok(());
        }
        self.dirty(|fd, fb_id| uapi::dirty_fb_rects(fd, fb_id, &clips))
    }

    /// Point the CRTC at our framebuffer.
    fn present(&mut self) -> io::Result<()> {
        let mut connectors = [self.connector_id];

        let mut req = zeroed::<uapi::Crtc>();
        req.set_connectors_ptr = connectors.as_mut_ptr() as u64;
        req.count_connectors = 1;
        req.crtc_id = self.crtc_id;
        req.fb_id = self.front.fb_id;
        req.mode = self.mode;
        req.mode_valid = 1;

        uapi::set_crtc(self.card.as_fd(), &mut req)
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        let fd = self.card.as_fd();

        // Put back whatever was on screen before, so exiting does not leave a
        // black display with no way to find out why.
        let _ = uapi::set_crtc(fd, &mut self.saved);

        uapi::rm_fb(fd, self.front.fb_id);
        uapi::destroy_dumb(fd, self.front.handle);
        uapi::drop_master(fd);
    }
}

/// Find an output worth driving, and the mode to drive it at.
///
/// Connectors are asked twice: once with zero counts to learn how many modes
/// they have, and again with buffers big enough to receive them. That
/// two-pass shape is how most of the DRM enumeration ioctls work.
fn pick_output(fd: BorrowedFd<'_>) -> io::Result<(u32, ModeInfo)> {
    let mut res = zeroed::<uapi::CardRes>();
    uapi::get_resources(fd, &mut res)?;

    if res.count_connectors == 0 {
        return Err(io::Error::other("the card reports no connectors"));
    }

    let mut connector_ids = vec![0u32; res.count_connectors as usize];
    let mut crtc_ids = vec![0u32; res.count_crtcs as usize];
    let mut encoder_ids = vec![0u32; res.count_encoders as usize];

    res.connector_id_ptr = connector_ids.as_mut_ptr() as u64;
    res.crtc_id_ptr = crtc_ids.as_mut_ptr() as u64;
    res.encoder_id_ptr = encoder_ids.as_mut_ptr() as u64;
    res.fb_id_ptr = 0;
    res.count_fbs = 0;
    uapi::get_resources(fd, &mut res)?;

    // Prefer a connector the driver calls connected. Virtual hardware is not
    // always honest about that, so a connector with modes is accepted as a
    // fallback rather than failing with a blank screen.
    let mut fallback = None;

    for &id in &connector_ids {
        let mut conn = zeroed::<uapi::GetConnector>();
        conn.connector_id = id;
        if uapi::get_connector(fd, &mut conn).is_err() || conn.count_modes == 0 {
            continue;
        }

        let mut modes = vec![zeroed::<ModeInfo>(); conn.count_modes as usize];
        let count = conn.count_modes;

        let mut conn = zeroed::<uapi::GetConnector>();
        conn.connector_id = id;
        conn.count_modes = count;
        conn.modes_ptr = modes.as_mut_ptr() as u64;
        if uapi::get_connector(fd, &mut conn).is_err() || conn.count_modes == 0 {
            continue;
        }

        // Mode zero is the driver's preferred one.
        let mode = modes[0];
        if conn.connection == CONNECTED {
            return Ok((id, mode));
        }
        fallback.get_or_insert((id, mode));
    }

    fallback.ok_or_else(|| io::Error::other("no connector reported any usable mode"))
}

/// Follow a connector to the CRTC that can scan out to it.
fn find_crtc(fd: BorrowedFd<'_>, connector_id: u32) -> io::Result<u32> {
    let mut conn = zeroed::<uapi::GetConnector>();
    conn.connector_id = connector_id;
    uapi::get_connector(fd, &mut conn)?;

    // The easy case: the connector already has an encoder attached, and that
    // encoder already has a CRTC.
    if conn.encoder_id != 0 {
        let mut enc = zeroed::<uapi::GetEncoder>();
        enc.encoder_id = conn.encoder_id;
        if uapi::get_encoder(fd, &mut enc).is_ok() && enc.crtc_id != 0 {
            return Ok(enc.crtc_id);
        }
    }

    // Otherwise ask which encoders can serve this connector, and which CRTCs
    // can serve those encoders. `possible_crtcs` is a bitmask of indices into
    // the resource list, not a list of ids.
    let mut res = zeroed::<uapi::CardRes>();
    uapi::get_resources(fd, &mut res)?;
    let mut crtc_ids = vec![0u32; res.count_crtcs as usize];
    res.crtc_id_ptr = crtc_ids.as_mut_ptr() as u64;
    res.count_connectors = 0;
    res.count_encoders = 0;
    res.count_fbs = 0;
    uapi::get_resources(fd, &mut res)?;

    let mut encoder_ids = vec![0u32; conn.count_encoders as usize];
    let mut conn = zeroed::<uapi::GetConnector>();
    conn.connector_id = connector_id;
    conn.count_encoders = encoder_ids.len() as u32;
    conn.encoders_ptr = encoder_ids.as_mut_ptr() as u64;
    uapi::get_connector(fd, &mut conn)?;

    for &encoder_id in &encoder_ids {
        let mut enc = zeroed::<uapi::GetEncoder>();
        enc.encoder_id = encoder_id;
        if uapi::get_encoder(fd, &mut enc).is_err() {
            continue;
        }

        for (index, &crtc_id) in crtc_ids.iter().enumerate() {
            if enc.possible_crtcs & (1 << index) != 0 {
                return Ok(crtc_id);
            }
        }
    }

    Err(io::Error::other("no CRTC can drive the chosen connector"))
}

/// Allocate a dumb buffer, map it, and wrap it in a framebuffer object.
fn allocate(fd: BorrowedFd<'_>, width: u32, height: u32) -> io::Result<Framebuffer> {
    let mut create = zeroed::<uapi::CreateDumb>();
    create.width = width;
    create.height = height;
    create.bpp = 32;
    uapi::create_dumb(fd, &mut create)?;

    let mut fb = uapi::FbCmd {
        fb_id: 0,
        width,
        height,
        pitch: create.pitch,
        bpp: 32,
        // 24 bits of colour in a 32-bit pixel: the top byte is ignored. This is
        // XRGB8888, the one format every dumb-buffer driver supports.
        depth: 24,
        handle: create.handle,
    };
    uapi::add_fb(fd, &mut fb)?;

    let mut map = zeroed::<uapi::MapDumb>();
    map.handle = create.handle;
    uapi::map_dumb(fd, &mut map)?;

    // SAFETY: mapping a driver-provided offset of the size the driver reported,
    // on a descriptor we own.
    let addr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            create.size as usize,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd.as_raw_fd(),
            map.offset as i64,
        )
    };

    if addr == libc::MAP_FAILED {
        let err = io::Error::last_os_error();
        uapi::rm_fb(fd, fb.fb_id);
        uapi::destroy_dumb(fd, create.handle);
        return Err(err);
    }

    Ok(Framebuffer {
        pixels: addr.cast(),
        bytes: create.size as usize,
        stride: create.pitch as usize / 4,
        width: width as usize,
        height: height as usize,
        handle: create.handle,
        fb_id: fb.fb_id,
    })
}
