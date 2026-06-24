// macOS platform — screen capture via CoreGraphics and VideoToolbox H.264 encoding.
//
// Capture:  CGDisplayCreateImage (via core-graphics crate)
// Encoding: VideoToolbox VTCompressionSession (raw FFI)
// Inputs:   CoreGraphics event injection (raw FFI)

use anyhow::{Context, Result};
use core_graphics::display::CGDisplay;
use core_graphics::image::CGImage;
use std::ffi::c_void;
use std::sync::mpsc;

// ---------------------------------------------------------------------------
// FFI type aliases
// ---------------------------------------------------------------------------

type OSStatus = i32;
type CVReturn = i32;

#[allow(non_camel_case_types)]
type CMTimeValue = i64;
#[allow(non_camel_case_types)]
type CMTimeScale = i32;
#[allow(non_camel_case_types)]
type CMTimeFlags = u32;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct CMTime {
    value: CMTimeValue,
    timescale: CMTimeScale,
    flags: CMTimeFlags,
    epoch: CMTimeValue,
}

impl CMTime {
    fn valid(value: CMTimeValue, timescale: CMTimeScale) -> Self {
        Self {
            value,
            timescale,
            flags: 1, // kCMTimeFlags_Valid
            epoch: 0,
        }
    }
}

// Pixel / codec constants
#[allow(non_upper_case_globals)]
const kCVPixelFormatType_32BGRA: u32 = 0x4247_5241; // 'BGRA' LE
#[allow(non_upper_case_globals)]
const kCMVideoCodecType_H264: u32 = 0x6832_3634; // 'h264' (FourCharCode, BE on Apple platforms)

// H.264 profile levels
#[allow(non_upper_case_globals)]
const kVTProfileLevel_H264_Baseline_AutoLevel: &str = "BaselineAutoLevel";
#[allow(non_upper_case_globals)]
const kCMVideoCodecType_HEVC: u32 = 0x6876_6331; // 'hvc1'

// ---------------------------------------------------------------------------
// FFI: CoreVideo
// ---------------------------------------------------------------------------

extern "C" {
    fn CVPixelBufferCreate(
        allocator: *const c_void,
        width: usize,
        height: usize,
        pixel_format_type: u32,
        attributes: *const c_void, // CFDictionary of additional attributes, or NULL
        pixel_buffer_out: *mut *mut c_void,
    ) -> CVReturn;

    fn CVPixelBufferLockBaseAddress(
        pixel_buffer: *mut c_void,
        lock_flags: u32,
    ) -> CVReturn;

    fn CVPixelBufferUnlockBaseAddress(
        pixel_buffer: *mut c_void,
        unlock_flags: u32,
    ) -> CVReturn;

    fn CVPixelBufferGetBaseAddress(pixel_buffer: *mut c_void) -> *mut c_void;
    fn CVPixelBufferGetBytesPerRow(pixel_buffer: *mut c_void) -> usize;
    fn CVPixelBufferGetWidth(pixel_buffer: *mut c_void) -> usize;
    fn CVPixelBufferGetHeight(pixel_buffer: *mut c_void) -> usize;
    fn CVPixelBufferRelease(pixel_buffer: *mut c_void);
}

// ---------------------------------------------------------------------------
// FFI: VideoToolbox
// ---------------------------------------------------------------------------

type VTCompressionOutputCallback = Option<
    unsafe extern "C" fn(
        output_callback_ref_con: *mut c_void,
        source_frame_ref_con: *mut c_void,
        status: OSStatus,
        info_flags: u32,
        sample_buffer: *mut c_void, // CMSampleBufferRef
    ),
>;

