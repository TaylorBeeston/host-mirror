//! Keyboard from Stardust, not winit.
//!
//! Flatland's KeyboardHandler (Stardust client input for this panel) sends every key as a
//! 24-byte datagram to `$XDG_RUNTIME_DIR/host-mirror-keys/<app_id>.sock`:
//! keycode (evdev), pressed, depressed, latched, locked, layout_group (LE u32).
//! The xkb `depressed` mask is Stardust's own modifier state, so we use it to release any
//! modifier we still hold that Stardust says is up (fixes stuck Super/Ctrl/Shift/Alt).
use std::os::unix::net::UnixDatagram;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy)]
pub struct StardustKey {
    pub keycode: u16,
    pub pressed: bool,
    pub depressed: u32,
}

pub struct StardustKeys {
    sock: UnixDatagram,
    path: PathBuf,
}

impl StardustKeys {
    /// Bind the sink. None when disabled (HOST_MIRROR_STARDUST_KEYS=0) or unavailable.
    pub fn bind(app_id: &str) -> Option<Self> {
        if std::env::var("HOST_MIRROR_STARDUST_KEYS").is_ok_and(|v| v == "0") {
            log::info!("stardust key sink disabled (HOST_MIRROR_STARDUST_KEYS=0); winit keys only");
            return None;
        }
        let dir = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR")?).join("host-mirror-keys");
        let path = dir.join(format!("{app_id}.sock"));
        let bound = std::fs::create_dir_all(&dir)
            .and_then(|_| {
                let _ = std::fs::remove_file(&path);
                UnixDatagram::bind(&path)
            })
            .and_then(|sock| sock.set_nonblocking(true).map(|_| sock));
        match bound {
            Ok(sock) => {
                log::info!("stardust key sink listening at {}", path.display());
                Some(Self { sock, path })
            }
            Err(err) => {
                log::warn!("stardust key sink bind {} failed: {err}; winit keys only", path.display());
                None
            }
        }
    }

    /// Drain pending keys (non-blocking).
    pub fn drain(&self) -> Vec<StardustKey> {
        let mut out = Vec::new();
        let mut buf = [0u8; 64];
        while let Ok(n) = self.sock.recv(&mut buf) {
            if n < 12 {
                continue;
            }
            let u = |i: usize| u32::from_le_bytes(buf[i * 4..i * 4 + 4].try_into().unwrap());
            let Ok(keycode) = u16::try_from(u(0)) else { continue };
            out.push(StardustKey { keycode, pressed: u(1) != 0, depressed: u(2) });
        }
        out
    }
}

impl Drop for StardustKeys {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// xkb modifier bit (standard keymap) for an evdev modifier key.
pub fn modifier_bit(keycode: u16) -> Option<u32> {
    match keycode {
        42 | 54 => Some(1 << 0),   // Shift
        29 | 97 => Some(1 << 2),   // Control
        56 => Some(1 << 3),        // Alt (Mod1); RightAlt is often AltGr/Mod5, left out
        125 | 126 => Some(1 << 6), // Super (Mod4)
        _ => None,
    }
}

/// Held modifiers (other than `except`) that the Stardust mask says are up.
pub fn stale_modifiers(held: impl Iterator<Item = u16>, depressed: u32, except: u16) -> Vec<u16> {
    held.filter(|&k| k != except)
        .filter(|&k| modifier_bit(k).is_some_and(|b| depressed & b == 0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn super_released_when_mask_clears() {
        assert_eq!(stale_modifiers([125u16, 30].into_iter(), 0, 30), vec![125]);
        assert!(stale_modifiers([125u16].into_iter(), 1 << 6, 30).is_empty());
        assert!(stale_modifiers([29u16].into_iter(), 0, 29).is_empty());
        assert_eq!(stale_modifiers([29u16, 42].into_iter(), 1 << 0, 46), vec![29]);
    }
}
