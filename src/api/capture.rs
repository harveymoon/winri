//! Win32 window snapshot → PNG bytes. Used by `GET /windows/{id}/thumbnail`.
//!
//! We use `PrintWindow` with `PW_RENDERFULLCONTENT` rather than `BitBlt`ing
//! the screen DC, so the capture is independent of whether the window is
//! currently visible/occluded — important because winri moves source
//! windows offscreen during overview mode.

use std::io::Cursor;

use anyhow::{Context, anyhow};
use windows::Win32::{
    Foundation::{HWND, RECT},
    Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleBitmap, CreateCompatibleDC,
        DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, HBITMAP, HDC, HGDIOBJ,
        ReleaseDC, SelectObject,
    },
    Storage::Xps::{PRINT_WINDOW_FLAGS, PrintWindow},
    UI::WindowsAndMessaging::GetClientRect,
};

/// Capture the source window's client area and return PNG-encoded bytes.
///
/// If `max_width` is provided and is smaller than the captured width, the
/// image is downsampled (nearest-neighbor, aspect preserved) before
/// PNG-encoding. Otherwise it's returned at the window's native resolution.
pub fn capture_window_png(hwnd_raw: u64, max_width: Option<u32>) -> anyhow::Result<Vec<u8>> {
    let hwnd = HWND(hwnd_raw as *mut std::ffi::c_void);

    let mut rect = RECT::default();
    unsafe { GetClientRect(hwnd, &raw mut rect) }
        .context("GetClientRect failed (window may be gone)")?;
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;
    if width <= 0 || height <= 0 {
        return Err(anyhow!("window has zero/negative client area: {width}x{height}"));
    }

    // SAFETY: the unsafe blocks below are scoped per-call so RAII guards
    // below clean up GDI handles even on early return.
    let src_dc = unsafe { GetDC(Some(hwnd)) };
    if src_dc.0.is_null() {
        return Err(anyhow!("GetDC failed"));
    }
    let _src_dc_guard = ReleaseDcGuard {
        hwnd: Some(hwnd),
        dc: src_dc,
    };

    let mem_dc: HDC = unsafe { CreateCompatibleDC(Some(src_dc)) };
    if mem_dc.0.is_null() {
        return Err(anyhow!("CreateCompatibleDC failed"));
    }
    let _mem_dc_guard = DeleteDcGuard { dc: mem_dc };

    let bitmap: HBITMAP = unsafe { CreateCompatibleBitmap(src_dc, width, height) };
    if bitmap.0.is_null() {
        return Err(anyhow!("CreateCompatibleBitmap failed"));
    }
    let _bitmap_guard = DeleteObjectGuard {
        obj: HGDIOBJ(bitmap.0),
    };

    let prev = unsafe { SelectObject(mem_dc, HGDIOBJ(bitmap.0)) };
    if prev.0.is_null() {
        return Err(anyhow!("SelectObject failed"));
    }

    // PW_RENDERFULLCONTENT == 0x00000002 — render even occluded / DWM-cached
    // content. Not defined as a constant in windows-rs 0.62, so we construct
    // the flag value inline.
    const PW_RENDERFULLCONTENT: PRINT_WINDOW_FLAGS = PRINT_WINDOW_FLAGS(0x0000_0002);
    let ok = unsafe { PrintWindow(hwnd, mem_dc, PW_RENDERFULLCONTENT) };
    if !ok.as_bool() {
        return Err(anyhow!(
            "PrintWindow returned false (some apps refuse capture, e.g. UWP without permissions)"
        ));
    }

    // Restore the original DC selection before extracting pixels.
    unsafe { SelectObject(mem_dc, prev) };

    let pixel_count = (width as usize) * (height as usize);
    let mut pixels = vec![0u8; pixel_count * 4]; // BGRA

    let mut info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: u32::try_from(std::mem::size_of::<BITMAPINFOHEADER>()).unwrap_or(0),
            biWidth: width,
            // Negative height: top-down DIB.
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };

    let copied = unsafe {
        GetDIBits(
            mem_dc,
            bitmap,
            0,
            u32::try_from(height).unwrap_or(0),
            Some(pixels.as_mut_ptr().cast()),
            &raw mut info,
            DIB_RGB_COLORS,
        )
    };
    if copied == 0 {
        return Err(anyhow!("GetDIBits failed"));
    }

    // Convert BGRA → RGBA for PNG, and force alpha=255 (GDI doesn't write
    // alpha consistently and we want an opaque snapshot anyway).
    for chunk in pixels.chunks_exact_mut(4) {
        chunk.swap(0, 2);
        chunk[3] = 255;
    }

    let src_w = u32::try_from(width).unwrap_or(0);
    let src_h = u32::try_from(height).unwrap_or(0);

    let (pixels, out_w, out_h) = match max_width {
        Some(target_w) if target_w > 0 && target_w < src_w => {
            let (resized, h) = downsample_rgba_nearest(&pixels, src_w, src_h, target_w);
            (resized, target_w, h)
        }
        _ => (pixels, src_w, src_h),
    };

    let mut out = Vec::with_capacity(((out_w * out_h) / 4) as usize); // ~10% of raw
    {
        let mut encoder = png::Encoder::new(Cursor::new(&mut out), out_w, out_h);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .context("PNG header write failed")?;
        writer
            .write_image_data(&pixels)
            .context("PNG image data write failed")?;
    }

    Ok(out)
}

