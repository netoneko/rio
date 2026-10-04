use std::cell::Cell;
use std::collections::VecDeque;
use std::io::Read;
use std::marker::PhantomData;
use std::os::unix::io::AsRawFd;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use smol_str::SmolStr;

use crate::error::EventLoopError;
use crate::event::{self, Ime, Modifiers, StartCause};
use crate::event_loop::{self, ControlFlow, DeviceEvents};
use crate::keyboard::{
    Key, KeyCode, KeyLocation, ModifiersKeys, ModifiersState, NamedKey, NativeKey,
    NativeKeyCode, PhysicalKey,
};
use crate::window::{
    CustomCursor as RootCustomCursor, CustomCursorSource, WindowId as RootWindowId,
};

use super::{DeviceId, KeyEventExtra, MonitorHandle, OsError, WindowId};

// ---------------------------------------------------------------------------
// The console tty: raw mode, escape-sequence decoding
// ---------------------------------------------------------------------------

/// The console keyboard. rio's stdin is the kernel console tty; we put it in
/// raw mode and decode the bytes (escape sequences, control bytes, UTF-8)
/// into one `KeyMsg` per press.
struct Tty {
    fd: std::os::unix::io::RawFd,
    saved: libc::termios,
}

impl Tty {
    fn open() -> Result<Self, OsError> {
        let fd = libc::STDIN_FILENO;
        let mut saved: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: fd points at our stdin for the process lifetime; the
        // termios pointers are valid kernel-ABI structs.
        if unsafe { libc::tcgetattr(fd, &mut saved) } == -1 {
            return Err(OsError::new(std::io::Error::last_os_error()));
        }
        let mut raw = saved;
        //same flags as every raw console client (cfmakeraw): no echo, no
        // line buffering, no signal generation (^C arrives as a byte).
        raw.c_iflag &= !(libc::IGNBRK
            | libc::BRKINT
            | libc::PARMRK
            | libc::ISTRIP
            | libc::INLCR
            | libc::IGNCR
            | libc::ICRNL
            | libc::IXON);
        raw.c_oflag &= !libc::OPOST;
        raw.c_lflag &= !(libc::ECHO | libc::ECHONL | libc::ICANON | libc::ISIG | libc::IEXTEN);
        raw.c_cflag &= !(libc::CSIZE | libc::PARENB);
        raw.c_cflag |= libc::CS8;
        raw.c_cc[libc::VMIN] = 0;
        raw.c_cc[libc::VTIME] = 0;
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } == -1 {
            return Err(OsError::new(std::io::Error::last_os_error()));
        }
        Ok(Tty { fd, saved })
    }

    /// Non-blocking read of whatever bytes are queued.
    fn read_available(&self, buf: &mut [u8]) -> usize {
        // SAFETY: buf is a valid slice for the process lifetime.
        let n = unsafe { libc::read(self.fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n <= 0 {
            0
        } else {
            n as usize
        }
    }
}

impl Drop for Tty {
    fn drop(&mut self) {
        // restore the line discipline the console set up
        // SAFETY: as in `open`.
        unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.saved) };
    }
}

/// One decoded key event from the console.
#[derive(Debug, Clone)]
struct KeyMsg {
    /// text produced on press (before ctrl folding), if any
    text: Option<String>,
    logical: Key,
    physical: PhysicalKey,
    named: Option<NamedKey>,
    pressed: bool,
}

