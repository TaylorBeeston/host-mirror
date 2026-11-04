mod dmabuf;
mod stardust_keys;

use anyhow::{anyhow, Context, Result};
use dmabuf::{DmabufPresenter, OwnedDmabuf};
use input_linux::{
    AbsoluteAxis, AbsoluteInfo, AbsoluteInfoSetup, EventKind, InputId, Key, RelativeAxis,
    UInputHandle,
};
use libc::{input_event, timeval, MAP_PRIVATE, PROT_READ};
use softbuffer::{Context as SoftbufferContext, Surface as SoftbufferSurface};
use std::fs::File;
use std::num::NonZeroU32;
use std::ptr;
use std::rc::Rc;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, Event, MouseButton, WindowEvent};
use winit::event_loop::{ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::platform::wayland::WindowAttributesExtWayland;
use winit::window::Window;
use wlx_capture::frame::{
    FourCC, FramePlane, WlxFrame, DRM_FORMAT_ABGR8888, DRM_FORMAT_ARGB8888, DRM_FORMAT_XBGR8888,
    DRM_FORMAT_XRGB8888,
};
use wlx_capture::pipewire::{pipewire_select_screen, PipewireCapture};
use wlx_capture::WlxCapture;

const UINPUT_EXTENT: f32 = 32768.0;
const EV_SYN: u16 = 0x0;
const EV_KEY: u16 = 0x1;
const EV_REL: u16 = 0x2;
const EV_ABS: u16 = 0x3;

/// What the PipeWire thread hands the main thread.
enum Captured {
    Cpu(OwnedFrame),
    Dmabuf(OwnedDmabuf),
}

#[derive(Clone)]
struct OwnedFrame {
    width: u32,
    height: u32,
    pixels: Vec<u32>,
}

impl Drop for HostInput {
    fn drop(&mut self) {
        self.release_all();
    }
}

struct HostInput {
    keyboard: UInputHandle<File>,
    held_keys: std::cell::RefCell<std::collections::BTreeSet<u16>>,
    /// Last key event time; a held key with no key traffic past `key_hold_max`
    /// is force-released (Stardust can drop key-ups, e.g. Super).
    last_key: std::cell::Cell<std::time::Instant>,
    key_hold_max: std::time::Duration,
    /// Stardust has sent a nonzero modifier mask at least once (masks are trustworthy).
    mask_seen: std::cell::Cell<bool>,
    mouse: UInputHandle<File>,
    origin: (f32, f32),
    extent: (f32, f32),
}

impl HostInput {
    fn new(origin: (i32, i32), extent: (i32, i32)) -> Result<Self> {
        let keyboard_file = File::create("/dev/uinput").context("open /dev/uinput for keyboard")?;
        let mouse_file = File::create("/dev/uinput").context("open /dev/uinput for mouse")?;

        let keyboard = UInputHandle::new(keyboard_file);
        let mouse = UInputHandle::new(mouse_file);

        keyboard
            .set_evbit(EventKind::Key)
            .context("keyboard EV_KEY")?;
        for code in supported_key_codes() {
            let key: Key = unsafe { std::mem::transmute(*code) };
            keyboard
                .set_keybit(key)
                .with_context(|| format!("keyboard keybit {code}"))?;
        }
        keyboard
            .create(
                &InputId {
                    bustype: 0x03,
                    vendor: 0x4711,
                    product: 0x0831,
                    version: 1,
                },
                b"Stardust Host Mirror Keyboard\0",
                0,
                &[],
            )
            .context("create uinput keyboard")?;

        mouse
            .set_evbit(EventKind::Absolute)
            .context("mouse EV_ABS")?;
        mouse
            .set_evbit(EventKind::Relative)
            .context("mouse EV_REL")?;
        mouse.set_evbit(EventKind::Key).context("mouse EV_KEY")?;
        mouse.set_absbit(AbsoluteAxis::X).context("mouse ABS_X")?;
        mouse.set_absbit(AbsoluteAxis::Y).context("mouse ABS_Y")?;
        mouse
            .set_relbit(RelativeAxis::WheelHiRes)
            .context("mouse REL_WHEEL_HI_RES")?;
        mouse
            .set_relbit(RelativeAxis::HorizontalWheelHiRes)
            .context("mouse REL_HWHEEL_HI_RES")?;
        for code in [0x110u16, 0x111u16, 0x112u16] {
            let key: Key = unsafe { std::mem::transmute(code) };
            mouse
                .set_keybit(key)
                .with_context(|| format!("mouse keybit {code}"))?;
        }
        mouse
            .create(
                &InputId {
                    bustype: 0x03,
                    vendor: 0x4711,
                    product: 0x0832,
                    version: 1,
                },
                b"Stardust Host Mirror Mouse\0",
                0,
                &[
                    AbsoluteInfoSetup {
                        axis: AbsoluteAxis::X,
                        info: AbsoluteInfo {
                            value: 0,
                            minimum: 0,
                            maximum: UINPUT_EXTENT as i32,
                            fuzz: 0,
                            flat: 0,
                            resolution: 10,
                        },
                    },
                    AbsoluteInfoSetup {
                        axis: AbsoluteAxis::Y,
                        info: AbsoluteInfo {
                            value: 0,
                            minimum: 0,
                            maximum: UINPUT_EXTENT as i32,
                            fuzz: 0,
                            flat: 0,
                            resolution: 10,
                        },
                    },
                ],
            )
            .context("create uinput mouse")?;

        Ok(Self {
            keyboard,
            held_keys: Default::default(),
            last_key: std::cell::Cell::new(std::time::Instant::now()),
            mask_seen: std::cell::Cell::new(false),
            key_hold_max: std::time::Duration::from_millis(
                std::env::var("HOST_MIRROR_KEY_HOLD_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(1500),
            ),
            mouse,
            origin: (origin.0 as f32, origin.1 as f32),
            extent: (extent.0.max(1) as f32, extent.1.max(1) as f32),
        })
    }

    fn move_mouse(&self, x: f32, y: f32) {
        let px = ((x - self.origin.0) / self.extent.0).clamp(0.0, 1.0) * UINPUT_EXTENT;
        let py = ((y - self.origin.1) / self.extent.1).clamp(0.0, 1.0) * UINPUT_EXTENT;
        let time = get_time();
        let events = [
            new_event(time, EV_ABS, AbsoluteAxis::X as u16, px as i32),
            new_event(time, EV_ABS, AbsoluteAxis::Y as u16, py as i32),
            new_event(time, EV_SYN, 0, 0),
        ];
        if let Err(err) = self.mouse.write(&events) {
            log::warn!("mouse move failed: {err}");
        }
    }

    fn mouse_button(&self, button: MouseButton, pressed: bool) {
        let code = match button {
            MouseButton::Left => 0x110,
            MouseButton::Right => 0x111,
            MouseButton::Middle => 0x112,
            _ => return,
        };
        let time = get_time();
        let events = [
            new_event(time, EV_KEY, code, i32::from(pressed)),
            new_event(time, EV_SYN, 0, 0),
        ];
        if let Err(err) = self.mouse.write(&events) {
            log::warn!("mouse button failed: {err}");
        }
    }

    fn scroll(&self, dx: f32, dy: f32) {
        let time = get_time();
        let events = [
            new_event(
                time,
                EV_REL,
                RelativeAxis::HorizontalWheelHiRes as u16,
                (dx * 64.0) as i32,
            ),
            new_event(
                time,
                EV_REL,
                RelativeAxis::WheelHiRes as u16,
                (dy * 64.0) as i32,
            ),
            new_event(time, EV_SYN, 0, 0),
        ];
        if let Err(err) = self.mouse.write(&events) {
            log::warn!("mouse scroll failed: {err}");
        }
    }

    fn key(&self, keycode: u16, pressed: bool) {
        {
            let mut held = self.held_keys.borrow_mut();
            if pressed { held.insert(keycode); } else { held.remove(&keycode); }
        }
        self.last_key.set(std::time::Instant::now());
        if matches!(keycode, 29 | 42 | 54 | 56 | 97 | 100 | 125 | 126) {
            log::info!("modifier {keycode} {}", if pressed { "down" } else { "up" });
        }
        let time = get_time();
        let events = [
            new_event(time, EV_KEY, keycode, i32::from(pressed)),
            new_event(time, EV_SYN, 0, 0),
        ];
        if let Err(err) = self.keyboard.write(&events) {
            log::warn!("keyboard write failed: {err}");
        }
    }

    /// Release every key we pressed (focus loss / close / drop) so the host
    /// never sees a modifier stuck down when the key-up went elsewhere.
    fn release_all(&self) {
        let held: Vec<u16> = std::mem::take(&mut *self.held_keys.borrow_mut()).into_iter().collect();
        if held.is_empty() { return; }
        log::info!("releasing {} held key(s): {:?}", held.len(), held);
        let time = get_time();
        let mut events: Vec<_> = held.iter().map(|&k| new_event(time, EV_KEY, k, 0)).collect();
        events.push(new_event(time, EV_SYN, 0, 0));
        if let Err(err) = self.keyboard.write(&events) {
            log::warn!("keyboard release_all failed: {err}");
        }
    }

    /// Watchdog: release held keys when no key event arrived for `key_hold_max`.
    fn release_stale(&self) {
        if self.held_keys.borrow().is_empty() { return; }
        if self.last_key.get().elapsed() >= self.key_hold_max {
            log::warn!("no key-up within {:?}; watchdog releasing", self.key_hold_max);
            self.release_all();
        }
    }

    /// A key from the Stardust key sink: inject it, then drop any held modifier that
    /// Stardust's own xkb mask says is up (only once masks have proven populated).
    fn stardust_key(&self, k: stardust_keys::StardustKey) {
        if stardust_keys::modifier_bit(k.keycode).is_some() {
            log::info!("[stardust] modifier {} {} mask={:#x}", k.keycode, if k.pressed { "down" } else { "up" }, k.depressed);
        }
        self.key(k.keycode, k.pressed);
        if k.depressed != 0 {
            self.mask_seen.set(true);
        }
        if !self.mask_seen.get() {
            return;
        }
        let stale = stardust_keys::stale_modifiers(self.held_keys.borrow().iter().copied(), k.depressed, k.keycode);
        for m in stale {
            log::warn!("[stardust] mask {:#x} says modifier {m} is up; releasing", k.depressed);
            self.key(m, false);
        }
    }

    fn keyboard_input(&self, physical_key: &PhysicalKey, pressed: bool) {
        let Some(keycode) = map_keycode(physical_key) else {
            return;
        };
        self.key(keycode, pressed);
    }
}

const HELP: &str = "host-mirror: mirror one host monitor (portal ScreenCast) into a Wayland window

USAGE: host-mirror [--label NAME] [--token-file PATH] [--size WxH] [--no-input]

  --label NAME       Window title \"Stardust Host Mirror NAME\", app_id \"host-mirror-<name lowercase>\".
                     Env: HOST_MIRROR_LABEL. The Flatland panel shell matches eDP-1 / Meta-0.
  --token-file PATH  Portal restore token file (pick the monitor once, reuse after).
                     Env: HOST_MIRROR_TOKEN_FILE. Default with a label:
                     $XDG_STATE_HOME/host-mirror/<label>.token. No label: no token.
  --size WxH         Window size in px. Env: HOST_MIRROR_SIZE. Default: captured monitor size.
  --no-input         Do not create uinput devices. Env: HOST_MIRROR_NO_INPUT=1.
  Window close requests are ignored unless HOST_MIRROR_CLOSE_EXITS=1.
  -h, --help         This help.

The monitor is chosen in the GNOME portal dialog the first time (or when the token is stale).
Set WAYLAND_DISPLAY to Stardust's socket so the window lands in Stardust.";

struct Opts {
    label: Option<String>,
    token_file: Option<std::path::PathBuf>,
    size: Option<(u32, u32)>,
    no_input: bool,
}

fn parse_size(s: &str) -> Result<(u32, u32)> {
    let (w, h) = s.split_once(['x', 'X']).context("size must be WxH")?;
    Ok((w.trim().parse()?, h.trim().parse()?))
}

fn parse_opts() -> Result<Opts> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let mut label = env("HOST_MIRROR_LABEL");
    let mut token_file = env("HOST_MIRROR_TOKEN_FILE").map(Into::into);
    let mut size = env("HOST_MIRROR_SIZE").map(|s| parse_size(&s)).transpose()?;
    let mut no_input = env("HOST_MIRROR_NO_INPUT").is_some_and(|v| v != "0");
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut val = || args.next().with_context(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{HELP}");
                std::process::exit(0);
            }
            "--label" => label = Some(val()?),
            "--token-file" => token_file = Some(val()?.into()),
            "--size" => size = Some(parse_size(&val()?)?),
            "--no-input" => no_input = true,
            other => return Err(anyhow!("unknown argument {other:?} (try --help)")),
        }
    }
    if token_file.is_none() {
        if let Some(label) = &label {
            let state = env("XDG_STATE_HOME")
                .map(std::path::PathBuf::from)
                .or_else(|| env("HOME").map(|h| std::path::Path::new(&h).join(".local/state")));
            token_file = state.map(|d| d.join("host-mirror").join(format!("{label}.token")));
        }
    }
    Ok(Opts { label, token_file, size, no_input })
}