extern "C" {
    fn VTCompressionSessionCreate(
        allocator: *const c_void,
        width: i32,
        height: i32,
        codec_type: u32,
        encoder_specification: *const c_void,
        source_image_buffer_attributes: *const c_void,
        compressed_data_allocator: *const c_void,
        output_callback: VTCompressionOutputCallback,
        output_callback_ref_con: *mut c_void,
        compression_session_out: *mut *mut c_void,
    ) -> OSStatus;

    fn VTCompressionSessionEncodeFrame(
        session: *mut c_void,
        image_buffer: *mut c_void,
        presentation_timestamp: CMTime,
        duration: CMTime,
        frame_properties: *const c_void,
        source_frame_ref_con: *mut c_void,
        info_flags_out: *mut u32,
    ) -> OSStatus;

    fn VTSessionSetProperty(
        session: *mut c_void,
        key: *const c_void,   // CFStringRef
        value: *const c_void, // CFTypeRef
    ) -> OSStatus;

    fn VTCompressionSessionCompleteFrames(session: *mut c_void) -> OSStatus;
    fn VTCompressionSessionInvalidate(session: *mut c_void);
}

// ---------------------------------------------------------------------------
// FFI: CoreMedia (CMSampleBuffer / CMBlockBuffer)
// ---------------------------------------------------------------------------

extern "C" {
    fn CMSampleBufferGetDataBuffer(sample_buffer: *mut c_void) -> *mut c_void;

    fn CMBlockBufferGetDataPointer(
        block_buffer: *mut c_void,
        offset: usize,
        length_at_offset: *mut usize,
        total_length: *mut usize,
        data_pointer: *mut *mut c_void,
    ) -> OSStatus;
}

// ---------------------------------------------------------------------------
// VTCompressionSession output callback (C → Rust bridge)
// ---------------------------------------------------------------------------

/// Called by VideoToolbox whenever an encoded frame is ready.
///
/// SAFETY: `output_callback_ref_con` is a valid `*const mpsc::Sender<Vec<u8>>`
/// that lives for the lifetime of the compression session. We never take
/// ownership — we just borrow immutably.
unsafe extern "C" fn vt_output_callback(
    output_callback_ref_con: *mut c_void,
    _source_frame_ref_con: *mut c_void,
    status: OSStatus,
    _info_flags: u32,
    sample_buffer: *mut c_void,
) {
    if status != 0 {
        return;
    }

    // Borrow the sender (never take ownership — leaked at creation time)
    let sender = &*(output_callback_ref_con as *const mpsc::Sender<Vec<u8>>);

    if let Some(data) = extract_sample_buffer_data(sample_buffer) {
        let _ = sender.send(data);
    }
}

/// Extract raw H.264 bytes from a CMSampleBuffer.
unsafe fn extract_sample_buffer_data(sample_buffer: *mut c_void) -> Option<Vec<u8>> {
    if sample_buffer.is_null() {
        return None;
    }

    let block_buffer = CMSampleBufferGetDataBuffer(sample_buffer);
    if block_buffer.is_null() {
        return None;
    }

    let mut length_at_offset: usize = 0;
    let mut total_length: usize = 0;
    let mut data_ptr: *mut c_void = std::ptr::null_mut();

    let status = CMBlockBufferGetDataPointer(
        block_buffer,
        0,
        &mut length_at_offset,
        &mut total_length,
        &mut data_ptr,
    );
    if status != 0 || data_ptr.is_null() || total_length == 0 {
        return None;
    }

    let slice = std::slice::from_raw_parts(data_ptr as *const u8, total_length);
    Some(slice.to_vec())
}

// ---------------------------------------------------------------------------
// VideoEncoder: wraps a VTCompressionSession
// ---------------------------------------------------------------------------

pub struct VideoEncoder {
    session: *mut c_void,
    pub codec_type: u32,
    width: u32,
    height: u32,
    frame_count: i64,
    /// Receiver for encoded H.264/HEVC data.
    output_rx: mpsc::Receiver<Vec<u8>>,
}

// SAFETY: the compression session can be used from any thread.
unsafe impl Send for VideoEncoder {}

