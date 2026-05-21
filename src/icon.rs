//! Extract a window's app icon as RGBA pixels suitable for an iced
//! `image::Handle`. Used by the overview view to render small app icons
//! next to thumbnail labels.

use std::ffi::c_void;

use windows::Win32::{
    Foundation::{HWND, LPARAM, WPARAM},
    Graphics::Gdi::{
        BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, DeleteObject, GetDC,
        GetDIBits, GetObjectW, HGDIOBJ, ReleaseDC,
    },
    UI::WindowsAndMessaging::{
        GCLP_HICON, GCLP_HICONSM, GET_CLASS_LONG_INDEX, GetClassLongPtrW, GetIconInfo, HICON,
        ICONINFO, SendMessageW,
    },
};

const WM_GETICON: u32 = 0x007F;
const ICON_SMALL: usize = 0;
const ICON_BIG: usize = 1;
const ICON_SMALL2: usize = 2;

/// Try the standard sources of a window's icon, in quality-descending
/// order. Returns the first non-null HICON, or None if none of them are
/// available (e.g. UWP host windows, system console).
fn fetch_hicon(hwnd_raw: u64) -> Option<HICON> {
    let hwnd = HWND(hwnd_raw as *mut c_void);
    unsafe {
        for icon_type in [ICON_SMALL2, ICON_SMALL, ICON_BIG] {
            let r = SendMessageW(hwnd, WM_GETICON, Some(WPARAM(icon_type)), Some(LPARAM(0)));
            if r.0 != 0 {
                return Some(HICON(r.0 as *mut c_void));
            }
        }
        for nindex in [GCLP_HICONSM, GCLP_HICON] {
            let v = GetClassLongPtrW(hwnd, GET_CLASS_LONG_INDEX(nindex.0));
            if v != 0 {
                return Some(HICON(v as *mut c_void));
            }
        }
    }
    None
}

/// Extract an HICON's pixels and return them as `(width, height, RGBA
/// top-down)`. Returns `None` if the icon's bitmap can't be introspected.
fn hicon_to_rgba(hicon: HICON) -> Option<(u32, u32, Vec<u8>)> {
    let mut info = ICONINFO::default();
    unsafe { GetIconInfo(hicon, &raw mut info) }.ok()?;

    let mut bmp = BITMAP::default();
    #[allow(clippy::cast_possible_truncation)]
    let bmp_size = std::mem::size_of::<BITMAP>() as i32;
    let got = unsafe {
        GetObjectW(
            HGDIOBJ(info.hbmColor.0),
            bmp_size,
            Some(&raw mut bmp as *mut c_void),
        )
    };
    let cleanup = |info: &ICONINFO| {
        if !info.hbmColor.0.is_null() {
            let _ = unsafe { DeleteObject(HGDIOBJ(info.hbmColor.0)) };
        }
        if !info.hbmMask.0.is_null() {
            let _ = unsafe { DeleteObject(HGDIOBJ(info.hbmMask.0)) };
        }
    };
    if got == 0 {
        cleanup(&info);
        return None;
    }

    #[allow(clippy::cast_sign_loss)]
    let width = bmp.bmWidth.max(0) as u32;
    #[allow(clippy::cast_sign_loss)]
    let height = bmp.bmHeight.max(0) as u32;
    if width == 0 || height == 0 {
        cleanup(&info);
        return None;
    }

    let pixel_count = (width as usize) * (height as usize);
    let mut bgra = vec![0u8; pixel_count * 4];

    let mut bi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: u32::try_from(std::mem::size_of::<BITMAPINFOHEADER>()).unwrap_or(0),
            #[allow(clippy::cast_possible_wrap)]
            biWidth: width as i32,
            // Negative height = top-down rows.
            #[allow(clippy::cast_possible_wrap)]
            biHeight: -(height as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };

    let screen_dc = unsafe { GetDC(None) };
    let copied = unsafe {
        GetDIBits(
            screen_dc,
            info.hbmColor,
            0,
            height,
            Some(bgra.as_mut_ptr().cast()),
            &raw mut bi,
            DIB_RGB_COLORS,
        )
    };
    let _ = unsafe { ReleaseDC(None, screen_dc) };
    cleanup(&info);

    if copied == 0 {
        return None;
    }

    // BGRA → RGBA.
    for chunk in bgra.chunks_exact_mut(4) {
        chunk.swap(0, 2);
    }

    // Older icons (16/24-bit) have no alpha channel — every byte is 0.
    // Detect that and assume fully opaque.
    let any_alpha = bgra.chunks_exact(4).any(|p| p[3] != 0);
    if !any_alpha {
        for chunk in bgra.chunks_exact_mut(4) {
            chunk[3] = 255;
        }
    }

    Some((width, height, bgra))
}

/// Convenience entry: fetch an icon for `hwnd_raw` and return its RGBA
/// representation, or `None` if it doesn't expose one.
pub fn fetch_icon_rgba(hwnd_raw: u64) -> Option<(u32, u32, Vec<u8>)> {
    let hicon = fetch_hicon(hwnd_raw)?;
    hicon_to_rgba(hicon)
}
