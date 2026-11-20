use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Write as _;
use std::os::fd::AsFd;
use std::path::Path;
use std::sync::{Arc, mpsc};
use std::thread;

use memmap2::MmapMut;
use wayland_client::protocol::{
    wl_buffer::WlBuffer,
    wl_output::{self, WlOutput},
    wl_registry::{self, WlRegistry},
    wl_seat::WlSeat,
    wl_shm::{self, WlShm},
    wl_shm_pool::WlShmPool,
};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
    zwlr_data_control_source_v1::{self, ZwlrDataControlSourceV1},
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

/// A rectangular selection in an output's logical coordinate space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureRegion {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// Captured RGBA8 screenshot pixels.
#[derive(Debug, Clone)]
pub struct CapturedImage {
    pub width: u32,
    pub height: u32,
    /// Tightly packed RGBA8 pixel data (`width * height * 4` bytes).
    pub rgba: Vec<u8>,
}

impl CapturedImage {
    /// Crop this physical-resolution capture using a region in the output's logical coordinates.
    pub fn crop_logical(
        &self,
        region: CaptureRegion,
        logical_w: f32,
        logical_h: f32,
    ) -> Result<Self, String> {
        if self.width == 0 || self.height == 0 {
            return Err(String::from("cannot crop an empty capture"));
        }
        if logical_w <= 0.0 || logical_h <= 0.0 {
            return Err(String::from("logical output dimensions must be positive"));
        }
        if region.width <= 0 || region.height <= 0 {
            return Err(String::from("capture region must have positive size"));
        }

        let scale_x = self.width as f32 / logical_w;
        let scale_y = self.height as f32 / logical_h;

        let x0 = ((region.x.max(0) as f32) * scale_x)
            .round()
            .clamp(0.0, self.width as f32) as u32;
        let y0 = ((region.y.max(0) as f32) * scale_y)
            .round()
            .clamp(0.0, self.height as f32) as u32;
        let x1 = (((region.x + region.width).max(0) as f32) * scale_x)
            .round()
            .clamp(0.0, self.width as f32) as u32;
        let y1 = (((region.y + region.height).max(0) as f32) * scale_y)
            .round()
            .clamp(0.0, self.height as f32) as u32;

        if x1 <= x0 || y1 <= y0 {
            return Err(String::from("cropped region is empty"));
        }

        let crop_w = x1 - x0;
        let crop_h = y1 - y0;
        let mut rgba = Vec::with_capacity((crop_w * crop_h * 4) as usize);
        let src_stride = (self.width * 4) as usize;
        let row_start = (x0 * 4) as usize;
        let row_bytes = (crop_w * 4) as usize;

        for y in y0..y1 {
            let offset = (y as usize) * src_stride + row_start;
            rgba.extend_from_slice(&self.rgba[offset..offset + row_bytes]);
        }

        Ok(Self {
            width: crop_w,
            height: crop_h,
            rgba,
        })
    }

    /// Encode the RGBA8 image into PNG bytes.
    pub fn encode_png(&self) -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut out, self.width, self.height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.set_compression(png::Compression::Fast);
            let mut writer = encoder
                .write_header()
                .map_err(|err| format!("png header error: {err}"))?;
            writer
                .write_image_data(&self.rgba)
                .map_err(|err| format!("png encode error: {err}"))?;
        }
        Ok(out)
    }

    /// Encode to PNG, write to `path` (creating parent directories if needed), and return the PNG bytes.
    pub fn save_png(&self, path: &Path) -> Result<Vec<u8>, String> {
        let png_bytes = self.encode_png()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|err| format!("failed to create directory {}: {err}", parent.display()))?;
        }
        fs::write(path, &png_bytes)
            .map_err(|err| format!("failed to write {}: {err}", path.display()))?;
        Ok(png_bytes)
    }
}

/// Pure-Rust Wayland screenshot capture (`zwlr_screencopy_manager_v1`),
/// Wayland clipboard ownership (`zwlr_data_control_manager_v1`),
/// and D-Bus desktop notification helper.
pub struct Screenshot;

