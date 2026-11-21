use std::os::fd::AsFd;

use memmap2::MmapMut;
use wayland_client::protocol::{
    wl_buffer::WlBuffer,
    wl_registry::{self, WlRegistry},
    wl_shm::{self, WlShm},
    wl_shm_pool::WlShmPool,
};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1::{self, ExtForeignToplevelHandleV1},
    ext_foreign_toplevel_list_v1::{self, ExtForeignToplevelListV1},
};
use wayland_protocols::ext::image_capture_source::v1::client::{
    ext_foreign_toplevel_image_capture_source_manager_v1::ExtForeignToplevelImageCaptureSourceManagerV1,
    ext_image_capture_source_v1::ExtImageCaptureSourceV1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1},
    ext_image_copy_capture_manager_v1::{self, ExtImageCopyCaptureManagerV1},
    ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
};

use super::screenshot::{CapturedImage, convert_shm_to_rgba, is_supported_shm_format};

struct ToplevelEntry {
    handle: ExtForeignToplevelHandleV1,
    identifier: String,
}

#[derive(Default)]
struct ToplevelCaptureState {
    shm: Option<WlShm>,
    toplevel_list: Option<ExtForeignToplevelListV1>,
    source_manager: Option<ExtForeignToplevelImageCaptureSourceManagerV1>,
    capture_manager: Option<ExtImageCopyCaptureManagerV1>,
    toplevels: Vec<ToplevelEntry>,

    buffer_width: u32,
    buffer_height: u32,
    shm_format: Option<wl_shm::Format>,
    session_done: bool,
    session_stopped: bool,

    frame_ready: bool,
    frame_failed: Option<String>,
}

/// Captures a single SHM frame of the foreign toplevel matching `identifier` via
/// `ext-foreign-toplevel-list-v1` + `ext-image-capture-source-v1` + `ext-image-copy-capture-v1`.
pub fn capture_toplevel(identifier: &str) -> Result<CapturedImage, String> {
    if identifier.trim().is_empty() {
        return Err(String::from("toplevel identifier is empty"));
    }

    let conn = Connection::connect_to_env()
        .map_err(|err| format!("failed to connect to Wayland display: {err}"))?;
    let display = conn.display();
    let mut queue = conn.new_event_queue::<ToplevelCaptureState>();
    let qh = queue.handle();

    let mut state = ToplevelCaptureState::default();
    let _registry = display.get_registry(&qh, ());

    // Roundtrip 1: bind globals; Roundtrip 2: receive toplevel handles; Roundtrip 3: receive identifiers.
    for step in 0..3 {
        queue
            .roundtrip(&mut state)
            .map_err(|err| format!("Wayland roundtrip {step} failed: {err}"))?;
    }

    let shm = state
        .shm
        .clone()
        .ok_or_else(|| String::from("compositor does not expose wl_shm"))?;
    let source_manager = state.source_manager.clone().ok_or_else(|| {
        String::from("compositor does not expose ext_foreign_toplevel_image_capture_source_manager_v1")
    })?;
    let capture_manager = state.capture_manager.clone().ok_or_else(|| {
        String::from("compositor does not expose ext_image_copy_capture_manager_v1")
    })?;

    let target_handle = state
        .toplevels
        .iter()
        .find(|entry| entry.identifier == identifier)
        .map(|entry| entry.handle.clone())
        .ok_or_else(|| format!("toplevel '{identifier}' not found"))?;

    let source = source_manager.create_source(&target_handle, &qh, ());
    let session = capture_manager.create_session(
        &source,
        ext_image_copy_capture_manager_v1::Options::empty(),
        &qh,
        (),
    );

    // Wait for session constraints (buffer size + supported SHM format + done).
    while !state.session_done && !state.session_stopped {
        queue
            .blocking_dispatch(&mut state)
            .map_err(|err| format!("Wayland dispatch failed waiting for capture session: {err}"))?;
    }

    if state.session_stopped {
        cleanup(&mut queue, &mut state, None, Some(&session), Some(&source));
        return Err(String::from("capture session stopped before ready"));
    }

    let width = state.buffer_width;
    let height = state.buffer_height;
    let format = state
        .shm_format
        .ok_or_else(|| String::from("compositor offered no supported SHM format"))?;

    if width == 0 || height == 0 {
        cleanup(&mut queue, &mut state, None, Some(&session), Some(&source));
        return Err(String::from("capture session reported zero buffer dimensions"));
    }

    let stride = width.saturating_mul(4);
    let size = (stride as usize).saturating_mul(height as usize);
    let file = tempfile::tempfile()
        .map_err(|err| format!("failed to allocate SHM buffer file: {err}"))?;
    file.set_len(size as u64)
        .map_err(|err| format!("failed to resize SHM buffer file: {err}"))?;
    let mmap = unsafe { MmapMut::map_mut(&file) }
        .map_err(|err| format!("failed to mmap SHM buffer: {err}"))?;

    let pool = shm.create_pool(file.as_fd(), size as i32, &qh, ());
    let buffer = pool.create_buffer(
        0,
        width as i32,
        height as i32,
        stride as i32,
        format,
        &qh,
        (),
    );

    let frame = session.create_frame(&qh, ());
    frame.attach_buffer(&buffer);
    frame.damage_buffer(0, 0, width as i32, height as i32);
    frame.capture();

    while !state.frame_ready && state.frame_failed.is_none() && !state.session_stopped {
        queue
            .blocking_dispatch(&mut state)
            .map_err(|err| format!("Wayland dispatch failed waiting for capture frame: {err}"))?;
    }

    let result = if let Some(reason) = state.frame_failed.take() {
        Err(format!("toplevel capture failed: {reason}"))
    } else if state.session_stopped && !state.frame_ready {
        Err(String::from("capture session stopped before frame ready"))
    } else {
        convert_shm_to_rgba(&mmap, width, height, stride, format, false).map(|rgba| CapturedImage {
            width,
            height,
            rgba,
        })
    };

    buffer.destroy();
    pool.destroy();
    cleanup(
        &mut queue,
        &mut state,
        Some(&frame),
        Some(&session),
        Some(&source),
    );

    result
}