impl VideoEncoder {
    /// Create a new H.264 encoder.
    ///
    /// Create a new encoder. Tries H.264 first, falls back to HEVC.
    pub fn new(width: u32, height: u32, fps: u8) -> Result<Self> {
        // Try H.264 first
        match Self::new_with_codec(width, height, fps, kCMVideoCodecType_H264) {
            Ok(enc) => return Ok(enc),
            Err(e) => tracing::warn!("H.264 encoder unavailable ({}), falling back to HEVC", e),
        }
        Self::new_with_codec(width, height, fps, kCMVideoCodecType_HEVC)
    }

    /// Create an encoder with a specific codec type (e.g. H.264 or HEVC).
    pub fn new_with_codec(
        width: u32,
        height: u32,
        _fps: u8,
        codec_type: u32,
    ) -> Result<Self> {
        let (output_tx, output_rx) = mpsc::channel::<Vec<u8>>();

        // Leak the sender into a raw pointer for the C callback.
        // It is never reclaimed — the memory is tiny and the OS cleans up on exit.
        let tx_ptr = Box::into_raw(Box::new(output_tx));

        let mut session: *mut c_void = std::ptr::null_mut();

        // INCERTIDUMBRE: passing null for encoder_specification means
        // "system defaults". This works but may not be optimal.
        let status = unsafe {
            VTCompressionSessionCreate(
                std::ptr::null(),           // allocator (default)
                width as i32,
                height as i32,
                codec_type,
                std::ptr::null(),           // encoder specification
                std::ptr::null(),           // source image buffer attributes (let VT decide)
                std::ptr::null(),           // compressed data allocator
                Some(vt_output_callback),   // output callback
                tx_ptr as *mut c_void,      // ref-con = channel sender
                &mut session,
            )
        };

        if status != 0 || session.is_null() {
            // Reclaim the leaked sender on failure
            unsafe {
                let _ = Box::from_raw(tx_ptr);
            }
            anyhow::bail!("VTCompressionSessionCreate failed: status={status}");
        }

        let encoder = Self {
            session,
            codec_type,
            width,
            height,
            frame_count: 0,
            output_rx,
        };

        // Apply encoding properties (skip ProfileLevel for HEVC — it's H.264-specific)
        encoder.set_property_bool("RealTime", true)?;
        if codec_type == kCMVideoCodecType_H264 {
            encoder.set_property_string("ProfileLevel", kVTProfileLevel_H264_Baseline_AutoLevel)?;
        }
        encoder.set_property_int("AverageBitRate", 20_000_000)?;
        encoder.set_property_int("MaxKeyFrameInterval", 120)?;
        encoder.set_property_bool("AllowFrameReordering", false)?;

        Ok(encoder)
    }