impl Screenshot {
    /// Capture an entire output (matched by `wl_output` name, or the first output if `None`)
    /// at its native physical resolution.
    pub fn capture_output(output_name: Option<&str>) -> Result<CapturedImage, String> {
        let conn = Connection::connect_to_env()
            .map_err(|err| format!("failed to connect to Wayland display: {err}"))?;
        let display = conn.display();
        let mut queue = conn.new_event_queue::<CaptureState>();
        let qh = queue.handle();

        let mut state = CaptureState::default();
        let _registry = display.get_registry(&qh, ());

        // First roundtrip binds globals; second receives wl_output names.
        queue
            .roundtrip(&mut state)
            .map_err(|err| format!("Wayland registry roundtrip failed: {err}"))?;
        queue
            .roundtrip(&mut state)
            .map_err(|err| format!("Wayland output roundtrip failed: {err}"))?;

        let manager = state
            .screencopy
            .as_ref()
            .ok_or_else(|| String::from("compositor does not expose zwlr_screencopy_manager_v1"))?
            .clone();
        if state.shm.is_none() {
            return Err(String::from("compositor does not expose wl_shm"));
        }

        let output = pick_output(&state.outputs, output_name)
            .ok_or_else(|| String::from("no matching Wayland output found"))?
            .clone();

        let frame = manager.capture_output(0, &output, &qh, ());
        state.frame_version = frame.version();

        while !state.ready && !state.failed && state.error.is_none() {
            queue
                .blocking_dispatch(&mut state)
                .map_err(|err| format!("Wayland screencopy dispatch failed: {err}"))?;
        }

        frame.destroy();
        if let Some(buf) = state.buffer.take() {
            buf.destroy();
        }
        if let Some(pool) = state.pool.take() {
            pool.destroy();
        }

        if let Some(err) = state.error {
            return Err(err);
        }
        if state.failed {
            return Err(String::from("compositor reported screencopy frame failure"));
        }

        let info = state
            .shm_info
            .ok_or_else(|| String::from("compositor did not advertise an SHM buffer format"))?;
        let mmap = state
            .mmap
            .as_ref()
            .ok_or_else(|| String::from("screencopy SHM mapping missing"))?;

        let rgba = convert_shm_to_rgba(
            mmap,
            info.width,
            info.height,
            info.stride,
            info.format,
            state.y_invert,
        )?;

        Ok(CapturedImage {
            width: info.width,
            height: info.height,
            rgba,
        })
    }

    /// Offer raw bytes (e.g. PNG with mime `"image/png"`) to the Wayland clipboard
    /// via `zwlr_data_control_manager_v1`. Spawns a background thread that serves paste
    /// requests until another application replaces the clipboard selection.
    pub fn copy_to_clipboard(mime: &str, bytes: Vec<u8>) -> Result<(), String> {
        let mime = mime.to_string();
        let data = Arc::new(bytes);
        let (tx, rx) = mpsc::sync_channel::<Result<(), String>>(1);

        thread::Builder::new()
            .name(String::from("amane-clipboard"))
            .spawn(move || run_clipboard_server(mime, data, tx))
            .map_err(|err| format!("failed to spawn clipboard thread: {err}"))?;

        rx.recv()
            .map_err(|_| String::from("clipboard thread exited before registering selection"))?
    }

    /// Send a desktop notification over the session D-Bus (`org.freedesktop.Notifications.Notify`).
    pub fn notify(
        app: &str,
        summary: &str,
        body: &str,
        image_path: Option<&Path>,
    ) -> Result<u32, String> {
        let conn = zbus::blocking::Connection::session()
            .map_err(|err| format!("failed to connect to session D-Bus: {err}"))?;

        let mut hints: HashMap<&str, zbus::zvariant::Value<'_>> = HashMap::new();
        let img_str;
        if let Some(path) = image_path {
            img_str = path.to_string_lossy().to_string();
            hints.insert("image-path", zbus::zvariant::Value::from(img_str.as_str()));
        }

        let actions: Vec<&str> = Vec::new();
        let reply = conn
            .call_method(
                Some("org.freedesktop.Notifications"),
                "/org/freedesktop/Notifications",
                Some("org.freedesktop.Notifications"),
                "Notify",
                &(app, 0u32, "", summary, body, actions, hints, 5000i32),
            )
            .map_err(|err| format!("D-Bus Notify call failed: {err}"))?;

        reply
            .body()
            .deserialize::<u32>()
            .map_err(|err| format!("failed to decode Notify reply: {err}"))
    }
}

// ---------------------------------------------------------------------------
// Screencopy Wayland client state
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct ShmBufferInfo {
    format: wl_shm::Format,
    width: u32,
    height: u32,
    stride: u32,
}

struct OutputEntry {
    global_name: u32,
    output: WlOutput,
    name: Option<String>,
}