fn cleanup(
    queue: &mut wayland_client::EventQueue<ToplevelCaptureState>,
    state: &mut ToplevelCaptureState,
    frame: Option<&ExtImageCopyCaptureFrameV1>,
    session: Option<&ExtImageCopyCaptureSessionV1>,
    source: Option<&ExtImageCaptureSourceV1>,
) {
    if let Some(f) = frame {
        f.destroy();
    }
    if let Some(s) = session {
        s.destroy();
    }
    if let Some(src) = source {
        src.destroy();
    }
    for entry in state.toplevels.drain(..) {
        entry.handle.destroy();
    }
    if let Some(list) = state.toplevel_list.take() {
        list.stop();
        list.destroy();
    }
    let _ = queue.roundtrip(state);
}

impl Dispatch<WlRegistry, ()> for ToplevelCaptureState {
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
                "wl_shm" if state.shm.is_none() => {
                    state.shm = Some(registry.bind::<WlShm, _, _>(name, version.min(1), qh, ()));
                }
                "ext_foreign_toplevel_list_v1" if state.toplevel_list.is_none() => {
                    state.toplevel_list = Some(
                        registry.bind::<ExtForeignToplevelListV1, _, _>(
                            name,
                            version.min(1),
                            qh,
                            (),
                        ),
                    );
                }
                "ext_foreign_toplevel_image_capture_source_manager_v1"
                    if state.source_manager.is_none() =>
                {
                    state.source_manager = Some(
                        registry.bind::<ExtForeignToplevelImageCaptureSourceManagerV1, _, _>(
                            name,
                            version.min(1),
                            qh,
                            (),
                        ),
                    );
                }
                "ext_image_copy_capture_manager_v1" if state.capture_manager.is_none() => {
                    state.capture_manager = Some(
                        registry.bind::<ExtImageCopyCaptureManagerV1, _, _>(
                            name,
                            version.min(1),
                            qh,
                            (),
                        ),
                    );
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<WlShm, ()> for ToplevelCaptureState {
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

impl Dispatch<WlShmPool, ()> for ToplevelCaptureState {
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

impl Dispatch<WlBuffer, ()> for ToplevelCaptureState {
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

impl Dispatch<ExtForeignToplevelListV1, ()> for ToplevelCaptureState {
    fn event(
        state: &mut Self,
        _: &ExtForeignToplevelListV1,
        event: ext_foreign_toplevel_list_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_foreign_toplevel_list_v1::Event::Toplevel { toplevel } = event {
            state.toplevels.push(ToplevelEntry {
                handle: toplevel,
                identifier: String::new(),
            });
        }
    }

    wayland_client::event_created_child!(ToplevelCaptureState, ExtForeignToplevelListV1, [
        ext_foreign_toplevel_list_v1::EVT_TOPLEVEL_OPCODE => (ExtForeignToplevelHandleV1, ())
    ]);
}

impl Dispatch<ExtForeignToplevelHandleV1, ()> for ToplevelCaptureState {
    fn event(
        state: &mut Self,
        proxy: &ExtForeignToplevelHandleV1,
        event: ext_foreign_toplevel_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_foreign_toplevel_handle_v1::Event::Identifier { identifier } = event
            && let Some(entry) = state.toplevels.iter_mut().find(|e| &e.handle == proxy)
        {
            entry.identifier = identifier;
        }
    }
}

impl Dispatch<ExtForeignToplevelImageCaptureSourceManagerV1, ()> for ToplevelCaptureState {
    fn event(
        _: &mut Self,
        _: &ExtForeignToplevelImageCaptureSourceManagerV1,
        _: wayland_protocols::ext::image_capture_source::v1::client::ext_foreign_toplevel_image_capture_source_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtImageCaptureSourceV1, ()> for ToplevelCaptureState {
    fn event(
        _: &mut Self,
        _: &ExtImageCaptureSourceV1,
        _: wayland_protocols::ext::image_capture_source::v1::client::ext_image_capture_source_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtImageCopyCaptureManagerV1, ()> for ToplevelCaptureState {
    fn event(
        _: &mut Self,
        _: &ExtImageCopyCaptureManagerV1,
        _: ext_image_copy_capture_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, ()> for ToplevelCaptureState {
    fn event(
        state: &mut Self,
        _: &ExtImageCopyCaptureSessionV1,
        event: ext_image_copy_capture_session_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_image_copy_capture_session_v1::Event::BufferSize { width, height } => {
                state.buffer_width = width;
                state.buffer_height = height;
            }
            ext_image_copy_capture_session_v1::Event::ShmFormat {
                format: WEnum::Value(fmt),
            } => {
                if state.shm_format.is_none() && is_supported_shm_format(fmt) {
                    state.shm_format = Some(fmt);
                }
            }
            ext_image_copy_capture_session_v1::Event::Done => {
                state.session_done = true;
            }
            ext_image_copy_capture_session_v1::Event::Stopped => {
                state.session_stopped = true;
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, ()> for ToplevelCaptureState {
    fn event(
        state: &mut Self,
        _: &ExtImageCopyCaptureFrameV1,
        event: ext_image_copy_capture_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_image_copy_capture_frame_v1::Event::Ready => {
                state.frame_ready = true;
            }
            ext_image_copy_capture_frame_v1::Event::Failed { reason } => {
                state.frame_failed = Some(format!("{reason:?}"));
            }
            _ => {}
        }
    }
}
