//! Zero-CPU-copy present path: PipeWire DMA-BUF frames -> zwp_linux_dmabuf_v1 wl_buffers
//! attached straight to winit's wl_surface (softbuffer stays the shm fallback).
//!
//! We piggyback on winit's wl_display (foreign display, like softbuffer does) with our own
//! event queue, so nothing about the winit/xdg-toplevel setup changes.
use anyhow::{anyhow, bail, Context, Result};
use std::collections::HashMap;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd, RawFd};
use wayland_client::backend::{Backend, ObjectId};
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_buffer::WlBuffer, wl_registry::WlRegistry, wl_surface::WlSurface};
use wayland_client::{event_created_child, Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1},
    zwp_linux_dmabuf_v1::{self, ZwpLinuxDmabufV1},
};
use winit::raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use wlx_capture::frame::{DmabufFrame, DrmFormat, DRM_FORMAT_ABGR8888, DRM_FORMAT_ARGB8888, DRM_FORMAT_XBGR8888, DRM_FORMAT_XRGB8888};

pub const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

/// A PipeWire DMA-BUF frame whose plane fds we own (dup'd inside the PipeWire callback,
/// because wlx-capture requeues the pw_buffer as soon as the callback returns).
pub struct OwnedDmabuf {
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
    pub modifier: u64,
    pub planes: Vec<OwnedPlane>,
    /// (st_dev, st_ino) of plane 0: identifies the underlying dma-buf across frames, so one
    /// wl_buffer per PipeWire pool buffer can be reused instead of importing every frame.
    pub ino: (u64, u64),
}

pub struct OwnedPlane {
    pub fd: OwnedFd,
    pub offset: u32,
    pub stride: u32,
}

impl OwnedDmabuf {
    /// Runs on the PipeWire thread inside wlx-capture's receive callback.
    pub fn from_frame(frame: &DmabufFrame) -> Result<Self> {
        let n = frame.num_planes.clamp(1, 4);
        let mut planes = Vec::with_capacity(n);
        for p in &frame.planes[..n] {
            let raw: RawFd = p.fd.context("dmabuf plane without fd")?;
            // SAFETY: fd is valid for the duration of the PipeWire process callback.
            let fd = unsafe { BorrowedFd::borrow_raw(raw) }
                .try_clone_to_owned()
                .context("dup dmabuf fd")?;
            planes.push(OwnedPlane { fd, offset: p.offset, stride: p.stride as u32 });
        }
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(std::os::fd::AsRawFd::as_raw_fd(&planes[0].fd), &mut st) } != 0 {
            bail!("fstat dmabuf fd failed");
        }
        Ok(Self {
            width: frame.format.width,
            height: frame.format.height,
            fourcc: frame.format.fourcc.value,
            modifier: frame.format.modifier,
            planes,
            ino: (st.st_dev as u64, st.st_ino as u64),
        })
    }
}

type Key = ((u64, u64), u32, u32, u32, u64);

#[derive(Default)]
struct State {
    modifiers: Vec<(u32, u64)>,
    formats: Vec<u32>,
    result: Option<std::result::Result<WlBuffer, ()>>,
    releases: u64,
}

pub struct DmabufPresenter {
    conn: Connection,
    queue: EventQueue<State>,
    qh: QueueHandle<State>,
    state: State,
    surface: WlSurface,
    dmabuf: ZwpLinuxDmabufV1,
    cache: HashMap<Key, WlBuffer>,
    cache_shape: Option<(u32, u32, u32, u64)>,
    pub imports: u64,
    pub presents: u64,
}

impl DmabufPresenter {
    pub fn new(window: &winit::window::Window) -> Result<Self> {
        let RawDisplayHandle::Wayland(dh) = window.display_handle().map_err(|e| anyhow!("{e}"))?.as_raw() else {
            bail!("not a wayland display");
        };
        let RawWindowHandle::Wayland(wh) = window.window_handle().map_err(|e| anyhow!("{e}"))?.as_raw() else {
            bail!("not a wayland window");
        };
        // SAFETY: winit owns the wl_display / wl_surface and outlives this presenter
        // (presenter is dropped inside the event loop closure together with the window Rc).
        let backend = unsafe { Backend::from_foreign_display(dh.display.as_ptr().cast()) };
        let conn = Connection::from_backend(backend);
        let surface_id = unsafe { ObjectId::from_ptr(WlSurface::interface(), wh.surface.as_ptr().cast()) }
            .map_err(|_| anyhow!("wrap winit wl_surface"))?;
        let surface = WlSurface::from_id(&conn, surface_id).map_err(|_| anyhow!("wl_surface proxy"))?;
        let (globals, mut queue) = registry_queue_init::<State>(&conn).context("registry init")?;
        let qh = queue.handle();
        // v3 on purpose: Stardust's wayland-service sends per-format `modifier` events only to
        // v3 binds (v4+ get the feedback table instead).
        let dmabuf: ZwpLinuxDmabufV1 = globals
            .bind(&qh, 3..=3, ())
            .context("compositor has no zwp_linux_dmabuf_v1 v3")?;
        let mut state = State::default();
        queue.roundtrip(&mut state).context("dmabuf roundtrip")?;
        Ok(Self {
            conn,
            queue,
            qh,
            state,
            surface,
            dmabuf,
            cache: HashMap::new(),
            cache_shape: None,
            imports: 0,
            presents: 0,
        })
    }

