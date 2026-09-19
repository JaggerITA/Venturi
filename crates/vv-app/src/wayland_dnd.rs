//! Drop di file dal file manager su Wayland, dove winit 0.30 non lo gestisce
//! (solo X11). Un thread con una propria coda sulla connessione di winit,
//! come fa smithay-clipboard; i file arrivano a egui in `raw_input_hook`.

use std::ffi::OsString;
use std::io::Read;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use raw_window_handle::{HasDisplayHandle, RawDisplayHandle};
use sctk::data_device_manager::data_device::{DataDevice, DataDeviceHandler};
use sctk::data_device_manager::data_offer::{DataOfferHandler, DragOffer};
use sctk::data_device_manager::data_source::DataSourceHandler;
use sctk::data_device_manager::{DataDeviceManagerState, WritePipe};
use sctk::reexports::client::globals::registry_queue_init;
use sctk::reexports::client::protocol::wl_data_device::WlDataDevice;
use sctk::reexports::client::protocol::wl_data_device_manager::DndAction;
use sctk::reexports::client::protocol::wl_data_source::WlDataSource;
use sctk::reexports::client::protocol::wl_seat::WlSeat;
use sctk::reexports::client::protocol::wl_surface::WlSurface;
use sctk::reexports::client::{Connection, QueueHandle, backend::Backend};
use sctk::registry::{ProvidesRegistryState, RegistryState};
use sctk::seat::{Capability, SeatHandler, SeatState};
use sctk::{delegate_data_device, delegate_registry, delegate_seat, registry_handlers};

const URI_LIST: &str = "text/uri-list";

#[derive(Default)]
struct Shared {
    hovering: bool,
    dropped: Vec<PathBuf>,
}

pub struct WaylandDnd {
    shared: Arc<Mutex<Shared>>,
}

impl WaylandDnd {
    /// `None` fuori da Wayland o se il compositor non offre il DnD.
    pub fn start(cc: &eframe::CreationContext<'_>) -> Option<Self> {
        let RawDisplayHandle::Wayland(handle) = cc.display_handle().ok()?.as_raw() else {
            return None;
        };
        // Il display resta valido per tutta la vita del processo: è di winit.
        let backend = unsafe { Backend::from_foreign_display(handle.display.as_ptr().cast()) };
        let conn = Connection::from_backend(backend);
        let (globals, mut queue) = registry_queue_init::<State>(&conn).ok()?;
        let qh = queue.handle();
        let shared = Arc::new(Mutex::new(Shared::default()));
        let mut state = State {
            registry: RegistryState::new(&globals),
            seats: SeatState::new(&globals, &qh),
            manager: DataDeviceManagerState::bind(&globals, &qh).ok()?,
            devices: Vec::new(),
            conn: conn.clone(),
            shared: shared.clone(),
            ctx: cc.egui_ctx.clone(),
        };
        for seat in state.seats.seats() {
            state.add_device(&qh, &seat);
        }
        std::thread::Builder::new()
            .name("wayland-dnd".into())
            .spawn(move || {
            loop {
                if let Err(e) = queue.blocking_dispatch(&mut state) {
                    eprintln!("wayland-dnd: {e}");
                    return;
                }
            }
        })
            .ok()?;
        Some(Self { shared })
    }

    pub fn feed(&self, raw_input: &mut egui::RawInput) {
        let mut shared = self.shared.lock().unwrap();
        if shared.hovering {
            raw_input.hovered_files.push(egui::HoveredFile::default());
        }
        raw_input.dropped_files.extend(
            shared
                .dropped
                .drain(..)
                .map(|path| Arc::new(DroppedPath(path)) as Arc<dyn egui::DroppedFile + Send + Sync>),
        );
    }
}

#[derive(Debug)]
struct DroppedPath(PathBuf);

impl egui::DroppedFile for DroppedPath {
    fn path(&self) -> &std::path::Path {
        &self.0
    }
    fn bytes(&self) -> Result<Vec<u8>, String> {
        std::fs::read(&self.0).map_err(|e| e.to_string())
    }
}

struct State {
    registry: RegistryState,
    seats: SeatState,
    manager: DataDeviceManagerState,
    devices: Vec<DataDevice>,
    conn: Connection,
    shared: Arc<Mutex<Shared>>,
    ctx: egui::Context,
}