fn main() -> Result<()> {
    env_logger::init();
    {
        let args: Vec<String> = std::env::args().collect();
        if args.get(1).map(String::as_str) == Some("--probe-dmabuf") {
            return dmabuf::probe(args.get(2).map(String::as_str));
        }
    }
    let opts = parse_opts()?;
    let saved_token = opts
        .token_file
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());
    let selection = async_std::task::block_on(pipewire_select_screen(
        saved_token.as_deref(),
        true,
        true,
        true,
        false,
    ))
    .context("select PipeWire screencast source")?;
    if let (Some(path), Some(token)) = (&opts.token_file, &selection.restore_token) {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match std::fs::write(path, token) {
            Ok(()) => {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
                log::info!("saved portal restore token to {}", path.display());
            }
            Err(err) => log::warn!("could not save restore token {}: {err}", path.display()),
        }
    }
    let stream = selection
        .streams
        .first()
        .context("no monitor stream returned by portal")?;
    let desktop_origin = stream.position.unwrap_or((0, 0));
    let desktop_size = stream.size.unwrap_or((1920, 1080));
    log::info!(
        "selected stream node={} origin={:?} size={:?}",
        stream.node_id,
        desktop_origin,
        desktop_size
    );

    let node_id = stream.node_id;

    let host_input = match (!opts.no_input)
        .then(|| {
            // The uinput mouse is absolute and unmapped, so mutter scales 0..1 over the WHOLE
            // desktop (all monitors), not this stream's monitor. Normalise against the full
            // layout box or every mirror but a lone monitor sends the cursor to the wrong screen.
            // HOST_MIRROR_DESKTOP="x,y,w,h" (set by stardust-dual-mirror-flat from Mutter).
            let layout = std::env::var("HOST_MIRROR_DESKTOP").ok().and_then(|v| {
                let n: Vec<i32> = v.split(',').map(|p| p.trim().parse().ok()).collect::<Option<_>>()?;
                (n.len() == 4 && n[2] > 0 && n[3] > 0).then(|| ((n[0], n[1]), (n[2], n[3])))
            });
            let (o, e) = layout.unwrap_or_else(|| {
                log::warn!("HOST_MIRROR_DESKTOP unset: mouse maps to this monitor only (wrong on multi-monitor)");
                (desktop_origin, desktop_size)
            });
            log::info!("pointer layout origin={o:?} extent={e:?}");
            HostInput::new(o, e)
        })
        .transpose()
    {
        Ok(host_input) => host_input,
        Err(err) => {
            log::warn!("host input unavailable: {err:#}");
            None
        }
    };

    let (win_w, win_h) = opts.size.unwrap_or((
        desktop_size.0.max(1) as u32,
        desktop_size.1.max(1) as u32,
    ));
    let title = match &opts.label {
        Some(label) => format!("Stardust Host Mirror {label}"),
        None => "Stardust Host Mirror".to_string(),
    };
    let app_id = match &opts.label {
        Some(label) => format!("host-mirror-{}", label.to_lowercase()),
        None => "host-mirror".to_string(),
    };
    log::info!("window {title:?} app_id={app_id:?} size={win_w}x{win_h}");
    // Option 2 keyboard: keys come from Stardust (Flatland KeyboardHandler) via a datagram
    // sink; winit KeyboardInput is fallback-only until the first Stardust key arrives.
    let stardust_keys = host_input.as_ref().and_then(|_| stardust_keys::StardustKeys::bind(&app_id));
    let mut stardust_key_seen = false;
    let mut winit_key_ignored_logged = false;
    let event_loop = EventLoop::new().context("create event loop")?;
    let window = Rc::new(
        event_loop
            .create_window(
                Window::default_attributes()
                    .with_title(title)
                    .with_name(app_id.clone(), app_id)
                    .with_inner_size(PhysicalSize::new(win_w, win_h)),
            )
            .context("create mirror window")?,
    );
    let context = SoftbufferContext::new(window.clone())
        .map_err(|err| anyhow!("create softbuffer context: {err:?}"))?;
    let mut surface = SoftbufferSurface::new(&context, window.clone())
        .map_err(|err| anyhow!("create softbuffer surface: {err:?}"))?;
    // DMA-BUF present path (default on; HOST_MIRROR_DMABUF=0 forces the shm/softbuffer path).
    let want_dmabuf = std::env::var("HOST_MIRROR_DMABUF").map_or(true, |v| v != "0");
    let mut presenter = if want_dmabuf {
        match DmabufPresenter::new(&window) {
            Ok(p) => Some(p),
            Err(err) => {
                log::warn!("dmabuf present path unavailable, using shm: {err:#}");
                None
            }
        }
    } else {
        None
    };
    let dmabuf_formats = presenter
        .as_ref()
        .map(|p| {
            let formats = p.capture_formats(&dmabuf::default_order());
            log::info!(
                "compositor advertises {} dmabuf (format, modifier) pairs; offering PipeWire: {}",
                p.advertised().len(),
                formats
                    .iter()
                    .map(|f| format!("{}{:x?}", f.fourcc, f.modifiers))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
            formats
        })
        .unwrap_or_default();
    if presenter.is_some() && dmabuf_formats.is_empty() {
        log::warn!("no importable 8-bit RGB dmabuf formats advertised; using shm");
        presenter = None;
    }
    let start_capture = move |formats: &[wlx_capture::frame::DrmFormat]| {
        let mut capture = PipewireCapture::<Captured>::new("stardust-host-mirror".into(), node_id);
        capture.init(formats, (), receive_frame);
        capture
    };
    let mut capture = start_capture(&dmabuf_formats);
    let mut current_frame: Option<OwnedFrame> = None;
    // Size of the last frame of either kind (input mapping + buffer size).
    let mut current_size: Option<(u32, u32)> = None;
    // True once a dmabuf frame was attached: softbuffer must not present over it.
    let mut dmabuf_active = false;
    // winit has acked the first xdg configure once it emits Resized/RedrawRequested;
    // attaching a buffer before that is a protocol error.
    let mut configured = false;
    let mut pending_dmabuf: Option<OwnedDmabuf> = None;
    let fixed_size = opts.size.is_some();
    // Buffer size stays at the capture (or --size) size even when the compositor configures
    // the toplevel smaller: Stardust panels then keep their px/density size, and resizes
    // don't churn buffers. HOST_MIRROR_FOLLOW_WINDOW=1 restores follow-the-window.
    let lock_size = std::env::var_os("HOST_MIRROR_FOLLOW_WINDOW").is_none();
    // Pin geometry (min == max) so the panel shell can't configure a squashed size.
    let pin_window = window.clone();
    let pin_size = move |w: u32, h: u32| {
        if lock_size {
            pin_window.set_min_inner_size(Some(PhysicalSize::new(w, h)));
            pin_window.set_max_inner_size(Some(PhysicalSize::new(w, h)));
        }
    };
    pin_size(win_w, win_h);
    let dump_dir = std::env::var("HOST_MIRROR_DUMP_DIR").ok().filter(|v| !v.is_empty());
    let label_s = opts.label.clone().unwrap_or_else(|| "mirror".into());
    let mut stats = (0u64, 0u64, std::time::Instant::now()); // frames, presents, last log
    let mut dumped = false;

    event_loop.run(move |event, elwt| {
        // Poll the capture ~250 Hz instead of busy-spinning a core.
        elwt.set_control_flow(ControlFlow::WaitUntil(
            std::time::Instant::now() + std::time::Duration::from_millis(4),
        ));
        match event {
            Event::AboutToWait => {
                if let (Some(host_input), Some(sk)) = (&host_input, &stardust_keys) {
                    for k in sk.drain() {
                        if !stardust_key_seen {
                            log::info!("keyboard via Stardust key sink (winit keys now ignored)");
                            stardust_key_seen = true;
                        }
                        host_input.stardust_key(k);
                    }
                }
                if let Some(host_input) = &host_input {
                    host_input.release_stale();
                }
                let received =
                    <PipewireCapture<Captured> as WlxCapture<(), Captured>>::receive(&mut capture);
                let frame_size = match &received {
                    Some(Captured::Cpu(f)) => Some((f.width, f.height)),
                    Some(Captured::Dmabuf(f)) => Some((f.width, f.height)),
                    None => None,
                };
                if let Some((fw, fh)) = frame_size {
                    // Match the window to the real capture size (portal `size` can be
                    // logical px on a scaled monitor) unless --size was given.
                    if !fixed_size && current_size != Some((fw, fh)) {
                        log::info!("capture frame {fw}x{fh}; resizing window");
                        pin_size(fw, fh);
                        let _ = window.request_inner_size(PhysicalSize::new(fw, fh));
                    }
                    current_size = Some((fw, fh));
                }
                let (received_cpu, received_dmabuf) = match received {
                    Some(Captured::Cpu(f)) => (Some(f), None),
                    Some(Captured::Dmabuf(f)) => (None, Some(f)),
                    None => (None, None),
                };
                if let Some(f) = received_dmabuf {
                    stats.0 += 1;
                    pending_dmabuf = Some(f);
                }
                if configured {
                    if let Some(f) = pending_dmabuf.take() {
                        let result = match presenter.as_mut() {
                            Some(p) => p.present(&f),
                            None => Err(anyhow!("dmabuf frame but no presenter")),
                        };
                        match result {
                            Ok(()) => {
                                if !dmabuf_active {
                                    log::info!("presenting via dmabuf (zero CPU copy) {}x{} fourcc={:#x} modifier={:#x}", f.width, f.height, f.fourcc, f.modifier);
                                }
                                dmabuf_active = true;
                                stats.1 += 1;
                            }
                            Err(err) => {
                                log::warn!("dmabuf present failed, restarting capture on shm: {err:#}");
                                presenter = None;
                                dmabuf_active = false;
                                drop(f);
                                capture = start_capture(&[]);
                            }
                        }
                    }
                }
                if stats.2.elapsed().as_secs() >= 5 && dmabuf_active {
                    let win = window.inner_size();
                    log::info!(
                        "stats: frames={} presents={} {} window={}x{}",
                        stats.0, stats.1,
                        presenter.as_ref().map(|p| p.stats()).unwrap_or_default(),
                        win.width, win.height
                    );
                    stats.2 = std::time::Instant::now();
                }
                if let Some(frame) = received_cpu {
                    if dmabuf_active {
                        // Renegotiated to shm mid-stream: hand the surface back to softbuffer.
                        log::info!("capture switched to shm frames");
                        dmabuf_active = false;
                    }
                    stats.0 += 1;
                    if !dumped {
                        if let Some(dir) = &dump_dir {
                            dumped = true;
                            let path = format!("{dir}/host-mirror-{label_s}.ppm");
                            let mut out = format!("P6\n{} {}\n255\n", frame.width, frame.height).into_bytes();
                            for p in &frame.pixels {
                                out.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, *p as u8]);
                            }
                            match std::fs::write(&path, out) {
                                Ok(()) => log::info!("dumped first frame to {path}"),
                                Err(e) => log::warn!("dump {path}: {e}"),
                            }
                        }
                    }
                    if stats.2.elapsed().as_secs() >= 5 {
                        let n = frame.pixels.len().max(1);
                        let step = (n / 4096).max(1);
                        let (mut sum, mut cnt, mut nonblack) = (0u64, 0u64, 0u64);
                        for p in frame.pixels.iter().step_by(step) {
                            let l = ((p >> 16) & 0xff) + ((p >> 8) & 0xff) + (p & 0xff);
                            sum += l as u64; cnt += 1; if l > 24 { nonblack += 1; }
                        }
                        let win = window.inner_size();
                        log::info!(
                            "stats: frames={} presents={} mean_rgb_sum={} nonblack={}/{} px0={:08x} window={}x{}",
                            stats.0, stats.1, sum / cnt.max(1), nonblack, cnt, frame.pixels[0], win.width, win.height
                        );
                        stats.2 = std::time::Instant::now();
                    }
                    current_frame = Some(frame);
                    window.request_redraw();
                }
            }
            Event::WindowEvent { window_id, event } if window_id == window.id() => match event {
                WindowEvent::CloseRequested => {
                    if let Some(host_input) = &host_input {
                        host_input.release_all();
                    }
                    // Stardust panel close buttons/derez can fire by accident (e.g. black-hole
                    // collapses panels onto the hand). Mirrors ignore close unless opted in.
                    if std::env::var("HOST_MIRROR_CLOSE_EXITS").is_ok_and(|v| v == "1") {
                        log::info!("close requested; exiting (HOST_MIRROR_CLOSE_EXITS=1)");
                        elwt.exit();
                    } else {
                        log::warn!("close requested; ignored (stop with pkill -f 'host-mirror --label', or set HOST_MIRROR_CLOSE_EXITS=1)");
                    }
                }
                WindowEvent::Resized(size) => {
                    configured = true;
                    if lock_size {
                        // Keep the window geometry equal to the buffer: the panel shell reads
                        // geometry as the toplevel size, so a configure from it (e.g. Flatland's
                        // initial resize) would otherwise squash the panel's aspect.
                        let want = buffer_size(&window, current_size, lock_size, fixed_size, (win_w, win_h));
                        if want != size && want.width > 0 && want.height > 0 {
                            log::info!("configured {}x{}; re-requesting {}x{}", size.width, size.height, want.width, want.height);
                            let _ = window.request_inner_size(want);
                        }
                        window.request_redraw();
                    } else if let (Some(width), Some(height)) =
                        (NonZeroU32::new(size.width), NonZeroU32::new(size.height))
                    {
                        if let Err(err) = surface.resize(width, height) {
                            log::warn!("resize failed: {err}");
                        }
                    }
                    window.request_redraw();
                }
                WindowEvent::CursorMoved { position, .. } => {
                    if let Some(host_input) = &host_input {
                        let size = buffer_size(&window, current_size, lock_size, fixed_size, (win_w, win_h));
                        if size.width > 0 && size.height > 0 {
                            let x = desktop_origin.0 as f32
                                + (position.x as f32 / size.width as f32) * desktop_size.0 as f32;
                            let y = desktop_origin.1 as f32
                                + (position.y as f32 / size.height as f32) * desktop_size.1 as f32;
                            host_input.move_mouse(x, y);
                        }
                    }
                }
                WindowEvent::MouseInput { state, button, .. } => {
                    if let Some(host_input) = &host_input {
                        host_input.mouse_button(button, state == ElementState::Pressed);
                    }
                }
                WindowEvent::MouseWheel { delta, .. } => {
                    if let Some(host_input) = &host_input {
                        match delta {
                            winit::event::MouseScrollDelta::LineDelta(dx, dy) => {
                                host_input.scroll(dx, dy)
                            }
                            winit::event::MouseScrollDelta::PixelDelta(pos) => {
                                host_input.scroll(pos.x as f32 / 16.0, pos.y as f32 / 16.0)
                            }
                        }
                    }
                }
                WindowEvent::Focused(false) | WindowEvent::CursorLeft { .. } => {
                    if let Some(host_input) = &host_input {
                        log::info!("focus/pointer left; releasing keys");
                        host_input.release_all();
                    }
                }
                WindowEvent::KeyboardInput { event, .. } => {
                    if stardust_key_seen {
                        if !winit_key_ignored_logged {
                            log::info!("ignoring winit KeyboardInput (Stardust key sink active)");
                            winit_key_ignored_logged = true;
                        }
                    } else if let Some(host_input) = &host_input {
                        host_input.keyboard_input(
                            &event.physical_key,
                            event.state == ElementState::Pressed,
                        );
                    }
                }
                WindowEvent::RedrawRequested => {
                    configured = true;
                    if dmabuf_active {
                        return;
                    }
                    let size = buffer_size(&window, current_size, lock_size, fixed_size, (win_w, win_h));
                    let (Some(width), Some(height)) =
                        (NonZeroU32::new(size.width), NonZeroU32::new(size.height))
                    else {
                        return;
                    };
                    if let Err(err) = surface.resize(width, height) {
                        log::warn!("resize before redraw failed: {err}");
                        return;
                    }
                    if let Ok(mut buffer) = surface.buffer_mut() {
                        render_frame(
                            current_frame.as_ref(),
                            width.get(),
                            height.get(),
                            &mut buffer,
                        );
                        stats.1 += 1;
                        if let Err(err) = buffer.present() {
                            log::warn!("present failed: {err}");
                        }
                    } else {
                        log::warn!("buffer acquisition failed");
                    }
                }
                _ => {}
            },
            _ => {}
        }
    })?;
    Ok(())
}