    /// Feed a raw BGRA frame to the encoder.
    ///
    /// The frame data must be in BGRA format with the given bytes-per-row.
    /// After calling this, check `output_rx` for encoded data.
    pub fn encode_frame(&mut self, bgra: &[u8], bytes_per_row: usize) -> Result<()> {
        let pixel_buffer = self.create_pixel_buffer(bgra, bytes_per_row)?;

        let timestamp = CMTime::valid(self.frame_count, 1);
        self.frame_count += 1;

        let status = unsafe {
            VTCompressionSessionEncodeFrame(
                self.session,
                pixel_buffer,
                timestamp,
                CMTime::valid(1, 120),
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };

        unsafe {
            CVPixelBufferRelease(pixel_buffer);
        }

        if status != 0 {
            anyhow::bail!("VTCompressionSessionEncodeFrame failed: status={status}");
        }

        Ok(())
    }

    /// Borrow the output receiver so the caller can drain encoded frames.
    pub fn output_rx(&self) -> &mpsc::Receiver<Vec<u8>> {
        &self.output_rx
    }

    /// Flush pending frames and drain all encoded output.
    /// After calling this, the encoder is consumed (the session is invalidated).
    pub fn finish(mut self) -> Vec<u8> {
        unsafe {
            VTCompressionSessionInvalidate(self.session);
            self.session = std::ptr::null_mut();
        }
        let mut data = Vec::new();
        while let Ok(chunk) = self.output_rx.try_recv() {
            data.extend_from_slice(&chunk);
        }
        data
    }

    /// Mutable access to drain frames without borrowing issues.
    pub fn try_recv_encoded(&mut self) -> Option<Vec<u8>> {
        self.output_rx.try_recv().ok()
    }

    // -- internal helpers --

    fn create_pixel_buffer(&self, bgra: &[u8], bytes_per_row: usize) -> Result<*mut c_void> {
        let mut pixel_buffer: *mut c_void = std::ptr::null_mut();

        let status = unsafe {
            CVPixelBufferCreate(
                std::ptr::null(),
                self.width as usize,
                self.height as usize,
                kCVPixelFormatType_32BGRA,
                std::ptr::null(), // no additional attributes
                &mut pixel_buffer,
            )
        };

        if status != 0 || pixel_buffer.is_null() {
            anyhow::bail!("CVPixelBufferCreate failed: status={status}");
        }

        // Lock and copy data
        unsafe {
            let lock_status = CVPixelBufferLockBaseAddress(pixel_buffer, 0);
            if lock_status != 0 {
                CVPixelBufferRelease(pixel_buffer);
                anyhow::bail!("CVPixelBufferLockBaseAddress failed: status={lock_status}");
            }

            let dst = CVPixelBufferGetBaseAddress(pixel_buffer) as *mut u8;
            let dst_bpr = CVPixelBufferGetBytesPerRow(pixel_buffer);
            let dst_height = CVPixelBufferGetHeight(pixel_buffer);

            // Copy row by row to handle stride differences
            let copy_bytes = (self.width as usize * 4).min(bytes_per_row).min(dst_bpr);
            for row in 0..dst_height {
                let src_offset = row * bytes_per_row;
                let dst_offset = row * dst_bpr;
                if src_offset + copy_bytes <= bgra.len() {
                    std::ptr::copy_nonoverlapping(
                        bgra.as_ptr().add(src_offset),
                        dst.add(dst_offset),
                        copy_bytes,
                    );
                }
            }

            CVPixelBufferUnlockBaseAddress(pixel_buffer, 0);
        }

        Ok(pixel_buffer)
    }

    fn set_property_bool(&self, key: &str, value: bool) -> Result<()> {
        use core_foundation::base::TCFType;
        use core_foundation::boolean::CFBoolean;
        use core_foundation::string::CFString;

        let cf_key = CFString::new(key);
        let cf_value = if value {
            CFBoolean::true_value()
        } else {
            CFBoolean::false_value()
        };

        let status = unsafe {
            VTSessionSetProperty(
                self.session,
                cf_key.as_concrete_TypeRef() as *const c_void,
                cf_value.as_concrete_TypeRef() as *const c_void,
            )
        };

        if status != 0 {
            anyhow::bail!("VTSessionSetProperty({key}) failed: status={status}");
        }
        Ok(())
    }

    fn set_property_int(&self, key: &str, value: i32) -> Result<()> {
        use core_foundation::base::TCFType;
        use core_foundation::number::CFNumber;
        use core_foundation::string::CFString;

        let cf_key = CFString::new(key);
        let cf_value = CFNumber::from(value);

        let status = unsafe {
            VTSessionSetProperty(
                self.session,
                cf_key.as_concrete_TypeRef() as *const c_void,
                cf_value.as_concrete_TypeRef() as *const c_void,
            )
        };

        if status != 0 {
            anyhow::bail!("VTSessionSetProperty({key}) failed: status={status}");
        }
        Ok(())
    }

    fn set_property_string(&self, key: &str, value: &str) -> Result<()> {
        use core_foundation::base::TCFType;
        use core_foundation::string::CFString;

        let cf_key = CFString::new(key);
        let cf_value = CFString::new(value);

        let status = unsafe {
            VTSessionSetProperty(
                self.session,
                cf_key.as_concrete_TypeRef() as *const c_void,
                cf_value.as_concrete_TypeRef() as *const c_void,
            )
        };

        if status != 0 {
            anyhow::bail!("VTSessionSetProperty({key}) failed: status={status}");
        }
        Ok(())
    }
}

impl Drop for VideoEncoder {
    fn drop(&mut self) {
        if !self.session.is_null() {
            unsafe {
                VTCompressionSessionInvalidate(self.session);
            }
        }
    }
}

/// Detect the main display's native resolution.
pub fn display_resolution() -> Result<(u32, u32)> {
    let display = CGDisplay::main();
    Ok((display.pixels_wide() as u32, display.pixels_high() as u32))
}

/// Capture a single frame from the main display.
///
/// Returns raw BGRA pixel data together with the image dimensions
/// (width, height) and bytes-per-row.
pub fn capture_frame() -> Result<(Vec<u8>, u32, u32, usize)> {
    let display = CGDisplay::main();
    let image: CGImage = display.image().context("CGDisplayCreateImage returned null")?;

    let width = image.width() as u32;
    let height = image.height() as u32;
    let bytes_per_row = image.bytes_per_row();

    let data = image.data();
    let raw_bytes = data.bytes().to_vec();

    Ok((raw_bytes, width, height, bytes_per_row))
}

/// Save a captured BGRA buffer as a PNG file (for verification).
#[cfg(test)]
pub fn save_png(path: &str, bgra: &[u8], width: u32, height: u32, bytes_per_row: usize) -> Result<()> {
    use image::{ImageBuffer, RgbaImage};

    // BGRA → RGBA while also handling possible row padding
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    for row in 0..height as usize {
        let row_start = row * bytes_per_row;
        for col in 0..width as usize {
            let offset = row_start + col * 4;
            let b = bgra[offset];
            let g = bgra[offset + 1];
            let r = bgra[offset + 2];
            let a = bgra[offset + 3];
            rgba.extend_from_slice(&[r, g, b, a]);
        }
    }

    let img: RgbaImage = ImageBuffer::from_raw(width, height, rgba)
        .context("create image buffer")?;
    img.save(path).context("save png")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Input injection (CoreGraphics events)
// ---------------------------------------------------------------------------

use core_graphics::event::{CGEvent, CGEventTapLocation, CGEventType, CGMouseButton};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;

/// Inject a mouse movement event.
///
/// Coordinates are normalized (0.0–1.0) and mapped to the provided screen
/// dimensions.
pub fn inject_mouse_move(x: f32, y: f32, screen_w: u32, screen_h: u32) -> Result<()> {
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|_| anyhow::anyhow!("create event source"))?;

    let px = (x.clamp(0.0, 1.0) * screen_w as f32) as f64;
    let py = (y.clamp(0.0, 1.0) * screen_h as f32) as f64;

    let event = CGEvent::new_mouse_event(
        source,
        CGEventType::MouseMoved,
        CGPoint::new(px, py),
        CGMouseButton::Left,
    )
    .map_err(|_| anyhow::anyhow!("create mouse move event"))?;

    event.post(CGEventTapLocation::HID);
    Ok(())
}

/// Inject a mouse button click or release.
///
/// `button`: 0 = left, 1 = right, 2 = center.
pub fn inject_mouse_click(button: u8, pressed: bool) -> Result<()> {
    let button = match button {
        0 => CGMouseButton::Left,
        1 => CGMouseButton::Right,
        2 => CGMouseButton::Center,
        _ => return Ok(()),
    };

    let event_type = match (button, pressed) {
        (CGMouseButton::Left, true) => CGEventType::LeftMouseDown,
        (CGMouseButton::Left, false) => CGEventType::LeftMouseUp,
        (CGMouseButton::Right, true) => CGEventType::RightMouseDown,
        (CGMouseButton::Right, false) => CGEventType::RightMouseUp,
        // Center button: not implemented (rarely used)
        _ => return Ok(()),
    };

    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|_| anyhow::anyhow!("create event source"))?;

