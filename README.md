# host-mirror

Mirror a host (GNOME) monitor into [Stardust XR](https://stardustxr.org) as a regular Wayland window, with mouse/keyboard passthrough.

`host-mirror` asks the xdg-desktop-portal ScreenCast API for a monitor, receives the PipeWire stream (via [wlx-capture](https://github.com/galister/wlx-capture)), and presents the frames as a window on Stardust's Wayland socket. Input you give that window in VR is replayed on the host through uinput devices.

## Present paths: dmabuf vs shm

- **DMA-BUF (default).** PipeWire DMA-BUF frames are handed straight to the compositor through `zwp_linux_dmabuf_v1` (v3+). No CPU copy, so full-res monitors stay smooth.
- **shm fallback.** If the compositor does not advertise a usable format/modifier, or the stream is not DMA-BUF, frames are copied into a `softbuffer` (wl_shm) window. Works everywhere, costs CPU at high resolutions.

Force shm with `HOST_MIRROR_DMABUF=0`.

## Build

```sh
cargo build --release
```

## Run

Point `WAYLAND_DISPLAY` at **Stardust's** Wayland socket (not GNOME's), e.g. the one `display-socket-finder wayland` prints in a Stardust startup script:

```sh
WAYLAND_DISPLAY=wayland-1 ./target/release/host-mirror --label eDP-1
```

```
host-mirror [--label NAME] [--token-file PATH] [--size WxH] [--no-input]
```

## Environment

| Variable | Meaning |
|---|---|
| `HOST_MIRROR_LABEL` | Window label / monitor name (`--label`). |
| `HOST_MIRROR_TOKEN_FILE` | Portal restore-token file (`--token-file`). |
| `HOST_MIRROR_SIZE` | Window size `WxH` (`--size`). Default: captured monitor size. |
| `HOST_MIRROR_NO_INPUT=1` | Don't create uinput devices (`--no-input`). |
| `HOST_MIRROR_DMABUF=0` | Disable the DMA-BUF path, use shm. |
| `HOST_MIRROR_DMABUF_FORMATS` | Format preference, e.g. `AR24,XR24` (default `XR24,AR24,XB24,AB24`). |
| `HOST_MIRROR_FOLLOW_WINDOW=1` | Resize buffers to follow the window instead of locking to monitor size. |
| `HOST_MIRROR_CLOSE_EXITS=1` | Exit on window close (otherwise close requests are ignored). |
| `HOST_MIRROR_DUMP_DIR` | Debug: dump frames to this directory. |
| `RUST_LOG` | Logging (`env_logger`). |

## Portal restore tokens

The first run shows the GNOME portal picker; choose the monitor once. The restore token is saved (default with a label: `$XDG_STATE_HOME/host-mirror/<label>.token`) and reused, so later runs start without a dialog. Delete the token file, or let it go stale, to pick again. Tokens are per-machine; never commit them.

## Mirroring two monitors

Run one instance per monitor with different labels (each gets its own token file):

```sh
WAYLAND_DISPLAY=wayland-1 host-mirror --label eDP-1 &
WAYLAND_DISPLAY=wayland-1 host-mirror --label Meta-0 --no-input &
```

Give only one of them input (`--no-input` on the others) so two sets of uinput devices don't fight. Each mirror is its own PipeWire stream, so expect roughly double the GPU/bandwidth cost.

## Requirements

Linux, PipeWire, xdg-desktop-portal (GNOME tested), access to `/dev/uinput` for input, a Stardust XR server with Wayland support.

## License

Source is MIT (see `LICENSE`). Note that it links `wlx-capture` (GPL-3.0), so distributed binaries fall under GPL-3.0.