fn buffer_size(
    window: &winit::window::Window,
    frame: Option<(u32, u32)>,
    lock_size: bool,
    fixed_size: bool,
    fixed: (u32, u32),
) -> PhysicalSize<u32> {
    match (lock_size, fixed_size, frame) {
        (false, _, _) => window.inner_size(),
        (true, true, _) => PhysicalSize::new(fixed.0, fixed.1),
        (true, false, Some((w, h))) => PhysicalSize::new(w, h),
        (true, false, None) => window.inner_size(),
    }
}

fn receive_frame(_: &(), frame: WlxFrame) -> Option<Captured> {
    if let WlxFrame::Dmabuf(frame) = &frame {
        return match OwnedDmabuf::from_frame(frame) {
            Ok(f) => Some(Captured::Dmabuf(f)),
            Err(err) => {
                log::warn!("dropping DMA-BUF frame: {err:#}");
                None
            }
        };
    }
    receive_cpu_frame(frame).map(Captured::Cpu)
}

fn receive_cpu_frame(frame: WlxFrame) -> Option<OwnedFrame> {
    match frame {
        WlxFrame::MemFd(frame) => {
            let bytes = unsafe { copy_fd_bytes(frame.plane, frame.format.height).ok()? };
            bytes_to_owned_frame(
                frame.format.width,
                frame.format.height,
                frame.format.fourcc,
                &bytes,
            )
        }
        WlxFrame::MemPtr(frame) => {
            let bytes = unsafe { std::slice::from_raw_parts(frame.ptr as *const u8, frame.size) };
            bytes_to_owned_frame(
                frame.format.width,
                frame.format.height,
                frame.format.fourcc,
                bytes,
            )
        }
        WlxFrame::Dmabuf(_) => None, // handled in receive_frame
    }
}

