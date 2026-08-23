//! Kindle physical button input via evdev (PagePress, page-turn buttons).
//!
//! Opens ALL /dev/input/event* devices that support EV_KEY and polls them
//! for page turn events. Different Kindle models expose page turn buttons
//! on different event devices (e.g., event0=gpio-keys/power, event3=fsr_keypad).
//! The Kindle Voyage's PagePress capacitive sensors also appear here.

use crate::{PageTurnCallback, PageTurnDirection};

/// Linux `struct input_event` on 32-bit ARM.
/// `timeval` has two `long` fields (4 bytes each on 32-bit), then
/// `__u16 type`, `__u16 code`, `__s32 value`. Total = 16 bytes.
#[repr(C)]
struct InputEvent {
    tv_sec: u32,
    tv_usec: u32,
    kind: u16,
    code: u16,
    value: i32,
}

const EV_KEY: u16 = 0x01;

// Key codes that different Kindle models use for page turn buttons.
// - KEY_PAGEUP=104 / KEY_PAGEDOWN=109: standard Linux, Kindle Voyage fsr_keypad
// - KEY_NEXT=193 / KEY_PREVIOUS=194: some Kindle models
// - KEY_LEFT=105 / KEY_RIGHT=106: directional pad
const KEY_PREV_CODES: &[u16] = &[104, 194, 105]; // PAGEUP, PREVIOUS, LEFT
const KEY_NEXT_CODES: &[u16] = &[109, 193, 106]; // PAGEDOWN, NEXT, RIGHT

// EVIOCGBIT(ev, len) = _IOC(_IOC_READ, 'E', 0x20 + ev, len)
const EVIOCGBIT_EV: libc::c_ulong = 0x80084520; // EVIOCGBIT(0, 8) — ev types
const EVIOCGBIT_KEY: libc::c_ulong = 0x80204521; // EVIOCGBIT(EV_KEY=1, 32)

/// Check if a specific key code is supported by the device's key capability bitmap.
fn key_supported(key_bits: &[u8], code: u16) -> bool {
    let byte = (code / 8) as usize;
    let bit = (code % 8) as u8;
    byte < key_bits.len() && (key_bits[byte] & (1 << bit)) != 0
}

/// Detect Kindle model from /proc/usid and return true if the device
/// has inverted page turn buttons (Oasis, Oasis 2, Oasis 3).
fn is_oasis_model() -> bool {
    let usid = match std::fs::read_to_string("/proc/usid") {
        Ok(s) => s.trim().to_string(),
        Err(_) => return false,
    };
    if usid.len() < 6 {
        return false;
    }

    let prefix6 = &usid[..6];
    let oasis1_prefixes = ["G0B0GC", "G0B0GD", "G0B0GR", "G0B0GU", "G0B0GT"];
    let oasis2_prefixes = ["G000P8", "G000S1", "G000SA", "G000S2", "G000P1"];
    let oasis3_prefixes = ["G0011L", "G000WQ", "G000WM", "G000WL", "G000WN", "G000WP"];

    let code3 = &usid[2..5];
    let oasis1_old = ["0GC", "0GD", "0GR", "0GU", "0GT"];
    let oasis2_old = ["0LM", "0LN", "0LP", "0LQ", "0P1", "0P2", "0P6", "0P7", "0P8", "0S1", "0S2", "0S3", "0S4", "0S7", "0SA"];

    oasis1_prefixes.contains(&prefix6)
        || oasis2_prefixes.contains(&prefix6)
        || oasis3_prefixes.contains(&prefix6)
        || oasis1_old.contains(&code3)
        || oasis2_old.contains(&code3)
}

pub(crate) struct ButtonInput {
    /// Page-turn button devices (file descriptors)
    fds: Vec<libc::c_int>,
    /// On Kindle Oasis, the page turn buttons are inverted:
    /// top button = next page, bottom button = prev page.
    /// On most other Kindles (Voyage, etc.), bottom = next, top = prev.
    inverted: bool,
}

impl ButtonInput {
    /// Find and open ALL /dev/input/event* devices that report EV_KEY
    /// with page-turn key codes. Returns None if no such devices found.
    pub(crate) fn open() -> Option<Self> {
        let mut fds = Vec::new();

        for n in 0..16 {
            let path = format!("/dev/input/event{n}");
            let c_path = std::ffi::CString::new(path.as_str()).unwrap();
            let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK) };
            if fd < 0 {
                continue;
            }

            // Check if this device supports EV_KEY
            let mut ev_bits = [0u8; 8];
            let ret = unsafe { libc::ioctl(fd, EVIOCGBIT_EV as _, ev_bits.as_mut_ptr()) };
            if ret < 0 {
                unsafe { libc::close(fd) };
                continue;
            }
            // EV_KEY is type 1 → byte 0, bit 1
            if ev_bits[0] & 0x02 == 0 {
                unsafe { libc::close(fd) };
                continue;
            }

            // Get the key capability bitmap
            let mut key_bits = [0u8; 32];
            let ret = unsafe { libc::ioctl(fd, EVIOCGBIT_KEY as _, key_bits.as_mut_ptr()) };
            if ret < 0 {
                unsafe { libc::close(fd) };
                continue;
            }

            // Check if any of our target key codes are supported
            let has_prev = KEY_PREV_CODES.iter().any(|&code| key_supported(&key_bits, code));
            let has_next = KEY_NEXT_CODES.iter().any(|&code| key_supported(&key_bits, code));

            if has_prev || has_next {
                log::info!("[kindle-buttons] found page-turn device at {path} (prev={has_prev}, next={has_next})");
                fds.push(fd);
            } else {
                unsafe { libc::close(fd) };
            }
        }

        if fds.is_empty() {
            log::info!("[kindle-buttons] no page-turn input devices found");
            None
        } else {
            let inverted = is_oasis_model();
            if inverted {
                log::info!("[kindle-buttons] Oasis model detected, inverting page buttons");
            }
            Some(Self { fds, inverted })
        }
    }

    /// Return all file descriptors for polling.
    pub(crate) fn fds(&self) -> &[libc::c_int] {
        &self.fds
    }

    /// Read any waiting button events and invoke the callback for each.
    pub(crate) fn poll(&mut self, on_page_turn: &PageTurnCallback) {
        const SIZE: usize = std::mem::size_of::<InputEvent>();
        let mut buf = [0u8; SIZE];

        for &fd in &self.fds {
            loop {
                let bytes_read = unsafe {
                    libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, SIZE)
                };
                if bytes_read <= 0 {
                    break;
                }
                let event = unsafe {
                    std::ptr::read_unaligned(buf.as_ptr() as *const InputEvent)
                };
                if event.kind == EV_KEY && (event.value == 1 || event.value == 2) {
                    let is_prev = KEY_PREV_CODES.contains(&event.code);
                    let is_next = KEY_NEXT_CODES.contains(&event.code);
                    if is_prev || is_next {
                        // On Oasis, PAGEUP (top) = next, PAGEDOWN (bottom) = prev.
                        // On other Kindles, PAGEUP = prev, PAGEDOWN = next.
                        let direction = if is_prev {
                            if self.inverted { PageTurnDirection::Next } else { PageTurnDirection::Prev }
                        } else {
                            if self.inverted { PageTurnDirection::Prev } else { PageTurnDirection::Next }
                        };
                        if let Some(cb) = on_page_turn.borrow_mut().as_mut() {
                            cb(direction);
                        }
                    }
                }
            }
        }
    }
}

impl Drop for ButtonInput {
    fn drop(&mut self) {
        for &fd in &self.fds {
            unsafe { libc::close(fd) };
        }
    }
}
