//! Windows taskbar overlay, drawn at the shell's recommended 16 × 16 pixels.

#[cfg(any(windows, test))]
fn badge_text(count: usize) -> Option<&'static str> {
    match count {
        0 => None,
        1..=9 => Some(["1", "2", "3", "4", "5", "6", "7", "8", "9"][count - 1]),
        _ => Some("9+"),
    }
}

#[cfg(windows)]
pub(crate) use windows_badge::Badge;

#[cfg(not(windows))]
#[derive(Default)]
pub(crate) struct Badge {}

#[cfg(not(windows))]
impl Badge {
    pub(crate) fn update(&mut self, _: &eframe::Frame, _: usize) {}
}

#[cfg(windows)]
mod windows_badge {
    use super::badge_text;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use std::time::{Duration, Instant};
    use windows::{
        Win32::{
            Foundation::{HWND, RPC_E_CHANGED_MODE},
            System::Com::{
                CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
                CoUninitialize,
            },
            UI::{
                Shell::{ITaskbarList3, TaskbarList},
                WindowsAndMessaging::{
                    CreateIconFromResourceEx, DestroyIcon, HICON, LR_DEFAULTCOLOR,
                },
            },
        },
        core::{HSTRING, PCWSTR, Result},
    };

    #[derive(Default)]
    pub(crate) struct Badge {
        count: usize,
        refresh_at: Option<Instant>,
    }

    impl Badge {
        pub(crate) fn update(&mut self, frame: &eframe::Frame, count: usize) {
            let now = Instant::now();
            if self.count == count && self.refresh_at.is_none_or(|at| now < at) {
                return;
            }
            let Ok(handle) = frame.window_handle() else {
                return;
            };
            let RawWindowHandle::Win32(handle) = handle.as_raw() else {
                return;
            };
            let hwnd = HWND(handle.hwnd.get() as *mut std::ffi::c_void);
            self.count = count;
            // Refresh an active overlay in case Explorer recreates the taskbar button.
            self.refresh_at = match set_overlay(hwnd, count) {
                Ok(()) if count == 0 => None,
                Ok(()) => Some(now + Duration::from_secs(10)),
                Err(_) => Some(now + Duration::from_secs(1)),
            };
        }
    }

    struct Com(bool);

    impl Com {
        fn init() -> Result<Self> {
            let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
            if hr == RPC_E_CHANGED_MODE {
                return Ok(Self(false));
            }
            hr.ok()?;
            Ok(Self(true))
        }
    }

    impl Drop for Com {
        fn drop(&mut self) {
            if self.0 {
                unsafe { CoUninitialize() };
            }
        }
    }

    fn set_overlay(hwnd: HWND, count: usize) -> Result<()> {
        let _com = Com::init()?;
        unsafe {
            let taskbar: ITaskbarList3 =
                CoCreateInstance(&TaskbarList, None, CLSCTX_INPROC_SERVER)?;
            taskbar.HrInit()?;
            let Some(text) = badge_text(count) else {
                return taskbar.SetOverlayIcon(hwnd, HICON::default(), PCWSTR::null());
            };
            let pixels = icon_resource(text);
            let icon =
                CreateIconFromResourceEx(&pixels.0, true, 0x0003_0000, 16, 16, LR_DEFAULTCOLOR)?;
            let description = HSTRING::from(format!("{count} 个工单需要查看"));
            let result = taskbar.SetOverlayIcon(hwnd, icon, &description);
            let _ = DestroyIcon(icon);
            result
        }
    }

    const SIDE: usize = 16;
    const HEADER_SIZE: usize = 40;
    const MASK_START: usize = HEADER_SIZE + SIDE * SIDE * 4;

    // A 32-bit icon DIB followed by the 1-bit AND mask (DWORD-aligned rows).
    #[repr(align(4))]
    struct IconResource([u8; MASK_START + SIDE * 4]);

    fn icon_resource(text: &str) -> IconResource {
        let mut resource = IconResource([0; MASK_START + SIDE * 4]);
        let bytes = &mut resource.0;
        bytes[0..4].copy_from_slice(&(HEADER_SIZE as u32).to_le_bytes());
        bytes[4..8].copy_from_slice(&(SIDE as u32).to_le_bytes());
        // The resource's height includes both the color bitmap and its mask.
        bytes[8..12].copy_from_slice(&(2 * SIDE as u32).to_le_bytes());
        bytes[12..14].copy_from_slice(&1u16.to_le_bytes());
        bytes[14..16].copy_from_slice(&32u16.to_le_bytes());
        for y in 0..SIDE {
            for x in 0..SIDE {
                let dx = x.abs_diff(7).min(x.abs_diff(8)).saturating_sub(3);
                let dy = y.abs_diff(7).min(y.abs_diff(8)).saturating_sub(3);
                if dx * dx + dy * dy <= 16 {
                    pixel(bytes, x, y).copy_from_slice(&[0x42, 0x26, 0xa1, 0xff]);
                } else {
                    let row = SIDE - 1 - y;
                    bytes[MASK_START + row * 4 + x / 8] |= 0x80 >> (x % 8);
                }
            }
        }
        let start = if text.len() == 1 { 5 } else { 2 };
        for (index, glyph) in text.bytes().enumerate() {
            for (y, row) in glyph_rows(glyph).into_iter().enumerate() {
                for x in 0..5 {
                    if row & (0b10000 >> x) != 0 {
                        pixel(bytes, start + index * 7 + x, 4 + y).fill(0xff);
                    }
                }
            }
        }
        resource
    }

    fn pixel(bytes: &mut [u8], x: usize, y: usize) -> &mut [u8] {
        let offset = HEADER_SIZE + (SIDE * (SIDE - 1 - y) + x) * 4;
        &mut bytes[offset..offset + 4]
    }

    fn glyph_rows(glyph: u8) -> [u8; 7] {
        match glyph {
            b'1' => [4, 12, 4, 4, 4, 4, 14],
            b'2' => [14, 17, 1, 2, 4, 8, 31],
            b'3' => [30, 1, 1, 14, 1, 1, 30],
            b'4' => [2, 6, 10, 18, 31, 2, 2],
            b'5' => [31, 16, 16, 30, 1, 1, 30],
            b'6' => [6, 8, 16, 30, 17, 17, 14],
            b'7' => [31, 1, 2, 4, 8, 8, 8],
            b'8' => [14, 17, 17, 14, 17, 17, 14],
            b'9' => [14, 17, 17, 15, 1, 2, 12],
            b'+' => [0, 0, 4, 31, 4, 0, 0],
            _ => unreachable!(),
        }
    }
}

#[cfg(test)]
#[test]
fn badge_display_caps_at_nine_plus_and_hides_at_zero() {
    assert_eq!(badge_text(0), None);
    for (count, glyph) in (1..=9).zip(["1", "2", "3", "4", "5", "6", "7", "8", "9"]) {
        assert_eq!(badge_text(count), Some(glyph));
    }
    assert_eq!(badge_text(10), Some("9+"));
    assert_eq!(badge_text(123), Some("9+"));
}