#[derive(Default)]
struct CaptureState {
    shm: Option<WlShm>,
    screencopy: Option<ZwlrScreencopyManagerV1>,
    outputs: Vec<OutputEntry>,
    frame_version: u32,
    shm_info: Option<ShmBufferInfo>,
    copy_requested: bool,
    pool: Option<WlShmPool>,
    buffer: Option<WlBuffer>,
    mmap: Option<MmapMut>,
    _shm_file: Option<File>,
    y_invert: bool,
    ready: bool,
    failed: bool,
    error: Option<String>,
}

impl CaptureState {
    fn request_copy(
        &mut self,
        frame: &ZwlrScreencopyFrameV1,
        qh: &QueueHandle<Self>,
    ) {
        if self.copy_requested {
            return;
        }
        let Some(info) = self.shm_info else {
            return;
        };
        let Some(shm) = self.shm.as_ref() else {
            self.error = Some(String::from("wl_shm unavailable"));
            return;
        };

        let size = (info.stride as usize).saturating_mul(info.height as usize);
        if size == 0 {
            self.error = Some(String::from("compositor advertised empty screencopy buffer"));
            return;
        }

        let file = match tempfile::tempfile() {
            Ok(f) => f,
            Err(err) => {
                self.error = Some(format!("failed to create SHM tempfile: {err}"));
                return;
            }
        };
        if let Err(err) = file.set_len(size as u64) {
            self.error = Some(format!("failed to size SHM tempfile: {err}"));
            return;
        }
        // SAFETY: The tempfile is exclusively owned by this capture and resized to `size` above.
        let mmap = match unsafe { MmapMut::map_mut(&file) } {
            Ok(m) => m,
            Err(err) => {
                self.error = Some(format!("failed to mmap SHM buffer: {err}"));
                return;
            }
        };

        let pool = shm.create_pool(file.as_fd(), size as i32, qh, ());
        let buffer = pool.create_buffer(
            0,
            info.width as i32,
            info.height as i32,
            info.stride as i32,
            info.format,
            qh,
            (),
        );

        frame.copy(&buffer);

        self.copy_requested = true;
        self.pool = Some(pool);
        self.buffer = Some(buffer);
        self.mmap = Some(mmap);
        self._shm_file = Some(file);
    }
}

fn pick_output<'a>(outputs: &'a [OutputEntry], wanted: Option<&str>) -> Option<&'a WlOutput> {
    if let Some(name) = wanted
        && let Some(found) = outputs
            .iter()
            .find(|entry| entry.name.as_deref() == Some(name))
    {
        return Some(&found.output);
    }
    outputs.first().map(|entry| &entry.output)
}

