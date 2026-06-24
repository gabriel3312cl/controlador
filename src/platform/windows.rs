// Windows platform — H.264/HEVC decoding (ffmpeg-next) and input capture
// (Win32 low-level hooks).
//
// INCERTIDUMBRE: this code is written for Windows but has NOT been compiled or
// tested. It is a best-effort implementation based on ffmpeg-next 6.x and
// windows 0.56 crate APIs. Expect adjustments during the first Windows build.

use anyhow::{Context, Result};

use crate::protocol::Message;

// ---------------------------------------------------------------------------
// Video decoder
// ---------------------------------------------------------------------------

use ffmpeg_next as ffm;

pub struct VideoDecoder {
    decoder: ffm::decoder::Video,
    scaler: ffm::software::scaling::Context,
    width: u32,
    height: u32,
}

impl VideoDecoder {
    /// Create a decoder for H.264 or HEVC.
    ///
    /// `codec_name` should be `"h264"` or `"hevc"` depending on what the
    /// server's handshake indicates (or detected from the bitstream).
    pub fn new(codec_name: &str, width: u32, height: u32) -> Result<Self> {
        // INCERTIDUMBRE: ffmpeg_next 6.x may require ffm::init() before any
        // other call. The caller must do this once at startup.
        let codec = ffm::decoder::find_by_name(codec_name)
            .with_context(|| format!("find {codec_name} decoder"))?;

        let mut decoder = codec.open().context("open decoder")?;

        // Configure decoder with resolution hints
        decoder.set_parameters(&{
            let mut ctx = ffm::codec::context::Context::new();
            ctx.set_width(width as i32);
            ctx.set_height(height as i32);
            ctx.set_pix_fmt(Some(ffm::format::Pixel::YUV420P));
            ctx
        })?;

        // Scaler: YUV420P → RGBA (what egui needs)
        let scaler = ffm::software::scaling::Context::get(
            ffm::format::Pixel::YUV420P,
            width,
            height,
            ffm::format::Pixel::RGBA,
            width,
            height,
            ffm::software::scaling::Flags::BILINEAR,
        )
        .context("create scaler")?;

        Ok(Self {
            decoder,
            scaler,
            width,
            height,
        })
    }

    /// Feed an encoded packet and receive decoded RGBA frames.
    ///
    /// Returns `Vec<u8>` of RGBA pixel data, or `None` if the decoder needs
    /// more input.
    pub fn decode(&mut self, data: &[u8]) -> Result<Option<Vec<u8>>> {
        // Wrap data in an ffmpeg Packet
        let mut packet = ffm::Packet::new(data.len());
        // INCERTIDUMBRE: ffmpeg-next 6.x Packet API may differ.
        // We assume `Packet::new(size)` allocates and we copy data in.
        packet.data_mut().copy_from_slice(data);

        self.decoder.send_packet(&packet).context("send packet")?;

        let mut decoded = ffm::frame::Video::empty();
        match self.decoder.receive_frame(&mut decoded) {
            Ok(()) => {
                // Convert YUV → RGBA
                let mut rgba = ffm::frame::Video::empty();
                self.scaler.run(&decoded, &mut rgba)?;

                let rgba_data = rgba.data(0).to_vec();
                Ok(Some(rgba_data))
            }
            Err(ffm::Error::Again) => {
                // Need more input — this is normal
                Ok(None)
            }
            Err(e) => Err(anyhow::Error::from(e).context("receive frame")),
        }
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }
}

// ---------------------------------------------------------------------------
// Input capture (Win32 hooks)
// ---------------------------------------------------------------------------

use std::sync::mpsc::Sender;
use windows::Win32::Foundation::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;

