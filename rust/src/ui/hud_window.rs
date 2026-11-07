//! HUD-only borrowed native handle adaptation; see docs/14 for safety rationale.
use crate::VoiceHud;
use slint::ComponentHandle;

thread_local! { static CREATING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
pub fn creating_hud() -> bool {
    CREATING.with(|v| v.get())
}
// Slint creates its adapter (and captures WindowAttributes) during component new(),
// before show(). Keep this scoped to HUD creation, including unwinding/nesting.
fn with_hud_attributes<T>(f: impl FnOnce() -> T) -> T {
    struct Reset(bool);
    impl Drop for Reset {
        fn drop(&mut self) {
            CREATING.with(|v| v.set(self.0));
        }
    }
    let _reset = Reset(CREATING.with(|v| v.replace(true)));
    f()
}
pub fn create() -> Result<VoiceHud, slint::PlatformError> {
    with_hud_attributes(VoiceHud::new)
}

type Geometry = (i32, i32, i32, i32, u64);
type Viewport = ([i32; 4], u64, u32, u32);
#[derive(Default)]
pub struct Placement {
    geometry: std::cell::Cell<Option<Geometry>>,
    viewport: std::cell::Cell<Option<Viewport>>,
}
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
                    GWL_EXSTYLE, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER,
                    WS_EX_NOACTIVATE,
                },
            };
            let opening = !hud.window().is_visible();
            if opening {
                self.geometry.set(None);
                self.viewport.set(None);
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
                let result = with_hud_attributes(|| hud.show());
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
            let mut resize_to = None;
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
                        let scale = w.scale_factor();
                        let margin = (16.0 * scale) as i32;
                        let viewport = (
                            [area.left, area.top, area.right, area.bottom],
                            scale.to_bits(),
                            hud.get_content_width().to_bits(),
                            hud.get_content_height().to_bits(),
                        );
                        if self.viewport.replace(Some(viewport)) != Some(viewport) {
                            resize_to = Some(slint::LogicalSize::new(
                                hud.get_content_width().min(
                                    ((area.right - area.left - margin * 2).max(1) as f64 / scale)
                                        as f32,
                                ),
                                hud.get_content_height().min(
                                    ((area.bottom - area.top - margin * 2).max(1) as f64 / scale)
                                        as f32,
                                ),
                            ));
                        }
                        let size = w.outer_size();
                        let width =
                            (size.width as i32).min((area.right - area.left - margin * 2).max(1));
                        let height =
                            (size.height as i32).min((area.bottom - area.top - margin * 2).max(1));
                        let x = (area.right - width - margin).max(area.left);
                        let y = (area.bottom - height - margin).max(area.top);
                        let geometry = (x, y, width, height, w.scale_factor().to_bits());
                        if self.geometry.replace(Some(geometry)) == Some(geometry) {
                            return;
                        }
                        let _ = SetWindowPos(
                            hwnd,
                            Some(HWND_TOPMOST),
                            x,
                            y,
                            0,
                            0,
                            SWP_NOACTIVATE
                                | SWP_NOSIZE
                                | if opening {
                                    Default::default()
                                } else {
                                    SWP_NOZORDER
                                },
                        );
                    }
                }
            });
            // Keep logical sizing inside Slint. Native resizing here competes with
            // its ScaleFactorChanged/Resized handling and can clip the button rows.
            if let Some(size) = resize_to {
                hud.window().set_size(size);
            }
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
        let hooks = std::rc::Rc::new(std::cell::Cell::new(0));
        let observed_hooks = hooks.clone();
        slint::BackendSelector::new()
            .backend_name("winit".into())
            .with_winit_window_attributes_hook(move |a| {
                if creating_hud() {
                    observed_hooks.set(observed_hooks.get() + 1);
                    a.with_active(false)
                } else {
                    a
                }
            })
            .select()
            .unwrap();
        let hud = create().unwrap();
        assert_eq!(
            hooks.get(),
            1,
            "HUD attributes must be captured during new()"
        );
        assert!(!creating_hud());
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
                placement.show(&hud); // Already visible: must also preserve foreground.
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

#[cfg(all(test, windows))]
mod pr16_layout_tests {
    use super::*;
    #[test]
    #[ignore = "interactive Windows HUD layout fixture"]
    fn long_content_hud_fixture() {
        slint::BackendSelector::new()
            .backend_name("winit".into())
            .select()
            .unwrap();
        let hud = create().unwrap();
        hud.set_status("等待审批".into());
        hud.set_listening("麦克风偏好：开 · 设备：capturing".into());
        hud.set_transcript("请列出窗口并检查当前文档".into());
        hud.set_reasoning("这是测试夹具中的模型事件文本。\n".repeat(60).into());
        hud.set_reply("这是增量回复测试内容。\n".repeat(40).into());
        hud.set_prompt("允许读取当前窗口标题吗？此操作会列出当前桌面窗口。".into());
        hud.set_asking(true);
        hud.set_request_id(1);
        hud.set_followup("可以继续说，无需唤醒词 · 8 秒".into());
        let placement = Placement::default();
        placement.show(&hud);
        let timer = slint::Timer::default();
        timer.start(
            slint::TimerMode::SingleShot,
            std::time::Duration::from_secs(45),
            || {
                slint::quit_event_loop().unwrap();
            },
        );
        slint::run_event_loop_until_quit().unwrap();
    }
}