impl Dispatch<WlRegistry, ()> for CaptureState {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_shm" => {
                    state.shm = Some(registry.bind::<WlShm, _, _>(name, 1, qh, ()));
                }
                "zwlr_screencopy_manager_v1" => {
                    let ver = version.min(3);
                    state.screencopy =
                        Some(registry.bind::<ZwlrScreencopyManagerV1, _, _>(name, ver, qh, ()));
                }
                "wl_output" => {
                    let ver = version.min(4);
                    let output = registry.bind::<WlOutput, _, _>(name, ver, qh, name);
                    state.outputs.push(OutputEntry {
                        global_name: name,
                        output,
                        name: None,
                    });
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<WlOutput, u32> for CaptureState {
    fn event(
        state: &mut Self,
        _: &WlOutput,
        event: wl_output::Event,
        global_name: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event
            && let Some(entry) = state
                .outputs
                .iter_mut()
                .find(|entry| entry.global_name == *global_name)
        {
            entry.name = Some(name);
        }
    }
}

impl Dispatch<WlShm, ()> for CaptureState {
    fn event(
        _: &mut Self,
        _: &WlShm,
        _: wl_shm::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlShmPool, ()> for CaptureState {
    fn event(
        _: &mut Self,
        _: &WlShmPool,
        _: wayland_client::protocol::wl_shm_pool::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlBuffer, ()> for CaptureState {
    fn event(
        _: &mut Self,
        _: &WlBuffer,
        _: wayland_client::protocol::wl_buffer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrScreencopyManagerV1, ()> for CaptureState {
    fn event(
        _: &mut Self,
        _: &ZwlrScreencopyManagerV1,
        _: wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for CaptureState {
    fn event(
        state: &mut Self,
        frame: &ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer {
                format,
                width,
                height,
                stride,
            } => {
                if let WEnum::Value(fmt) = format {
                    // Prefer 32-bit RGB/BGR formats if multiple Buffer events are sent.
                    if state.shm_info.is_none() || is_supported_shm_format(fmt) {
                        state.shm_info = Some(ShmBufferInfo {
                            format: fmt,
                            width,
                            height,
                            stride,
                        });
                    }
                    if state.frame_version < 3 {
                        state.request_copy(frame, qh);
                    }
                }
            }
            zwlr_screencopy_frame_v1::Event::BufferDone => {
                state.request_copy(frame, qh);
            }
            zwlr_screencopy_frame_v1::Event::Flags { flags } => {
                if let WEnum::Value(f) = flags {
                    state.y_invert = f.contains(zwlr_screencopy_frame_v1::Flags::YInvert);
                }
            }
            zwlr_screencopy_frame_v1::Event::Ready { .. } => {
                state.ready = true;
            }
            zwlr_screencopy_frame_v1::Event::Failed => {
                state.failed = true;
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Data-control Wayland clipboard server
// ---------------------------------------------------------------------------

struct ClipboardState {
    manager: Option<ZwlrDataControlManagerV1>,
    seat: Option<WlSeat>,
    data: Arc<Vec<u8>>,
    cancelled: bool,
}

fn run_clipboard_server(
    mime: String,
    data: Arc<Vec<u8>>,
    ready_tx: mpsc::SyncSender<Result<(), String>>,
) {
    let conn = match Connection::connect_to_env() {
        Ok(c) => c,
        Err(err) => {
            let _ = ready_tx.send(Err(format!("clipboard Wayland connect failed: {err}")));
            return;
        }
    };

    let display = conn.display();
    let mut queue = conn.new_event_queue::<ClipboardState>();
    let qh = queue.handle();

    let mut state = ClipboardState {
        manager: None,
        seat: None,
        data,
        cancelled: false,
    };

    let _registry = display.get_registry(&qh, ());
    if let Err(err) = queue.roundtrip(&mut state) {
        let _ = ready_tx.send(Err(format!("clipboard registry roundtrip failed: {err}")));
        return;
    }

    let Some(manager) = state.manager.as_ref() else {
        let _ = ready_tx.send(Err(String::from(
            "compositor does not expose zwlr_data_control_manager_v1",
        )));
        return;
    };
    let Some(seat) = state.seat.as_ref() else {
        let _ = ready_tx.send(Err(String::from("compositor does not expose wl_seat")));
        return;
    };

    let source = manager.create_data_source(&qh, ());
    source.offer(mime);

    let device = manager.get_data_device(seat, &qh, ());
    device.set_selection(Some(&source));

    if let Err(err) = queue.roundtrip(&mut state) {
        let _ = ready_tx.send(Err(format!("clipboard set_selection roundtrip failed: {err}")));
        return;
    }

    let _ = ready_tx.send(Ok(()));

    while !state.cancelled {
        if queue.blocking_dispatch(&mut state).is_err() {
            break;
        }
    }

    source.destroy();
    device.destroy();
}

impl Dispatch<WlRegistry, ()> for ClipboardState {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "zwlr_data_control_manager_v1" => {
                    let ver = version.min(2);
                    state.manager =
                        Some(registry.bind::<ZwlrDataControlManagerV1, _, _>(name, ver, qh, ()));
                }
                "wl_seat" if state.seat.is_none() => {
                    state.seat = Some(registry.bind::<WlSeat, _, _>(name, 1, qh, ()));
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<WlSeat, ()> for ClipboardState {
    fn event(
        _: &mut Self,
        _: &WlSeat,
        _: wayland_client::protocol::wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrDataControlManagerV1, ()> for ClipboardState {
    fn event(
        _: &mut Self,
        _: &ZwlrDataControlManagerV1,
        _: wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for ClipboardState {
    fn event(
        _: &mut Self,
        _: &ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Destroy any incoming offers we don't care about so they don't leak proxies.
        match event {
            zwlr_data_control_device_v1::Event::DataOffer { id } => id.destroy(),
            zwlr_data_control_device_v1::Event::Finished => {}
            _ => {}
        }
    }

    wayland_client::event_created_child!(ClipboardState, ZwlrDataControlDeviceV1, [
        zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ZwlrDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ZwlrDataControlOfferV1, ()> for ClipboardState {
    fn event(
        _: &mut Self,
        _: &ZwlrDataControlOfferV1,
        _: zwlr_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrDataControlSourceV1, ()> for ClipboardState {
    fn event(
        state: &mut Self,
        _: &ZwlrDataControlSourceV1,
        event: zwlr_data_control_source_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_source_v1::Event::Send { fd, .. } => {
                let bytes = Arc::clone(&state.data);
                thread::spawn(move || {
                    let mut file = File::from(fd);
                    let _ = file.write_all(&bytes);
                });
            }
            zwlr_data_control_source_v1::Event::Cancelled => {
                state.cancelled = true;
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Pixel format conversion
// ---------------------------------------------------------------------------

fn is_supported_shm_format(format: wl_shm::Format) -> bool {
    matches!(
        format,
        wl_shm::Format::Argb8888
            | wl_shm::Format::Xrgb8888
            | wl_shm::Format::Abgr8888
            | wl_shm::Format::Xbgr8888
            | wl_shm::Format::Bgra8888
            | wl_shm::Format::Bgrx8888
            | wl_shm::Format::Rgba8888
            | wl_shm::Format::Rgbx8888
    )
}

fn convert_shm_to_rgba(
    src: &[u8],
    width: u32,
    height: u32,
    stride: u32,
    format: wl_shm::Format,
    y_invert: bool,
) -> Result<Vec<u8>, String> {
    if !is_supported_shm_format(format) {
        return Err(format!("unsupported screencopy pixel format: {format:?}"));
    }
    let min_bytes = (stride as usize).saturating_mul(height as usize);
    if src.len() < min_bytes {
        return Err(String::from("SHM buffer smaller than expected stride * height"));
    }

    let mut out = vec![0u8; (width as usize) * (height as usize) * 4];
    for y in 0..height {
        let src_y = if y_invert { height - 1 - y } else { y };
        let src_row = (src_y as usize) * (stride as usize);
        let dst_row = (y as usize) * (width as usize) * 4;

        for x in 0..width as usize {
            let s = src_row + x * 4;
            let d = dst_row + x * 4;
            let (r, g, b) = match format {
                // Little-endian ARGB8888 / XRGB8888 -> [B, G, R, A/X]
                wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888 => {
                    (src[s + 2], src[s + 1], src[s])
                }
                // Little-endian ABGR8888 / XBGR8888 -> [R, G, B, A/X]
                wl_shm::Format::Abgr8888 | wl_shm::Format::Xbgr8888 => {
                    (src[s], src[s + 1], src[s + 2])
                }
                // Little-endian BGRA8888 / BGRX8888 -> [A/X, R, G, B]
                wl_shm::Format::Bgra8888 | wl_shm::Format::Bgrx8888 => {
                    (src[s + 1], src[s + 2], src[s + 3])
                }
                // Little-endian RGBA8888 / RGBX8888 -> [A/X, B, G, R]
                wl_shm::Format::Rgba8888 | wl_shm::Format::Rgbx8888 => {
                    (src[s + 3], src[s + 2], src[s + 1])
                }
                _ => unreachable!(),
            };
            out[d] = r;
            out[d + 1] = g;
            out[d + 2] = b;
            out[d + 3] = 255;
        }
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_xrgb8888_and_y_invert_to_opaque_rgba() {
        // 2x2 image: top row red/green, bottom row blue/white in little-endian XRGB8888 [B, G, R, 0]
        let src = vec![
            0, 0, 255, 0,   0, 255, 0, 0,
            255, 0, 0, 0,   255, 255, 255, 0,
        ];
        let rgba =
            convert_shm_to_rgba(&src, 2, 2, 8, wl_shm::Format::Xrgb8888, true).expect("convert");
        // Inverted: bottom row becomes top row
        assert_eq!(
            rgba,
            vec![
                0, 0, 255, 255,   255, 255, 255, 255,
                255, 0, 0, 255,   0, 255, 0, 255,
            ]
        );
    }

    #[test]
    fn crop_logical_scales_to_physical_pixels() {
        // 6x4 physical image representing a 4x2 logical screen (1.5x X, 2x Y)
        let mut rgba = Vec::with_capacity(6 * 4 * 4);
        for y in 0..4u8 {
            for x in 0..6u8 {
                rgba.extend_from_slice(&[x, y, 100, 255]);
            }
        }
        let img = CapturedImage {
            width: 6,
            height: 4,
            rgba,
        };

        let cropped = img
            .crop_logical(
                CaptureRegion {
                    x: 2,
                    y: 1,
                    width: 2,
                    height: 1,
                },
                4.0,
                2.0,
            )
            .expect("crop");

        assert_eq!(cropped.width, 3);
        assert_eq!(cropped.height, 2);
        // First pixel should be (x=3, y=2)
        assert_eq!(&cropped.rgba[0..4], &[3, 2, 100, 255]);
    }
}