/// Start capturing mouse and keyboard events on Windows.
///
/// Events are sent through the provided channel as `Message` variants.
/// This function blocks and should be spawned on a dedicated thread.
///
/// INCERTIDUMBRE: hook procedures need a Windows message pump (GetMessage or
/// PeekMessage) to receive events. This implementation assumes the message
/// pump is running elsewhere (e.g., egui's winit event loop).
pub fn start_input_capture(
    input_tx: Sender<Message>,
) -> Result<()> {
    // Mouse hook
    unsafe {
        let tx = input_tx.clone();
        let hook = SetWindowsHookExW(
            WH_MOUSE_LL,
            Some(mouse_hook_proc),
            None,
            0,
        );
        if hook.is_invalid() {
            return Err(anyhow::anyhow!("SetWindowsHookExW(WH_MOUSE_LL) failed"));
        }
        // INCERTIDUMBRE: the hook handle must be kept alive. We leak it here;
        // a proper implementation would store it and call UnhookWindowsHookEx
        // on shutdown.
        std::mem::forget(hook);
    }

    // Keyboard hook
    unsafe {
        let tx = input_tx;
        let hook = SetWindowsHookExW(
            WH_KEYBOARD_LL,
            Some(keyboard_hook_proc),
            None,
            0,
        );
        if hook.is_invalid() {
            return Err(anyhow::anyhow!(
                "SetWindowsHookExW(WH_KEYBOARD_LL) failed"
            ));
        }
        std::mem::forget(hook);
    }

    Ok(())
}

/// Mouse hook callback.
unsafe extern "system" fn mouse_hook_proc(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if code >= 0 {
        // INCERTIDUMBRE: accessing thread-local state from a hook callback
        // requires careful handling. We use a raw pointer stored in a
        // thread-local to pass the channel sender.
        let msg = match wparam.0 as u32 {
            WM_MOUSEMOVE => {
                // lparam points to MSLLHOOKSTRUCT
                let info = &*(lparam.0 as *const MSLLHOOKSTRUCT);
                // Get screen dimensions for normalization
                let screen_w = GetSystemMetrics(SM_CXSCREEN) as f32;
                let screen_h = GetSystemMetrics(SM_CYSCREEN) as f32;
                Message::MouseMove {
                    x: info.pt.x as f32 / screen_w,
                    y: info.pt.y as f32 / screen_h,
                }
            }
            WM_LBUTTONDOWN => Message::MouseClick {
                button: 0,
                pressed: true,
            },
            WM_LBUTTONUP => Message::MouseClick {
                button: 0,
                pressed: false,
            },
            WM_RBUTTONDOWN => Message::MouseClick {
                button: 1,
                pressed: true,
            },
            WM_RBUTTONUP => Message::MouseClick {
                button: 1,
                pressed: false,
            },
            WM_MBUTTONDOWN => Message::MouseClick {
                button: 2,
                pressed: true,
            },
            WM_MBUTTONUP => Message::MouseClick {
                button: 2,
                pressed: false,
            },
            _ => {
                // Not a mouse event we handle — let the next hook process it
                return CallNextHookEx(None, code, wparam, lparam);
            }
        };

        if let Some(tx) = INPUT_CHANNEL.with(|cell| cell.borrow().clone()) {
            let _ = tx.send(msg);
        }
    }

    CallNextHookEx(None, code, wparam, lparam)
}

/// Keyboard hook callback.
unsafe extern "system" fn keyboard_hook_proc(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if code >= 0 {
        let info = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
        let pressed = wparam.0 as u32 == WM_KEYDOWN || wparam.0 as u32 == WM_SYSKEYDOWN;
        let msg = Message::KeyEvent {
            keycode: info.vkCode,
            pressed,
        };

        if let Some(tx) = INPUT_CHANNEL.with(|cell| cell.borrow().clone()) {
            let _ = tx.send(msg);
        }
    }

    CallNextHookEx(None, code, wparam, lparam)
}

// Thread-local storage for the channel sender, so hook callbacks can send
// input events back to the network layer.
//
// INCERTIDUMBRE: std::thread::LocalKey panics if accessed from a thread
// where it wasn't initialized. Low-level hooks run on a dedicated thread
// set up by Windows. This may not work — the channel sender might need
// to be passed via a global static instead.
std::thread_local! {
    static INPUT_CHANNEL: std::cell::RefCell<Option<Sender<Message>>> =
        std::cell::RefCell::new(None);
}

/// Set the channel sender that hook callbacks will use.
pub fn set_input_channel(tx: Sender<Message>) {
    INPUT_CHANNEL.with(|cell| {
        *cell.borrow_mut() = Some(tx);
    });
}

/// Remove the channel sender (call before dropping the sender).
pub fn clear_input_channel() {
    INPUT_CHANNEL.with(|cell| {
        *cell.borrow_mut() = None;
    });
}