/// Decode one or more `KeyMsg`s out of a raw byte buffer (console
/// encoding: US layout, ESC [ .. sequences for special keys, control
/// bytes for Ctrl+letter). Returns how many bytes were consumed.
fn decode_keys(buf: &[u8], out: &mut Vec<KeyMsg>) -> usize {
    let mut i = 0;
    while i < buf.len() {
        let b = buf[i];
        if b == 0x1b {
            // escape sequence or bare ESC
            if buf.len() >= i + 3 && buf[i + 1] == b'[' {
                // CSI: parameters then a final byte
                let mut j = i + 2;
                while j < buf.len() && !(0x40..=0x7e).contains(&buf[j]) {
                    j += 1;
                }
                if j >= buf.len() {
                    break; // incomplete; wait for more bytes
                }
                let params = &buf[i + 2..j];
                let final_byte = buf[j];
                if let Some(msg) = decode_csi(params, final_byte) {
                    out.push(msg);
                }
                i = j + 1;
                continue;
            }
            if buf.len() >= i + 2 && buf[i + 1] == b'O' && buf.len() >= i + 3 {
                // SS3: O P / O Q / O R / O S = F1..F4
                let msg = match buf[i + 2] {
                    b'P' => named(NamedKey::F1, KeyCode::F1),
                    b'Q' => named(NamedKey::F2, KeyCode::F2),
                    b'R' => named(NamedKey::F3, KeyCode::F3),
                    b'S' => named(NamedKey::F4, KeyCode::F4),
                    _ => {
                        out.push(named(NamedKey::Escape, KeyCode::Escape));
                        i += 1;
                        continue;
                    }
                };
                out.push(msg);
                i += 3;
                continue;
            }
            if buf.len() >= i + 2 && 0x20 <= buf[i + 1] && buf[i + 1] <= 0x7e {
                // ESC + printable = Alt+char (console encoding)
                let ch = buf[i + 1] as char;
                let (physical, _) = physical_for_char(ch.to_ascii_lowercase());
                out.push(KeyMsg {
                    text: Some(ch.to_string()),
                    logical: Key::Character(ch.to_string().into()),
                    physical,
                    named: None,
                    pressed: true,
                });
                i += 2;
                continue;
            }
            out.push(named(NamedKey::Escape, KeyCode::Escape));
            i += 1;
            continue;
        }
        match b {
            b'\r' | b'\n' => out.push(KeyMsg {
                text: Some("\n".into()),
                logical: Key::Named(NamedKey::Enter),
                physical: PhysicalKey::Code(KeyCode::Enter),
                named: Some(NamedKey::Enter),
                pressed: true,
            }),
            b'\t' => out.push(KeyMsg {
                text: Some("\t".into()),
                logical: Key::Named(NamedKey::Tab),
                physical: PhysicalKey::Code(KeyCode::Tab),
                named: Some(NamedKey::Tab),
                pressed: true,
            }),
            0x7f | 0x08 => out.push(KeyMsg {
                text: Some("\u{7f}".into()),
                logical: Key::Named(NamedKey::Backspace),
                physical: PhysicalKey::Code(KeyCode::Backspace),
                named: Some(NamedKey::Backspace),
                pressed: true,
            }),
            0x01..=0x1a => {
                // Ctrl+letter (0x01..0x1a; tab/cr already handled above)
                let ch = (b'a' + (b - 1)) as char;
                let (physical, _) = physical_for_char(ch);
                out.push(KeyMsg {
                    text: None,
                    logical: Key::Character(ch.to_string().into()),
                    physical,
                    named: None,
                    pressed: true,
                });
            }
            0x20..=0x7e => {
                let ch = b as char;
                let (physical, _) = physical_for_char(ch);
                out.push(KeyMsg {
                    text: Some(ch.to_string()),
                    logical: Key::Character(ch.to_string().into()),
                    physical,
                    named: None,
                    pressed: true,
                });
            }
            0x80..=0xff => {
                // UTF-8 continuation; decode a scalar
                let len = utf8_len(b);
                if i + len > buf.len() {
                    break; // incomplete
                }
                match std::str::from_utf8(&buf[i..i + len]) {
                    Ok(s) => {
                        let ch = s.chars().next().unwrap();
                        let (physical, _) = physical_for_char(ch.to_ascii_lowercase());
                        out.push(KeyMsg {
                            text: Some(s.to_string()),
                            logical: Key::Character(s.into()),
                            physical,
                            named: None,
                            pressed: true,
                        });
                    }
                    Err(_) => {}
                }
                i += len;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    i
}

fn decode_csi(params: &[u8], final_byte: u8) -> Option<KeyMsg> {
    let named_key = |n: NamedKey, c: KeyCode| named(n, c);
    match final_byte {
        b'A' => Some(named_key(NamedKey::ArrowUp, KeyCode::ArrowUp)),
        b'B' => Some(named_key(NamedKey::ArrowDown, KeyCode::ArrowDown)),
        b'C' => Some(named_key(NamedKey::ArrowRight, KeyCode::ArrowRight)),
        b'D' => Some(named_key(NamedKey::ArrowLeft, KeyCode::ArrowLeft)),
        b'H' => Some(named_key(NamedKey::Home, KeyCode::Home)),
        b'F' => Some(named_key(NamedKey::End, KeyCode::End)),
        b'~' => {
            let n: u32 = std::str::from_utf8(params).ok()?.parse().ok()?;
            let (n, c) = match n {
                1 | 7 => (NamedKey::Home, KeyCode::Home),
                2 => (NamedKey::Insert, KeyCode::Insert),
                3 => (NamedKey::Delete, KeyCode::Delete),
                4 | 8 => (NamedKey::End, KeyCode::End),
                5 => (NamedKey::PageUp, KeyCode::PageUp),
                6 => (NamedKey::PageDown, KeyCode::PageDown),
                11 => (NamedKey::F1, KeyCode::F1),
                12 => (NamedKey::F2, KeyCode::F2),
                13 => (NamedKey::F3, KeyCode::F3),
                14 => (NamedKey::F4, KeyCode::F4),
                15 => (NamedKey::F5, KeyCode::F5),
                17 => (NamedKey::F6, KeyCode::F6),
                18 => (NamedKey::F7, KeyCode::F7),
                19 => (NamedKey::F8, KeyCode::F8),
                20 => (NamedKey::F9, KeyCode::F9),
                21 => (NamedKey::F10, KeyCode::F10),
                23 => (NamedKey::F11, KeyCode::F11),
                24 => (NamedKey::F12, KeyCode::F12),
                _ => return None,
            };
            Some(named_key(n, c))
        }
        _ => None,
    }
}

fn named(n: NamedKey, c: KeyCode) -> KeyMsg {
    KeyMsg {
        text: None,
        logical: Key::Named(n),
        physical: PhysicalKey::Code(c),
        named: Some(n),
        pressed: true,
    }
}

fn utf8_len(b: u8) -> usize {
    if b >= 0xf0 {
        4
    } else if b >= 0xe0 {
        3
    } else if b >= 0xc0 {
        2
    } else {
        1
    }
}

/// Physical key + named key for an ASCII character (US layout).
fn physical_for_char(ch: char) -> (PhysicalKey, Option<NamedKey>) {
    let code = match ch {
        'a' => KeyCode::KeyA,
        'b' => KeyCode::KeyB,
        'c' => KeyCode::KeyC,
        'd' => KeyCode::KeyD,
        'e' => KeyCode::KeyE,
        'f' => KeyCode::KeyF,
        'g' => KeyCode::KeyG,
        'h' => KeyCode::KeyH,
        'i' => KeyCode::KeyI,
        'j' => KeyCode::KeyJ,
        'k' => KeyCode::KeyK,
        'l' => KeyCode::KeyL,
        'm' => KeyCode::KeyM,
        'n' => KeyCode::KeyN,
        'o' => KeyCode::KeyO,
        'p' => KeyCode::KeyP,
        'q' => KeyCode::KeyQ,
        'r' => KeyCode::KeyR,
        's' => KeyCode::KeyS,
        't' => KeyCode::KeyT,
        'u' => KeyCode::KeyU,
        'v' => KeyCode::KeyV,
        'w' => KeyCode::KeyW,
        'x' => KeyCode::KeyX,
        'y' => KeyCode::KeyY,
        'z' => KeyCode::KeyZ,
        '0' => KeyCode::Digit0,
        '1' => KeyCode::Digit1,
        '2' => KeyCode::Digit2,
        '3' => KeyCode::Digit3,
        '4' => KeyCode::Digit4,
        '5' => KeyCode::Digit5,
        '6' => KeyCode::Digit6,
        '7' => KeyCode::Digit7,
        '8' => KeyCode::Digit8,
        '9' => KeyCode::Digit9,
        ' ' => return (PhysicalKey::Code(KeyCode::Space), Some(NamedKey::Space)),
        _ => return (PhysicalKey::Unidentified(NativeKeyCode::Unidentified), None),
    };
    (PhysicalKey::Code(code), None)
}

// ---------------------------------------------------------------------------
// Modifier state (shared shape with the orbital platform)
// ---------------------------------------------------------------------------

/// Fold the console's ctrl byte back in: with Ctrl held, 'i' really means
/// Tab and 'm' means Enter (the tty folds them before we ever see them).
fn refold(msg: &mut KeyMsg, ctrl: bool) {
    if !ctrl {
        return;
    }
    if let Key::Character(s) = &msg.logical {
        if s.len() == 1 {
            let ch = s.chars().next().unwrap();
            let (named, physical) = match ch {
                'i' => (Some(NamedKey::Tab), PhysicalKey::Code(KeyCode::Tab)),
                'm' | 'j' => (Some(NamedKey::Enter), PhysicalKey::Code(KeyCode::Enter)),
                'h' => (Some(NamedKey::Backspace), PhysicalKey::Code(KeyCode::Backspace)),
                _ => (None, msg.physical),
            };
            if let Some(n) = named {
                msg.named = Some(n);
                msg.logical = Key::Named(n);
                msg.physical = physical;
                msg.text = None;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The event loop
// ---------------------------------------------------------------------------

/// A cross-thread wakeup: a self-pipe. Writers (request_redraw,
/// EventLoopProxy) put a byte in; the loop's poll set includes the read end.
pub(crate) struct Waker {
    write: std::os::unix::io::RawFd,
}

impl Waker {
    fn new() -> (std::os::unix::io::RawFd, Self) {
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: fds is the pipe(2) out array.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } == -1 {
            panic!("fb platform: pipe() failed: {}", std::io::Error::last_os_error());
        }
        (fds[0], Waker { write: fds[1] })
    }

    pub fn wake(&self) {
        let byte = 1u8;
        // SAFETY: the write end is ours and never closed while any Waker
        // clone lives.
        unsafe { libc::write(self.write, &byte as *const u8 as *const libc::c_void, 1) };
    }
}

pub struct EventLoop<T> {
    tty: Arc<Mutex<Option<Tty>>>,
    pipe_read: std::os::unix::io::RawFd,
    window_target: event_loop::ActiveEventLoop,
    user_events_sender: mpsc::Sender<T>,
    user_events_receiver: mpsc::Receiver<T>,
}

impl<T: 'static> EventLoop<T> {
    pub(crate) fn new(
        _: &super::PlatformSpecificEventLoopAttributes,
    ) -> Result<Self, EventLoopError> {
        let (user_events_sender, user_events_receiver) = mpsc::channel();

        // The screen must exist before any window or wgpu surface: open
        // /dev/fb0 once, publish AKUMA_FB_FD for the wgpu backend.
        if super::Screen::get().is_none() {
            match super::Screen::open() {
                Ok(screen) => {
                    let _ = super::SCREEN.set(screen);
                }
                Err(e) => {
                    return Err(EventLoopError::Os(os_error!(OsError::new(e))));
                }
            }
        }

        let tty = Tty::open()
            .map_err(|e| EventLoopError::Os(os_error!(e)))?;
        let (pipe_read, waker) = Waker::new();

        Ok(Self {
            tty: Arc::new(Mutex::new(Some(tty))),
            pipe_read,
            window_target: event_loop::ActiveEventLoop {
                p: ActiveEventLoop {
                    control_flow: Cell::new(ControlFlow::default()),
                    exit: Cell::new(false),
                    creates: Mutex::new(VecDeque::new()),
                    redraws: Arc::new(Mutex::new(VecDeque::new())),
                    destroys: Arc::new(Mutex::new(VecDeque::new())),
                    waker: Arc::new(Mutex::new(waker)),
                },
                _marker: PhantomData,
            },
            user_events_sender,
            user_events_receiver,
        })
    }

    pub fn window_target(&self) -> &event_loop::ActiveEventLoop {
        &self.window_target
    }

    pub fn create_proxy(&self) -> EventLoopProxy<T> {
        EventLoopProxy {
            user_events_sender: self.user_events_sender.clone(),
            waker: self.window_target.p.waker.clone(),
        }
    }

    pub fn run<F>(mut self, mut event_handler_inner: F) -> Result<(), EventLoopError>
    where
        F: FnMut(event::Event<T>, &event_loop::ActiveEventLoop),
    {
        let mut event_handler =
            move |event: event::Event<T>, window_target: &event_loop::ActiveEventLoop| {
                event_handler_inner(event, window_target);
            };

        let window_id_cell: std::cell::Cell<Option<WindowId>> = std::cell::Cell::new(None);

        let mut start_cause = StartCause::Init;
        let mut keybuf: Vec<u8> = Vec::with_capacity(256);
        // modifier latches: the console folds modifiers into the bytes it
        // sends (upper-case text = shift, control bytes = ctrl, ESC prefix
        // = alt), so they are recovered per key press, not tracked
        let mut last_mods = Modifiers::default();

        loop {
            event_handler(event::Event::NewEvents(start_cause), &self.window_target);

            if start_cause == StartCause::Init {
                event_handler(event::Event::Resumed, &self.window_target);
            }

            // Handle window creates: the fb window is born knowing its size.
            while let Some((win_id, size)) = {
                let mut creates = self.window_target.p.creates.lock().unwrap();
                creates.pop_front()
            } {
                window_id_cell.set(Some(win_id));
                event_handler(
                    event::Event::WindowEvent {
                        window_id: RootWindowId(win_id),
                        event: event::WindowEvent::Resized(size),
                    },
                    &self.window_target,
                );
                event_handler(
                    event::Event::WindowEvent {
                        window_id: RootWindowId(win_id),
                        event: event::WindowEvent::Focused(true),
                    },
                    &self.window_target,
                );
            }

            // Handle window destroys.
            while let Some(destroy_id) = {
                let mut destroys = self.window_target.p.destroys.lock().unwrap();
                destroys.pop_front()
            } {
                event_handler(
                    event::Event::WindowEvent {
                        window_id: RootWindowId(destroy_id),
                        event: event::WindowEvent::Destroyed,
                    },
                    &self.window_target,
                );
            }

            // Decode and deliver tty input.
            let mut msgs: Vec<KeyMsg> = Vec::new();
            {
                let guard = self.tty.lock().unwrap();
                if let Some(tty) = guard.as_ref() {
                    let mut chunk = [0u8; 256];
                    loop {
                        let n = tty.read_available(&mut chunk);
                        if n == 0 {
                            break;
                        }
                        keybuf.extend_from_slice(&chunk[..n]);
                        if keybuf.len() > 4096 {
                            keybuf.clear(); // runaway sequence; drop it
                            break;
                        }
                        if n < chunk.len() {
                            break;
                        }
                    }
                    let consumed = decode_keys(&keybuf, &mut msgs);
                    keybuf.drain(..consumed);
                }
            }

            for mut msg in msgs {
                // A control byte (Ctrl+letter) is a Character key with no
                // text; fold Tab/Enter/Backspace back in.
                let ctrl = msg.text.is_none()
                    && msg.named.is_none()
                    && matches!(&msg.logical, Key::Character(_));
                refold(&mut msg, ctrl);

                let shift = msg
                    .text
                    .as_ref()
                    .is_some_and(|t| t.chars().all(|c| c.is_ascii_uppercase()));
                let alt = alt_of(&msg);
                let mods = keyboard_modifiers(shift, ctrl, alt);

                let id = window_id_cell.get().unwrap_or(WindowId::dummy());
                event_handler(
                    event::Event::WindowEvent {
                        window_id: RootWindowId(id),
                        event: event::WindowEvent::KeyboardInput {
                            device_id: event::DeviceId(DeviceId),
                            event: event::KeyEvent {
                                logical_key: msg.logical.clone(),
                                physical_key: msg.physical,
                                location: KeyLocation::Standard,
                                state: event::ElementState::Pressed,
                                repeat: false,
                                text: msg.text.clone().map(|t| {
                                    if ctrl {
                                        SmolStr::new("")
                                    } else {
                                        SmolStr::from(t)
                                    }
                                }),
                                platform_specific: KeyEventExtra {
                                    key_without_modifiers: msg.logical.clone(),
                                    text_with_all_modifiers: msg
                                        .text
                                        .clone()
                                        .map(SmolStr::from)
                                        .filter(|t| !t.is_empty()),
                                },
                            },
                            is_synthetic: false,
                        },
                    },
                    &self.window_target,
                );
                if mods.state != last_mods.state {
                    event_handler(
                        event::Event::WindowEvent {
                            window_id: RootWindowId(id),
                            event: event::WindowEvent::ModifiersChanged(mods.clone()),
                        },
                        &self.window_target,
                    );
                    last_mods = mods;
                }
            }

            while let Ok(event) = self.user_events_receiver.try_recv() {
                event_handler(event::Event::UserEvent(event), &self.window_target);
            }

            while let Some(id) = {
                let mut redraws = self.window_target.p.redraws.lock().unwrap();
                redraws.pop_front()
            } {
                event_handler(
                    event::Event::WindowEvent {
                        window_id: RootWindowId(id),
                        event: event::WindowEvent::RedrawRequested,
                    },
                    &self.window_target,
                );
            }

            event_handler(event::Event::AboutToWait, &self.window_target);

            if self.window_target.p.exiting() {
                break;
            }

            let timeout: Option<Duration> = match self.window_target.p.control_flow() {
                ControlFlow::Poll => Some(Duration::ZERO),
                ControlFlow::Wait => None,
                ControlFlow::WaitUntil(instant) => {
                    Some(instant.saturating_duration_since(Instant::now()))
                }
            };

            // Wait: tty fd + self-pipe.
            let tty_fd = self
                .tty
                .lock()
                .unwrap()
                .as_ref()
                .map(|t| t.fd)
                .unwrap_or(-1);
            let mut fds = [
                libc::pollfd { fd: tty_fd, events: libc::POLLIN, revents: 0 },
                libc::pollfd { fd: self.pipe_read, events: libc::POLLIN, revents: 0 },
            ];
            let timeout_ms = timeout.map(|d| d.as_millis() as libc::c_int).unwrap_or(-1);
            // SAFETY: fds is a valid poll array for the call.
            let ready = unsafe { libc::poll(fds.as_mut_ptr(), 2, timeout_ms) };
            let _ = ready; // EINTR etc. just fall through to a fresh iteration

            if fds[1].revents & libc::POLLIN != 0 {
                // drain the pipe
                let mut b = [0u8; 64];
                // SAFETY: valid fd + buffer.
                unsafe { libc::read(self.pipe_read, b.as_mut_ptr() as *mut libc::c_void, b.len()) };
            }

            start_cause = StartCause::Poll;
        }

        // Restore the console line discipline on the way out.
        if let Some(tty) = self.tty.lock().unwrap().take() {
            drop(tty);
        }

        event_handler(event::Event::LoopExiting, &self.window_target);

        Ok(())
    }

    fn current_window_id(&self) -> Option<WindowId> {
        None // set per-delivery by the run loop when windows exist
    }
}

fn keyboard_modifiers(shift: bool, ctrl: bool, alt: bool) -> Modifiers {
    let mut mods = ModifiersState::empty();
    let mut pressed = ModifiersKeys::empty();
    if shift {
        mods |= ModifiersState::SHIFT;
        pressed.set(ModifiersKeys::LSHIFT, true);
    }
    if ctrl {
        mods |= ModifiersState::CONTROL;
        pressed.set(ModifiersKeys::LCONTROL, true);
    }
    if alt {
        mods |= ModifiersState::ALT;
        pressed.set(ModifiersKeys::LALT, true);
    }
    Modifiers {
        state: mods,
        pressed_mods: pressed,
    }
}

/// Alt+char (ESC-prefixed) has no text folding, but the escape decoder
/// does not currently flag it; extended to keep the call site honest.
fn alt_of(_msg: &KeyMsg) -> bool {
    false
}

pub struct EventLoopProxy<T: 'static> {
    user_events_sender: mpsc::Sender<T>,
    waker: Arc<Mutex<Waker>>,
}

impl<T> EventLoopProxy<T> {
    pub fn send_event(&self, event: T) -> Result<(), event_loop::EventLoopClosed<T>> {
        self.user_events_sender
            .send(event)
            .map_err(|mpsc::SendError(x)| event_loop::EventLoopClosed(x))?;
        self.waker.lock().unwrap().wake();
        Ok(())
    }
}

impl<T> Clone for EventLoopProxy<T> {
    fn clone(&self) -> Self {
        Self {
            user_events_sender: self.user_events_sender.clone(),
            waker: self.waker.clone(),
        }
    }
}

impl<T> Unpin for EventLoopProxy<T> {}

pub struct ActiveEventLoop {
    control_flow: Cell<ControlFlow>,
    exit: Cell<bool>,
    pub(super) creates: Mutex<VecDeque<(WindowId, crate::dpi::PhysicalSize<u32>)>>,
    pub(super) redraws: Arc<Mutex<VecDeque<WindowId>>>,
    pub(super) destroys: Arc<Mutex<VecDeque<WindowId>>>,
    pub(super) waker: Arc<Mutex<Waker>>,
}

impl ActiveEventLoop {
    pub fn create_custom_cursor(&self, source: CustomCursorSource) -> RootCustomCursor {
        let _ = source.inner;
        RootCustomCursor {
            inner: super::PlatformCustomCursor,
        }
    }

    pub fn primary_monitor(&self) -> Option<MonitorHandle> {
        Some(MonitorHandle)
    }

    pub fn cursor_monitor(&self) -> Option<MonitorHandle> {
        Some(MonitorHandle)
    }

    pub fn available_monitors(&self) -> VecDeque<MonitorHandle> {
        let mut v = VecDeque::with_capacity(1);
        v.push_back(MonitorHandle);
        v
    }

    #[inline]
    pub fn listen_device_events(&self, _allowed: DeviceEvents) {}

    #[inline]
    pub fn raw_display_handle_raw_window_handle(
        &self,
    ) -> Result<raw_window_handle::RawDisplayHandle, raw_window_handle::HandleError> {
        Ok(raw_window_handle::RawDisplayHandle::Web(
            raw_window_handle::WebDisplayHandle::new(),
        ))
    }

    pub fn set_control_flow(&self, control_flow: ControlFlow) {
        self.control_flow.set(control_flow)
    }

    pub fn system_theme(&self) -> Option<crate::window::Theme> {
        None
    }

    pub fn control_flow(&self) -> ControlFlow {
        self.control_flow.get()
    }

    pub(crate) fn exit(&self) {
        self.exit.set(true);
        self.wake();
    }

    pub(crate) fn exiting(&self) -> bool {
        self.exit.get()
    }

    pub(crate) fn owned_display_handle(&self) -> OwnedDisplayHandle {
        OwnedDisplayHandle
    }

    pub(super) fn wake(&self) {
        self.waker.lock().unwrap().wake();
    }

    pub(super) fn wake_handle(&self) -> Arc<Mutex<Waker>> {
        Arc::clone(&self.waker)
    }
}

#[derive(Clone)]
pub(crate) struct OwnedDisplayHandle;

impl OwnedDisplayHandle {
    #[inline]
    pub fn raw_display_handle_raw_window_handle(
        &self,
    ) -> Result<raw_window_handle::RawDisplayHandle, raw_window_handle::HandleError> {
        Ok(raw_window_handle::WebDisplayHandle::new().into())
    }
}
