//! HUD-only borrowed native handle adaptation; see docs/14 for safety rationale.
use crate::VoiceHud;
use slint::ComponentHandle;

thread_local! { static CREATING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
pub fn creating_hud() -> bool {
    CREATING.with(|v| v.get())
}

type Geometry = (i32, i32, i32, i32, u64);
#[derive(Default)]
pub struct Placement(std::cell::Cell<Option<Geometry>>);
impl Placement {
    pub fn show(&self, hud: &VoiceHud) {
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
                    GetForegroundWindow, GetWindowLongPtrW, SetWindowLongPtrW, SetWindowPos,
                    GWL_EXSTYLE, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOZORDER, WS_EX_NOACTIVATE,
                },
            };
            let opening = !hud.window().is_visible();
            if opening {
                self.0.set(None);
                // Existing HWND: guard the Slint/winit SW_SHOW call, then restore click activation.
                let style = hud
                    .window()
                    .with_winit_window(|w| {
                        let RawWindowHandle::Win32(handle) = w.window_handle().ok()?.as_raw()
                        else {
                            return None;
                        };
                        let hwnd = HWND(handle.hwnd.get() as *mut _);
                        // SAFETY: UI-thread borrowed live window; only its activation style is changed.
                        let old = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) };
                        unsafe {
                            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, old | WS_EX_NOACTIVATE.0 as isize);
                        }
                        Some(old)
                    })
                    .flatten();
                struct Reset;
                impl Drop for Reset {
                    fn drop(&mut self) {
                        CREATING.with(|v| v.set(false));
                    }
                }
                CREATING.with(|v| v.set(true));
                let reset = Reset;
                let result = hud.show();
                drop(reset);
                if let Some(old) = style {
                    hud.window().with_winit_window(|w| {
                        if let Ok(handle) = w.window_handle() {
                            if let RawWindowHandle::Win32(handle) = handle.as_raw() {
                                // SAFETY: reacquired current live HWND on the same UI thread.
                                unsafe {
                                    SetWindowLongPtrW(
                                        HWND(handle.hwnd.get() as *mut _),
                                        GWL_EXSTYLE,
                                        old,
                                    );
                                }
                            }
                        }
                    });
                }
                if result.is_err() {
                    return;
                }
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
                    let monitor = MonitorFromWindow(
                        if opening { GetForegroundWindow() } else { hwnd },
                        MONITOR_DEFAULTTOPRIMARY,
                    );
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
                        let geometry = (x, y, width, height, w.scale_factor().to_bits());
                        if self.0.replace(Some(geometry)) == Some(geometry) {
                            return;
                        }
                        let _ = SetWindowPos(
                            hwnd,
                            Some(HWND_TOPMOST),
                            x,
                            y,
                            width,
                            height,
                            SWP_NOACTIVATE
                                | if opening {
                                    Default::default()
                                } else {
                                    SWP_NOZORDER
                                },
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
}
#[cfg(all(test, windows))]
mod native_tests {
    use super::*;
    #[test]
    #[ignore = "requires interactive Windows desktop with a foreground editor"]
    fn native_first_and_retained_show_preserve_foreground() {
        use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
        slint::BackendSelector::new()
            .backend_name("winit".into())
            .with_winit_window_attributes_hook(|a| {
                if creating_hud() {
                    a.with_active(false)
                } else {
                    a
                }
            })
            .select()
            .unwrap();
        let hud = VoiceHud::new().unwrap();
        let placement = Placement::default();
        let timer = slint::Timer::default();
        let step = std::cell::Cell::new(0);
        timer.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_millis(500),
            move || {
                // SAFETY: observation only, no handle is retained beyond this callback.
                let before = unsafe { GetForegroundWindow() };
                placement.show(&hud);
                let after = unsafe { GetForegroundWindow() };
                assert_eq!(before, after, "HUD activated on step {}", step.get());
                eprintln!(
                    "native HUD step={} foreground={:?} unchanged",
                    step.get(),
                    after
                );
                hud.hide().unwrap();
                step.set(step.get() + 1);
                if step.get() == 3 {
                    slint::quit_event_loop().unwrap();
                }
            },
        );
        slint::run_event_loop_until_quit().unwrap();
    }
}