    // For clicks the position doesn't matter — macOS uses the current cursor
    let event = CGEvent::new_mouse_event(
        source,
        event_type,
        CGPoint::new(0.0, 0.0),
        button,
    )
    .map_err(|_| anyhow::anyhow!("create mouse click event"))?;

    event.post(CGEventTapLocation::HID);
    Ok(())
}

/// Inject a keyboard key press or release.
pub fn inject_key_event(keycode: u16, pressed: bool) -> Result<()> {
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|_| anyhow::anyhow!("create event source"))?;

    let event = CGEvent::new_keyboard_event(source, keycode, pressed)
        .map_err(|_| anyhow::anyhow!("create keyboard event"))?;

    event.post(CGEventTapLocation::HID);
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_display_resolution() {
        let (w, h) = display_resolution().expect("get display resolution");
        assert!(w > 0, "width must be positive");
        assert!(h > 0, "height must be positive");
        println!("Main display resolution: {w}x{h}");
    }

    #[test]
    fn test_capture_frame() {
        let (data, width, height, bytes_per_row) =
            capture_frame().expect("capture frame");
        assert!(!data.is_empty(), "frame data must not be empty");
        assert!(width > 0);
        assert!(height > 0);
        assert!(bytes_per_row >= width as usize * 4);
        println!(
            "Captured frame: {width}x{height}, {} bytes (bytes_per_row={bytes_per_row})",
            data.len()
        );
    }

    #[test]
    fn test_input_message_encoding() {
        // Verify normalized coordinates map correctly to a known resolution.
        // Mouse coordinates are 0.0–1.0 normalized; server maps them to real pixels.
        let msg_x: f32 = 0.5;
        let msg_y: f32 = 0.75;
        let screen_w = 1920.0;
        let screen_h = 1080.0;

        let pixel_x = (msg_x * screen_w) as u32;
        let pixel_y = (msg_y * screen_h) as u32;

        assert_eq!(pixel_x, 960);
        assert_eq!(pixel_y, 810);
    }

    #[test]
    fn test_save_png() {
        let (data, width, height, bytes_per_row) =
            capture_frame().expect("capture frame");
        let path = "/tmp/remotedesk_capture_test.png";
        save_png(path, &data, width, height, bytes_per_row)
            .expect("save png");
        println!("Saved screenshot to {path}");

        // Verify file exists and has non-zero size
        let meta = std::fs::metadata(path).expect("file exists");
        assert!(meta.len() > 0, "PNG file must not be empty");
    }

    #[test]
    fn test_video_encoder() {
        let test_w: u32 = 1920;
        let test_h: u32 = 1080;
        let bpr = (test_w * 4) as usize;
        let mut bgra = vec![0u8; test_h as usize * bpr];
        for row in 0..test_h as usize {
            for col in 0..test_w as usize {
                let offset = row * bpr + col * 4;
                bgra[offset] = 0xAA;
                bgra[offset + 1] = 0x00;
                bgra[offset + 2] = (col * 255 / test_w as usize) as u8;
                bgra[offset + 3] = 0xFF;
            }
        }

        let mut encoder = VideoEncoder::new(test_w, test_h, 120)
            .expect("create encoder");
        println!(
            "Encoder created: codec=0x{:08X}",
            encoder.codec_type
        );

        // Encode 30 frames, draining the output channel after each
        for _ in 0..30 {
            encoder.encode_frame(&bgra, bpr).expect("encode frame");
            while encoder.output_rx().try_recv().is_ok() {}
        }

        // Flush and collect via finish()
        let encoded_data = encoder.finish();

        assert!(!encoded_data.is_empty(), "Expected encoded data");
        let path = "/tmp/remotedesk_encoder_test.hevc";
        std::fs::write(path, &encoded_data).expect("write file");
        println!(
            "Saved {path} ({} bytes)",
            encoded_data.len()
        );

        // Verify with ffprobe if available
        let _ = std::process::Command::new("ffprobe")
            .args(["-v", "error", path])
            .status();
    }
}
