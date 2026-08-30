//! Handling of the `xdg-toplevel-icon-v1` protocol.
//!
//! Unlike a launcher icon, a toplevel icon represents a single window and can be
//! changed at runtime. It is shown in taskbars, window switchers and overviews.

use sctk::reexports::client::globals::{BindError, GlobalList};
use sctk::reexports::client::protocol::wl_shm::Format;
use sctk::reexports::client::{delegate_dispatch, Connection, Dispatch, Proxy, QueueHandle};
use sctk::globals::GlobalData;
use sctk::shm::slot::{Buffer, SlotPool};

use wayland_protocols::xdg::shell::client::xdg_toplevel::XdgToplevel;
use wayland_protocols::xdg::toplevel_icon::v1::client::xdg_toplevel_icon_manager_v1::Event as IconManagerEvent;
use wayland_protocols::xdg::toplevel_icon::v1::client::xdg_toplevel_icon_manager_v1::XdgToplevelIconManagerV1;
use wayland_protocols::xdg::toplevel_icon::v1::client::xdg_toplevel_icon_v1::XdgToplevelIconV1;

use crate::platform_impl::wayland::state::WinitState;
use crate::platform_impl::PlatformIcon;

/// A toplevel icon.
///
/// Holds the pixel buffer alive until it is replaced; otherwise the shared
/// memory slot could be reused, corrupting the icon still shown by the
/// compositor.
#[derive(Debug)]
pub struct ToplevelIcon {
    _icon: XdgToplevelIconV1,
    _buffer: Buffer,
}

/// The `xdg_toplevel_icon_manager_v1` global.
#[derive(Debug, Clone)]
pub struct XdgToplevelIconManager {
    manager: XdgToplevelIconManagerV1,
}

impl XdgToplevelIconManager {
    pub fn new(
        globals: &GlobalList,
        queue_handle: &QueueHandle<WinitState>,
    ) -> Result<Self, BindError> {
        let manager = globals.bind(queue_handle, 1..=1, GlobalData)?;
        Ok(Self { manager })
    }

    /// Assign the given RGBA image as the icon of `toplevel`.
    ///
    /// Icons are immutable once assigned, so each call creates a fresh icon and
    /// returns it. The caller must keep the returned icon alive to avoid
    /// reusing its shared-memory buffer. Returns `None` if no slot is available
    /// to hold the pixel data.
    pub(crate) fn set_icon(
        &self,
        toplevel: &XdgToplevel,
        pool: &mut SlotPool,
        queue_handle: &QueueHandle<WinitState>,
        image: &PlatformIcon,
        scale: i32,
    ) -> Option<ToplevelIcon> {
        let icon = self.manager.create_icon(queue_handle, ());
        let buffer = set_image(&icon, pool, &image.rgba, image.width, image.height, scale)?;
        self.manager.set_icon(toplevel, Some(&icon));
        Some(ToplevelIcon { _icon: icon, _buffer: buffer })
    }

    /// Remove any previously set toplevel icon.
    pub fn clear_icon(&self, toplevel: &XdgToplevel) {
        self.manager.set_icon(toplevel, None);
    }
}

/// Write `rgba` pixel data into a shared-memory buffer and attach it to `icon`.
///
/// Named `set_image` (rather than the protocol's "add_buffer") because the icon
/// carries exactly one image; the name describes the payload, not the
/// transport.
fn set_image(
    icon: &XdgToplevelIconV1,
    pool: &mut SlotPool,
    rgba: &[u8],
    width: u32,
    height: u32,
    scale: i32,
) -> Option<Buffer> {
    let (buffer, canvas) = pool
        .create_buffer(
            width as i32,
            height as i32,
            4 * width as i32,
            Format::Argb8888,
        )
        .ok()?;

    // Alpha is premultiplied in the buffer (same layout as cursor images).
    for (canvas_chunk, rgba) in canvas.chunks_exact_mut(4).zip(rgba.chunks_exact(4)) {
        let alpha = rgba[3] as f32 / 255.;
        let r = (rgba[0] as f32 * alpha) as u32;
        let g = (rgba[1] as f32 * alpha) as u32;
        let b = (rgba[2] as f32 * alpha) as u32;
        let color = ((rgba[3] as u32) << 24) + (r << 16) + (g << 8) + b;
        let array: &mut [u8; 4] = canvas_chunk.try_into().unwrap();
        *array = color.to_le_bytes();
    }

    icon.add_buffer(buffer.wl_buffer(), scale);
    Some(buffer)
}

impl Dispatch<XdgToplevelIconManagerV1, GlobalData, WinitState> for XdgToplevelIconManager {
    fn event(
        _: &mut WinitState,
        _: &XdgToplevelIconManagerV1,
        event: <XdgToplevelIconManagerV1 as Proxy>::Event,
        _: &GlobalData,
        _: &Connection,
        _: &QueueHandle<WinitState>,
    ) {
        // `icon_size` hints the preferred icon size and `done` is a
        // synchronization point; neither requires any action here.
        match event {
            IconManagerEvent::IconSize { .. } => {}
            IconManagerEvent::Done => {}
            _ => {}
        }
    }
}

impl Dispatch<XdgToplevelIconV1, (), WinitState> for XdgToplevelIconManager {
    fn event(
        _: &mut WinitState,
        _: &XdgToplevelIconV1,
        _: <XdgToplevelIconV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<WinitState>,
    ) {
        unreachable!("no events defined for xdg_toplevel_icon_v1");
    }
}

delegate_dispatch!(WinitState: [XdgToplevelIconManagerV1: GlobalData] => XdgToplevelIconManager);
delegate_dispatch!(WinitState: [XdgToplevelIconV1: ()] => XdgToplevelIconManager);