unsafe fn copy_fd_bytes(plane: FramePlane, height: u32) -> Result<Vec<u8>> {
    let fd = plane.fd.context("missing memfd plane fd")?;
    let len = plane.offset as usize + plane.stride.max(0) as usize * height as usize;
    let ptr = libc::mmap(ptr::null_mut(), len, PROT_READ, MAP_PRIVATE, fd, 0);
    if ptr == libc::MAP_FAILED {
        return Err(anyhow!("mmap failed for capture frame"));
    }
    let data = std::slice::from_raw_parts(
        (ptr as *const u8).add(plane.offset as usize),
        len - plane.offset as usize,
    )
    .to_vec();
    if libc::munmap(ptr, len) != 0 {
        log::warn!("munmap failed for capture frame");
    }
    Ok(data)
}

fn bytes_to_owned_frame(
    width: u32,
    height: u32,
    fourcc: FourCC,
    bytes: &[u8],
) -> Option<OwnedFrame> {
    let stride = bytes.len() / height.max(1) as usize;
    if stride < width as usize * 4 {
        return None;
    }
    let mut pixels = vec![0u32; width as usize * height as usize];
    for y in 0..height as usize {
        let row = &bytes[y * stride..y * stride + width as usize * 4];
        for x in 0..width as usize {
            let i = x * 4;
            let (r, g, b) = match fourcc {
                FourCC {
                    value: DRM_FORMAT_ARGB8888,
                }
                | FourCC {
                    value: DRM_FORMAT_XRGB8888,
                } => (row[i + 2], row[i + 1], row[i]),
                FourCC {
                    value: DRM_FORMAT_ABGR8888,
                }
                | FourCC {
                    value: DRM_FORMAT_XBGR8888,
                } => (row[i], row[i + 1], row[i + 2]),
                _ => return None,
            };
            // Stardust uploads wl_shm Xrgb8888 as B8G8R8A8 and keeps the X byte as alpha,
            // so X must be 0xFF or the panel renders black.
            pixels[y * width as usize + x] =
                0xFF00_0000 | (b as u32) | ((g as u32) << 8) | ((r as u32) << 16);
        }
    }
    Some(OwnedFrame {
        width,
        height,
        pixels,
    })
}