    pub fn advertised(&self) -> &[(u32, u64)] {
        &self.state.modifiers
    }

    /// DrmFormat list for wlx-capture: 8-bit RGB formats the compositor can import,
    /// ordered by `order` (default XR24, AR24, XB24, AB24; env HOST_MIRROR_DMABUF_FORMATS=AR24,XR24...), excluding MOD_INVALID.
    pub fn capture_formats(&self, order: &[u32]) -> Vec<DrmFormat> {
        order
            .iter()
            .filter_map(|&fourcc| {
                let mut modifiers: Vec<u64> = self
                    .state
                    .modifiers
                    .iter()
                    .filter(|(f, m)| *f == fourcc && *m != DRM_FORMAT_MOD_INVALID)
                    .map(|(_, m)| *m)
                    .collect();
                modifiers.dedup();
                (!modifiers.is_empty()).then(|| DrmFormat { fourcc: fourcc.into(), modifiers })
            })
            .collect()
    }

    fn import(&mut self, f: &OwnedDmabuf) -> Result<WlBuffer> {
        let params = self.dmabuf.create_params(&self.qh, ());
        for (i, p) in f.planes.iter().enumerate() {
            params.add(
                p.fd.as_fd(),
                i as u32,
                p.offset,
                p.stride,
                (f.modifier >> 32) as u32,
                (f.modifier & 0xffff_ffff) as u32,
            );
        }
        // Non-immed create: an import failure is a `failed` event, not a fatal protocol
        // error that would kill winit's whole connection.
        params.create(f.width as i32, f.height as i32, f.fourcc, zwp_linux_buffer_params_v1::Flags::empty());
        self.state.result = None;
        while self.state.result.is_none() {
            self.queue.blocking_dispatch(&mut self.state).context("dmabuf import dispatch")?;
        }
        params.destroy();
        self.imports += 1;
        match self.state.result.take() {
            Some(Ok(buf)) => Ok(buf),
            _ => Err(anyhow!(
                "compositor rejected dmabuf {}x{} fourcc={:#x} modifier={:#x} planes={}",
                f.width, f.height, f.fourcc, f.modifier, f.planes.len()
            )),
        }
    }

    /// Attach the frame's wl_buffer (importing it once per pool buffer) and commit.
    pub fn present(&mut self, f: &OwnedDmabuf) -> Result<()> {
        self.queue.dispatch_pending(&mut self.state).context("dispatch_pending")?;
        let shape = (f.width, f.height, f.fourcc, f.modifier);
        let mut stale = Vec::new();
        if self.cache_shape != Some(shape) || self.cache.len() > 32 {
            // Renegotiation (or a pool that never repeats): drop old buffers after the new commit.
            stale = self.cache.drain().map(|(_, b)| b).collect();
            self.cache_shape = Some(shape);
        }
        let key: Key = (f.ino, f.width, f.height, f.fourcc, f.modifier);
        let buffer = match self.cache.get(&key) {
            Some(b) => b.clone(),
            None => {
                let b = self.import(f)?;
                log::info!(
                    "imported dmabuf wl_buffer #{} ino={:?} {}x{} fourcc={:#x} modifier={:#x}",
                    self.cache.len() + 1, f.ino, f.width, f.height, f.fourcc, f.modifier
                );
                self.cache.insert(key, b.clone());
                b
            }
        };
        self.surface.attach(Some(&buffer), 0, 0);
        if self.surface.version() >= 4 {
            self.surface.damage_buffer(0, 0, f.width as i32, f.height as i32);
        } else {
            self.surface.damage(0, 0, i32::MAX, i32::MAX);
        }
        self.surface.commit();
        for b in stale {
            b.destroy();
        }
        self.conn.flush().context("flush")?;
        self.presents += 1;
        Ok(())
    }

