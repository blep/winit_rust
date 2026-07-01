//! Wayland tablet protocol handling (`zwp_tablet_*` v2).
//!
//! Receives pen/stylus events via the `wp-tablet` protocol and translates
//! them into winit's `WindowEvent::Pen(PenEvent)` events.

use ahash::AHashMap;

use sctk::globals::GlobalData;
use sctk::reexports::client::backend::ObjectId;
use sctk::reexports::client::globals::{BindError, GlobalList};
use sctk::reexports::client::protocol::wl_seat::WlSeat;
use sctk::reexports::client::protocol::wl_surface::WlSurface;
use sctk::reexports::client::{Connection, Dispatch, Proxy, QueueHandle};
use sctk::reexports::client::delegate_dispatch;

use wayland_protocols::wp::tablet::zv2::client::zwp_tablet_manager_v2::ZwpTabletManagerV2;
use wayland_protocols::wp::tablet::zv2::client::zwp_tablet_seat_v2::ZwpTabletSeatV2;
use wayland_protocols::wp::tablet::zv2::client::zwp_tablet_seat_v2;
use wayland_protocols::wp::tablet::zv2::client::zwp_tablet_tool_v2::ZwpTabletToolV2;
use wayland_protocols::wp::tablet::zv2::client::zwp_tablet_tool_v2;
use wayland_protocols::wp::tablet::zv2::client::zwp_tablet_v2::ZwpTabletV2;

use crate::dpi::PhysicalPosition;
use crate::event::{Force, PenEvent, PenToolType, TouchPhase, WindowEvent};
use crate::platform_impl::wayland::state::WinitState;
use crate::platform_impl::wayland::{DeviceId, WindowId};

/// Per-tool state accumulated between events.
#[derive(Debug, Default)]
struct ToolFrame {
    in_proximity: bool,
    touching: bool,
    surface: Option<WlSurface>,
    position: Option<(f64, f64)>,
    pressure: Option<u32>,
    tilt: Option<(f64, f64)>,
    distance: Option<u32>,
    rotation: Option<f64>,
    buttons: u32,
    removed: bool,
}

/// Per-tool persistent state.
struct PenTool {
    _tool: ZwpTabletToolV2,
    tool_type: PenToolType,
    frame: ToolFrame,
    serial: u64,
}

/// Per-seat tablet state.
struct SeatTabletState {
    _tablet_seat: ZwpTabletSeatV2,
    tools: AHashMap<ObjectId, PenTool>,
    _tablets: Vec<ZwpTabletV2>,
}

/// Root state for the tablet protocol.
pub struct TabletState {
    _manager: ZwpTabletManagerV2,
    seats: AHashMap<ObjectId, SeatTabletState>,
    next_id: std::sync::atomic::AtomicU64,
}

impl TabletState {
    pub fn new(
        globals: &GlobalList,
        queue_handle: &QueueHandle<WinitState>,
    ) -> Result<Self, BindError> {
        let manager = globals.bind(queue_handle, 1..=1, GlobalData)?;
        Ok(Self {
            _manager: manager,
            seats: AHashMap::new(),
            next_id: std::sync::atomic::AtomicU64::new(1),
        })
    }

    pub fn add_seat(&mut self, seat: &WlSeat, queue_handle: &QueueHandle<WinitState>) {
        let tablet_seat = self._manager.get_tablet_seat(seat, queue_handle, GlobalData);
        self.seats.insert(
            seat.id(),
            SeatTabletState {
                _tablet_seat: tablet_seat,
                tools: AHashMap::new(),
                _tablets: Vec::new(),
            },
        );
    }

    pub fn remove_seat(&mut self, seat: &ObjectId) {
        self.seats.remove(seat);
    }