fn render_frame(frame: Option<&OwnedFrame>, width: u32, height: u32, buffer: &mut [u32]) {
    if let Some(frame) = frame.filter(|f| f.width == width && f.height == height) {
        buffer.copy_from_slice(&frame.pixels);
    } else if let Some(frame) = frame {
        for y in 0..height as usize {
            let sy = y * frame.height as usize / height as usize;
            for x in 0..width as usize {
                let sx = x * frame.width as usize / width as usize;
                buffer[y * width as usize + x] = frame.pixels[sy * frame.width as usize + sx];
            }
        }
    } else {
        for (index, pixel) in buffer.iter_mut().enumerate() {
            let x = (index as u32) % width;
            let y = (index as u32) / width;
            let r = (x * 255 / width.max(1)) & 0xff;
            let g = (y * 255 / height.max(1)) & 0xff;
            let b = 32;
            *pixel = 0xFF00_0000 | b | (g << 8) | (r << 16);
        }
    }
}

fn supported_key_codes() -> &'static [u16] {
    &[
        1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25,
        26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48,
        49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63, 64, 65, 66, 67, 68, 69, 70, 71,
        72, 73, 74, 75, 76, 77, 78, 79, 80, 81, 82, 83, 87, 88, 96, 97, 98, 99, 100, 102, 103, 104,
        105, 106, 107, 108, 109, 110, 111, 113, 114, 115, 125, 126,
    ]
}