/// Nearest-neighbor downsample of an RGBA buffer. Aspect ratio is preserved
/// by deriving the target height from the source's. Plenty fine for
/// thumbnail use; if anyone needs better quality (anti-aliased downscale)
/// we'd swap in a proper box-filter pass.
fn downsample_rgba_nearest(src: &[u8], src_w: u32, src_h: u32, dst_w: u32) -> (Vec<u8>, u32) {
    debug_assert!(src.len() == (src_w as usize) * (src_h as usize) * 4);
    debug_assert!(dst_w > 0);

    #[allow(clippy::cast_precision_loss)]
    let scale = src_w as f32 / dst_w as f32;
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let dst_h = (((src_h as f32) / scale) as u32).max(1);

    let mut dst = vec![0u8; (dst_w as usize) * (dst_h as usize) * 4];
    for y in 0..dst_h {
        for x in 0..dst_w {
            #[allow(
                clippy::cast_precision_loss,
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss
            )]
            let src_x = (((x as f32) * scale) as u32).min(src_w - 1);
            #[allow(
                clippy::cast_precision_loss,
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss
            )]
            let src_y = (((y as f32) * scale) as u32).min(src_h - 1);
            let src_idx = ((src_y * src_w + src_x) as usize) * 4;
            let dst_idx = ((y * dst_w + x) as usize) * 4;
            dst[dst_idx..dst_idx + 4].copy_from_slice(&src[src_idx..src_idx + 4]);
        }
    }
    (dst, dst_h)
}

struct DeleteDcGuard {
    dc: HDC,
}

impl Drop for DeleteDcGuard {
    fn drop(&mut self) {
        if !self.dc.0.is_null() {
            let _ = unsafe { DeleteDC(self.dc) };
        }
    }
}

struct DeleteObjectGuard {
    obj: HGDIOBJ,
}

impl Drop for DeleteObjectGuard {
    fn drop(&mut self) {
        if !self.obj.0.is_null() {
            let _ = unsafe { DeleteObject(self.obj) };
        }
    }
}

struct ReleaseDcGuard {
    hwnd: Option<HWND>,
    dc: HDC,
}

impl Drop for ReleaseDcGuard {
    fn drop(&mut self) {
        if !self.dc.0.is_null() {
            let _ = unsafe { ReleaseDC(self.hwnd, self.dc) };
        }
    }
}