    fn next_pen_id(&self) -> u64 {
        self.next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    fn find_window(state: &WinitState, surface: &WlSurface) -> Option<WindowId> {
        let wid = crate::platform_impl::wayland::make_wid(surface);
        if state.windows.borrow().contains_key(&wid) {
            Some(wid)
        } else {
            None
        }
    }
}

// ── Dispatch implementations ───────────────────────────────────────────────

impl Dispatch<ZwpTabletManagerV2, GlobalData, WinitState> for TabletState {
    fn event(
        _state: &mut WinitState,
        _proxy: &ZwpTabletManagerV2,
        _event: <ZwpTabletManagerV2 as Proxy>::Event,
        _data: &GlobalData,
        _conn: &Connection,
        _qhandle: &QueueHandle<WinitState>,
    ) {
    }
}

impl Dispatch<ZwpTabletSeatV2, GlobalData, WinitState> for TabletState {
    fn event(
        state: &mut WinitState,
        _proxy: &ZwpTabletSeatV2,
        event: <ZwpTabletSeatV2 as Proxy>::Event,
        _data: &GlobalData,
        _conn: &Connection,
        _qhandle: &QueueHandle<WinitState>,
    ) {
        let tablet_state = match state.tablet_state.as_mut() {
            Some(ts) => ts,
            None => return,
        };

        match event {
            zwp_tablet_seat_v2::Event::TabletAdded { id: _tablet } => {
                // Tablet info is tracked implicitly via the tablet_seat.
            },
            zwp_tablet_seat_v2::Event::ToolAdded { id: tool } => {
                let tool_id = tool.id();
                let pen_tool = PenTool {
                    _tool: tool,
                    tool_type: PenToolType::Pen,
                    frame: ToolFrame::default(),
                    serial: tablet_state.next_pen_id(),
                };
                if let Some(seat_state) = tablet_state.seats.values_mut().next() {
                    seat_state.tools.insert(tool_id, pen_tool);
                }
            },
            zwp_tablet_seat_v2::Event::PadAdded { .. } => {},
            _ => {},
        }
    }