fn map_keycode(physical_key: &PhysicalKey) -> Option<u16> {
    let PhysicalKey::Code(code) = physical_key else {
        return None;
    };
    match code {
        KeyCode::Escape => Some(1),
        KeyCode::Digit1 => Some(2),
        KeyCode::Digit2 => Some(3),
        KeyCode::Digit3 => Some(4),
        KeyCode::Digit4 => Some(5),
        KeyCode::Digit5 => Some(6),
        KeyCode::Digit6 => Some(7),
        KeyCode::Digit7 => Some(8),
        KeyCode::Digit8 => Some(9),
        KeyCode::Digit9 => Some(10),
        KeyCode::Digit0 => Some(11),
        KeyCode::Minus => Some(12),
        KeyCode::Equal => Some(13),
        KeyCode::Backspace => Some(14),
        KeyCode::Tab => Some(15),
        KeyCode::KeyQ => Some(16),
        KeyCode::KeyW => Some(17),
        KeyCode::KeyE => Some(18),
        KeyCode::KeyR => Some(19),
        KeyCode::KeyT => Some(20),
        KeyCode::KeyY => Some(21),
        KeyCode::KeyU => Some(22),
        KeyCode::KeyI => Some(23),
        KeyCode::KeyO => Some(24),
        KeyCode::KeyP => Some(25),
        KeyCode::BracketLeft => Some(26),
        KeyCode::BracketRight => Some(27),
        KeyCode::Enter => Some(28),
        KeyCode::ControlLeft => Some(29),
        KeyCode::KeyA => Some(30),
        KeyCode::KeyS => Some(31),
        KeyCode::KeyD => Some(32),
        KeyCode::KeyF => Some(33),
        KeyCode::KeyG => Some(34),
        KeyCode::KeyH => Some(35),
        KeyCode::KeyJ => Some(36),
        KeyCode::KeyK => Some(37),
        KeyCode::KeyL => Some(38),
        KeyCode::Semicolon => Some(39),
        KeyCode::Quote => Some(40),
        KeyCode::Backquote => Some(41),
        KeyCode::ShiftLeft => Some(42),
        KeyCode::Backslash => Some(43),
        KeyCode::KeyZ => Some(44),
        KeyCode::KeyX => Some(45),
        KeyCode::KeyC => Some(46),
        KeyCode::KeyV => Some(47),
        KeyCode::KeyB => Some(48),
        KeyCode::KeyN => Some(49),
        KeyCode::KeyM => Some(50),
        KeyCode::Comma => Some(51),
        KeyCode::Period => Some(52),
        KeyCode::Slash => Some(53),
        KeyCode::ShiftRight => Some(54),
        KeyCode::NumpadMultiply => Some(55),
        KeyCode::AltLeft => Some(56),
        KeyCode::Space => Some(57),
        KeyCode::CapsLock => Some(58),
        KeyCode::F1 => Some(59),
        KeyCode::F2 => Some(60),
        KeyCode::F3 => Some(61),
        KeyCode::F4 => Some(62),
        KeyCode::F5 => Some(63),
        KeyCode::F6 => Some(64),
        KeyCode::F7 => Some(65),
        KeyCode::F8 => Some(66),
        KeyCode::F9 => Some(67),
        KeyCode::F10 => Some(68),
        KeyCode::NumLock => Some(69),
        KeyCode::ScrollLock => Some(70),
        KeyCode::Numpad7 => Some(71),
        KeyCode::Numpad8 => Some(72),
        KeyCode::Numpad9 => Some(73),
        KeyCode::NumpadSubtract => Some(74),
        KeyCode::Numpad4 => Some(75),
        KeyCode::Numpad5 => Some(76),
        KeyCode::Numpad6 => Some(77),
        KeyCode::NumpadAdd => Some(78),
        KeyCode::Numpad1 => Some(79),
        KeyCode::Numpad2 => Some(80),
        KeyCode::Numpad3 => Some(81),
        KeyCode::Numpad0 => Some(82),
        KeyCode::NumpadDecimal => Some(83),
        KeyCode::F11 => Some(87),
        KeyCode::F12 => Some(88),
        KeyCode::NumpadEnter => Some(96),
        KeyCode::ControlRight => Some(97),
        KeyCode::NumpadDivide => Some(98),
        KeyCode::PrintScreen => Some(99),
        KeyCode::AltRight => Some(100),
        KeyCode::Home => Some(102),
        KeyCode::ArrowUp => Some(103),
        KeyCode::PageUp => Some(104),
        KeyCode::ArrowLeft => Some(105),
        KeyCode::ArrowRight => Some(106),
        KeyCode::End => Some(107),
        KeyCode::ArrowDown => Some(108),
        KeyCode::PageDown => Some(109),
        KeyCode::Insert => Some(110),
        KeyCode::Delete => Some(111),
        KeyCode::AudioVolumeMute => Some(113),
        KeyCode::AudioVolumeDown => Some(114),
        KeyCode::AudioVolumeUp => Some(115),
        KeyCode::SuperLeft => Some(125),
        KeyCode::SuperRight => Some(126),
        _ => None,
    }
}

fn get_time() -> timeval {
    let mut time = timeval {
        tv_sec: 0,
        tv_usec: 0,
    };
    unsafe { libc::gettimeofday(&mut time, ptr::null_mut()) };
    time
}

const fn new_event(time: timeval, type_: u16, code: u16, value: i32) -> input_event {
    input_event {
        time,
        type_,
        code,
        value,
    }
}