    pub fn stats(&self) -> String {
        format!(
            "dmabuf imports={} cached={} presents={} releases={}",
            self.imports,
            self.cache.len(),
            self.presents,
            self.state.releases
        )
    }
}

pub fn default_order() -> Vec<u32> {
    let parse = |s: &str| -> Option<u32> {
        match s.trim().to_ascii_uppercase().as_str() {
            "AR24" | "ARGB8888" => Some(DRM_FORMAT_ARGB8888),
            "XR24" | "XRGB8888" => Some(DRM_FORMAT_XRGB8888),
            "AB24" | "ABGR8888" => Some(DRM_FORMAT_ABGR8888),
            "XB24" | "XBGR8888" => Some(DRM_FORMAT_XBGR8888),
            _ => None,
        }
    };
    match std::env::var("HOST_MIRROR_DMABUF_FORMATS") {
        Ok(v) if !v.trim().is_empty() => v.split(',').filter_map(parse).collect(),
        // XR24 first: mutter's native screencast format, and wayland-service treats it as opaque
        // (no risk of a garbage X byte becoming alpha -> see-through/black panel).
        _ => vec![DRM_FORMAT_XRGB8888, DRM_FORMAT_ARGB8888, DRM_FORMAT_XBGR8888, DRM_FORMAT_ABGR8888],
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(_: &mut Self, _: &WlRegistry, _: <WlRegistry as Proxy>::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ZwpLinuxDmabufV1, ()> for State {
    fn event(state: &mut Self, _: &ZwpLinuxDmabufV1, event: zwp_linux_dmabuf_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            zwp_linux_dmabuf_v1::Event::Format { format } => state.formats.push(format),
            zwp_linux_dmabuf_v1::Event::Modifier { format, modifier_hi, modifier_lo } => {
                state.modifiers.push((format, ((modifier_hi as u64) << 32) | modifier_lo as u64))
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwpLinuxBufferParamsV1, ()> for State {
    fn event(state: &mut Self, _: &ZwpLinuxBufferParamsV1, event: zwp_linux_buffer_params_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            zwp_linux_buffer_params_v1::Event::Created { buffer } => state.result = Some(Ok(buffer)),
            zwp_linux_buffer_params_v1::Event::Failed => state.result = Some(Err(())),
            _ => {}
        }
    }

    event_created_child!(State, ZwpLinuxBufferParamsV1, [
        zwp_linux_buffer_params_v1::EVT_CREATED_OPCODE => (WlBuffer, ()),
    ]);
}

impl Dispatch<WlBuffer, ()> for State {
    fn event(state: &mut Self, _: &WlBuffer, _: wayland_client::protocol::wl_buffer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        state.releases += 1;
    }
}

/// Standalone probe (no window): list what a compositor advertises over linux-dmabuf v3.
pub fn probe(display_name: Option<&str>) -> Result<()> {
    if let Some(d) = display_name {
        std::env::set_var("WAYLAND_DISPLAY", d);
    }
    let conn = Connection::connect_to_env().context("connect")?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn).context("registry")?;
    let qh = queue.handle();
    let adv = globals.contents().with_list(|l| {
        l.iter().find(|g| g.interface == "zwp_linux_dmabuf_v1").map(|g| g.version)
    });
    println!("zwp_linux_dmabuf_v1 advertised version: {adv:?}");
    let _dmabuf: ZwpLinuxDmabufV1 = globals.bind(&qh, 3..=3, ()).context("bind dmabuf v3")?;
    let mut state = State::default();
    queue.roundtrip(&mut state)?;
    let name = |f: u32| -> String { f.to_le_bytes().iter().map(|&b| b as char).collect() };
    let mut by_fmt: std::collections::BTreeMap<u32, Vec<u64>> = Default::default();
    for (f, m) in &state.modifiers {
        by_fmt.entry(*f).or_default().push(*m);
    }
    for (f, mods) in by_fmt {
        let mods: Vec<String> = mods.iter().map(|m| format!("{m:#018x}")).collect();
        println!("{} ({f:#x}): {}", name(f), mods.join(" "));
    }
    if !state.formats.is_empty() {
        println!("format events: {:?}", state.formats.iter().map(|f| name(*f)).collect::<Vec<_>>());
    }
    Ok(())
}