    sctk::reexports::client::event_created_child!(WinitState, ZwpTabletSeatV2, [
        0 => (ZwpTabletV2, GlobalData),
        1 => (ZwpTabletToolV2, GlobalData),
    ]);
}

impl Dispatch<ZwpTabletToolV2, GlobalData, WinitState> for TabletState {
    fn event(
        state: &mut WinitState,
        proxy: &ZwpTabletToolV2,
        event: <ZwpTabletToolV2 as Proxy>::Event,
        _data: &GlobalData,
        _conn: &Connection,
        _qhandle: &QueueHandle<WinitState>,
    ) {
        // ── Phase 1: Update tool state, extract surface for emits ──
        let mut emit_surface: Option<WlSurface> = None;

        {
            let tablet_state = match state.tablet_state.as_mut() {
                Some(ts) => ts,
                None => return,
            };

            let tool_id = proxy.id();
            let seat_state = match tablet_state.seats.values_mut().next() {
                Some(s) => s,
                None => return,
            };
            let tool = match seat_state.tools.get_mut(&tool_id) {
                Some(t) => t,
                None => return,
            };

            match event {
                zwp_tablet_tool_v2::Event::Type { tool_type } => {
                    use sctk::reexports::client::WEnum;
                    tool.tool_type = match tool_type {
                        WEnum::Value(ty) => match ty {
                            zwp_tablet_tool_v2::Type::Pen => PenToolType::Pen,
                            zwp_tablet_tool_v2::Type::Eraser => PenToolType::Eraser,
                            _ => PenToolType::Pen,
                        },
                        _ => PenToolType::Unknown,
                    };
                },
                zwp_tablet_tool_v2::Event::HardwareSerial { hardware_serial_hi, hardware_serial_lo } => {
                    tool.serial = (hardware_serial_hi as u64) << 32 | hardware_serial_lo as u64;
                },
                zwp_tablet_tool_v2::Event::ProximityIn { tablet: _, surface, .. } => {
                    tool.frame.in_proximity = true;
                    tool.frame.surface = Some(surface);
                },
                zwp_tablet_tool_v2::Event::ProximityOut => {
                    emit_surface = tool.frame.surface.clone();
                    tool.frame = ToolFrame::default();
                    tool.frame.removed = false;
                },
                zwp_tablet_tool_v2::Event::Down { .. } => {
                    tool.frame.touching = true;
                },
                zwp_tablet_tool_v2::Event::Up => {
                    tool.frame.touching = false;
                },
                zwp_tablet_tool_v2::Event::Motion { x, y } => {
                    tool.frame.position = Some((x, y));
                },
                zwp_tablet_tool_v2::Event::Pressure { pressure } => {
                    tool.frame.pressure = Some(pressure);
                },
                zwp_tablet_tool_v2::Event::Distance { distance } => {
                    tool.frame.distance = Some(distance);
                },
                zwp_tablet_tool_v2::Event::Tilt { tilt_x, tilt_y } => {
                    tool.frame.tilt = Some((tilt_x, tilt_y));
                },
                zwp_tablet_tool_v2::Event::Rotation { degrees } => {
                    tool.frame.rotation = Some(degrees);
                },
                zwp_tablet_tool_v2::Event::Button { button, state, .. } => {
                    let bit = 1u32.wrapping_shl(button);
                    if state == sctk::reexports::client::WEnum::Value(zwp_tablet_tool_v2::ButtonState::Pressed) {
                        tool.frame.buttons |= bit;
                    } else {
                        tool.frame.buttons &= !bit;
                    }
                },
                zwp_tablet_tool_v2::Event::Frame { .. } => {
                    emit_surface = tool.frame.surface.clone();
                },
                zwp_tablet_tool_v2::Event::Removed => {
                    tool.frame.removed = true;
                },
                zwp_tablet_tool_v2::Event::Done => {},
                zwp_tablet_tool_v2::Event::Capability { .. } => {},
                zwp_tablet_tool_v2::Event::HardwareIdWacom { .. } => {},
                _ => {},
            }
        }

        // ── Phase 2: Emit pen event ──
        let pen_data: Option<(PenEvent, WindowId)> = (|| {
            let window_id = Self::find_window(state, emit_surface.as_ref()?)?;
            let tool = state.tablet_state.as_ref()?
                .seats.values().next()?
                .tools.get(&proxy.id())?;
            let f = &tool.frame;
            let location = PhysicalPosition::new(f.position?.0, f.position?.1);
            let phase = if f.removed { TouchPhase::Ended }
                else if f.touching { TouchPhase::Moved }
                else if f.in_proximity { TouchPhase::Moved }
                else { TouchPhase::Ended };
            let force = match (f.touching, f.pressure) {
                (true, Some(p)) if p > 0 => Some(Force::Normalized(p as f64 / 65535.0)),
                (true, _) => Some(Force::Normalized(0.0)),
                (false, _) => None,
            };
            // Negate tilt_x to match Android convention (positive = right)
            let tilt_x = f.tilt.map(|(tx, _)| (-tx).to_radians());
            let tilt_y = f.tilt.map(|(_, ty)| ty.to_radians());
            // Orientation (azimuth) is the direction of the tilt vector
            // on the screen plane, always derived from tilt_x/tilt_y.
            // (The protocol's `rotation` event is barrel twist, not azimuth.)
            let orientation = match (tilt_x, tilt_y) {
                (Some(tx), Some(ty)) => Some(f64::atan2(tx, ty)),
                _ => None,
            };
            // hover_distance: 0.0 = at surface (touching), 1.0 = hovering,
            // None = no pen in proximity. Matches Android convention.
            let hover_distance = f.distance
                .map(|d| d as f64 / 65535.0)
                .or_else(|| if force.is_some() { Some(0.0) } else { Some(1.0) });
            let pen = PenEvent {
                device_id: crate::event::DeviceId(
                    crate::platform_impl::DeviceId::Wayland(DeviceId),
                ),
                phase,
                location,
                force,
                tilt_x,
                tilt_y,
                orientation,
                hover_distance,
                tool_type: Some(tool.tool_type),
                button_state: Some(f.buttons),
                id: tool.serial,
            };
            Some((pen, window_id))
        })();

        if let Some((pen_event, window_id)) = pen_data {
            state
                .events_sink
                .push_window_event(WindowEvent::Pen(pen_event), window_id);
        }
    }
}

impl Dispatch<ZwpTabletV2, GlobalData, WinitState> for TabletState {
    fn event(
        _state: &mut WinitState,
        _proxy: &ZwpTabletV2,
        _event: <ZwpTabletV2 as Proxy>::Event,
        _data: &GlobalData,
        _conn: &Connection,
        _qhandle: &QueueHandle<WinitState>,
    ) {
    }
}

delegate_dispatch!(WinitState: [ZwpTabletManagerV2: GlobalData] => TabletState);
delegate_dispatch!(WinitState: [ZwpTabletSeatV2: GlobalData] => TabletState);
delegate_dispatch!(WinitState: [ZwpTabletToolV2: GlobalData] => TabletState);
delegate_dispatch!(WinitState: [ZwpTabletV2: GlobalData] => TabletState);