impl State {
    fn add_device(&mut self, qh: &QueueHandle<Self>, seat: &WlSeat) {
        self.devices.push(self.manager.get_data_device(qh, seat));
    }

    fn drag_offer(&self, device: &WlDataDevice) -> Option<DragOffer> {
        self.devices.iter().find(|d| d.inner() == device)?.data().drag_offer()
    }

    fn set_hovering(&self, hovering: bool) {
        self.shared.lock().unwrap().hovering = hovering;
        self.ctx.request_repaint();
    }
}

impl DataDeviceHandler for State {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        device: &WlDataDevice,
        _: f64,
        _: f64,
        _: &WlSurface,
    ) {
        let Some(offer) = self.drag_offer(device) else {
            return;
        };
        let has_files = offer.with_mime_types(|mimes| mimes.iter().any(|m| m == URI_LIST));
        offer.accept_mime_type(offer.serial, has_files.then(|| URI_LIST.to_string()));
        if has_files {
            offer.set_actions(DndAction::Copy, DndAction::Copy);
            self.set_hovering(true);
        }
    }

    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {
        self.set_hovering(false);
    }

    fn motion(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice, _: f64, _: f64) {}

    fn selection(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}

    fn drop_performed(&mut self, _: &Connection, _: &QueueHandle<Self>, device: &WlDataDevice) {
        self.set_hovering(false);
        let Some(offer) = self.drag_offer(device) else {
            return;
        };
        let Ok(mut pipe) = offer.receive(URI_LIST.to_string()) else {
            offer.destroy();
            return;
        };
        let _ = self.conn.flush();
        // La lettura aspetta il client sorgente: non deve bloccare la coda.
        let conn = self.conn.clone();
        let shared = self.shared.clone();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            let mut uri_list = Vec::new();
            let _ = pipe.read_to_end(&mut uri_list);
            offer.finish();
            offer.destroy();
            let _ = conn.flush();
            shared.lock().unwrap().dropped.extend(parse_uri_list(&uri_list));
            ctx.request_repaint();
        });
    }
}

impl DataOfferHandler for State {
    fn source_actions(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        offer: &mut DragOffer,
        _: DndAction,
    ) {
        offer.set_actions(DndAction::Copy, DndAction::Copy);
    }

    fn selected_action(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &mut DragOffer,
        _: DndAction,
    ) {
    }
}

impl DataSourceHandler for State {
    fn accept_mime(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlDataSource,
        _: Option<String>,
    ) {
    }
    fn send_request(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlDataSource,
        _: String,
        _: WritePipe,
    ) {
    }
    fn cancelled(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}
    fn dnd_dropped(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}
    fn dnd_finished(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}
    fn action(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource, _: DndAction) {}
}

impl SeatHandler for State {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seats
    }
    fn new_seat(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: WlSeat) {
        self.add_device(qh, &seat);
    }
    fn new_capability(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlSeat, _: Capability) {}
    fn remove_capability(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlSeat, _: Capability) {
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, seat: WlSeat) {
        self.devices.retain(|d| d.data().seat() != &seat);
    }
}

impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![SeatState];
}

delegate_data_device!(State);
delegate_seat!(State);
delegate_registry!(State);

/// Path locali di un `text/uri-list` (RFC 2483): commenti e URI non `file:`
/// si scartano.
fn parse_uri_list(bytes: &[u8]) -> Vec<PathBuf> {
    bytes
        .split(|&b| b == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
        .filter(|line| !line.starts_with(b"#"))
        .filter_map(|line| line.strip_prefix(b"file://"))
        .filter_map(|rest| {
            // Dopo `file://` c'è l'host, di solito vuoto o `localhost`.
            let path = &rest[rest.iter().position(|&b| b == b'/')?..];
            Some(PathBuf::from(OsString::from_vec(percent_decode(path))))
        })
        .collect()
}

fn percent_decode(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes
            .get(i + 1..i + 3)
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match (bytes[i], hex) {
            (b'%', Some(byte)) => {
                out.push(byte);
                i += 3;
            }
            (b, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_list_yields_decoded_local_paths() {
        let list = b"# commento\r\nfile:///home/me/a%20b.mp4\r\nfile://localhost/tmp/%C3%A8.wav\r\nhttps://x/y\r\n";
        assert_eq!(
            parse_uri_list(list),
            vec![PathBuf::from("/home/me/a b.mp4"), PathBuf::from("/tmp/è.wav")]
        );
    }
}
