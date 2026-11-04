//! HUD-only borrowed native handle adaptation; see docs/14 for safety rationale.
use crate::VoiceHud;
use slint::ComponentHandle;

pub fn show(hud: &VoiceHud) {
    #[cfg(windows)]
    {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        use slint::winit_030::WinitWindowAccessor;
        use windows::Win32::{
            Foundation::HWND,
            Graphics::Gdi::{
                GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTOPRIMARY,
            },
            UI::WindowsAndMessaging::{
                GetForegroundWindow, SetWindowPos, HWND_TOPMOST, SWP_NOACTIVATE,
            },
        };
        // Ensure the native window exists using Slint's inactive creation hook.
        if !hud.window().is_visible() {
            let _ = hud.show();
        }
        hud.window().with_winit_window(|w| {
            let Ok(handle) = w.window_handle() else {
                return;
            };
            let RawWindowHandle::Win32(handle) = handle.as_raw() else {
                return;
            };
            let hwnd = HWND(handle.hwnd.get() as *mut _);
            // SAFETY: borrowed live HWND, UI thread only, no pointer retained.
            unsafe {
                let monitor = MonitorFromWindow(GetForegroundWindow(), MONITOR_DEFAULTTOPRIMARY);
                let mut info = MONITORINFO {
                    cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                    ..Default::default()
                };
                if GetMonitorInfoW(monitor, &mut info).as_bool() {
                    let area = info.rcWork;
                    let size = w.outer_size();
                    let margin = (16.0 * w.scale_factor()) as i32;
                    let width =
                        (size.width as i32).min((area.right - area.left - margin * 2).max(1));
                    let height =
                        (size.height as i32).min((area.bottom - area.top - margin * 2).max(1));
                    let x = (area.right - width - margin).max(area.left);
                    let y = (area.bottom - height - margin).max(area.top);
                    let _ = SetWindowPos(
                        hwnd,
                        Some(HWND_TOPMOST),
                        x,
                        y,
                        width,
                        height,
                        SWP_NOACTIVATE,
                    );
                }
            }
        });
    }
    #[cfg(not(windows))]
    {
        let _ = hud.show();
    }
}
