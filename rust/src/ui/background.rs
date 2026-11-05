//! Native tray uses the same callbacks as the primary window. No runtime lives here.
use crate::{AppWindow, BackgroundTray, VoiceHud};
use slint::ComponentHandle;

pub struct BackgroundUi {
    _tray: BackgroundTray,
    _hud: VoiceHud,
    _timer: slint::Timer,
}

impl BackgroundUi {
    pub fn new(window: &AppWindow) -> Result<Self, slint::PlatformError> {
        let tray = BackgroundTray::new()?;
        let hud = super::hud_window::create()?;
        let manual = std::rc::Rc::new(std::cell::Cell::new(false));
        let placement = std::rc::Rc::new(super::hud_window::Placement::default());
        let weak = window.as_weak();
        hud.on_answer(move |id, allow| {
            if let Some(w) = weak.upgrade() {
                w.invoke_confirm_decision(id, allow);
            }
        });
        let weak = window.as_weak();
        hud.on_open_main(move || {
            if let Some(w) = weak.upgrade() {
                activate_main(&w);
            }
        });
        let weak = window.as_weak();
        hud.on_stop_speech(move || {
            if let Some(w) = weak.upgrade() {
                w.invoke_stop_speaking();
            }
        });
        let weak = window.as_weak();
        hud.on_cancel_task(move || {
            if let Some(w) = weak.upgrade() {
                w.invoke_cancel_turn();
            }
        });
        let h = hud.as_weak();
        let manual_hide = manual.clone();
        hud.on_hide_hud(move || {
            manual_hide.set(false);
            if let Some(h) = h.upgrade() {
                let _ = h.hide();
            }
        });
        hud.window()
            .on_close_requested(|| slint::CloseRequestResponse::KeepWindowShown);

        tray.set_login_start(super::autostart::enabled());
        let weak = window.as_weak();
        let t = tray.as_weak();
        tray.on_toggle_login(move || {
            let result = super::autostart::set(!super::autostart::enabled());
            if let Some(t) = t.upgrade() {
                t.set_login_start(super::autostart::enabled());
            }
            if let (Err(error), Some(w)) = (result, weak.upgrade()) {
                w.set_status(error.into());
            }
        });
        let h = hud.as_weak();
        let manual_toggle = manual.clone();
        let place = placement.clone();
        tray.on_toggle_hud(move || {
            if let Some(h) = h.upgrade() {
                if h.window().is_visible() && !h.get_asking() {
                    manual_toggle.set(false);
                    let _ = h.hide();
                } else {
                    manual_toggle.set(true);
                    place.show(&h);
                }
            }
        });
        let weak = window.as_weak();
        tray.on_open_main(move || {
            if let Some(w) = weak.upgrade() {
                activate_main(&w);
            }
        });
        let weak = window.as_weak();
        tray.on_settings(move || {
            if let Some(w) = weak.upgrade() {
                activate_main(&w);
                w.invoke_open_settings();
            }
        });
        let weak = window.as_weak();
        tray.on_toggle_microphone(move || {
            if let Some(w) = weak.upgrade() {
                w.invoke_set_voice_enabled(!w.get_voice_enabled());
            }
        });
        let weak = window.as_weak();
        tray.on_stop_speech(move || {
            if let Some(w) = weak.upgrade() {
                w.invoke_stop_speaking();
            }
        });
        let weak = window.as_weak();
        tray.on_quit_app(move || {
            if let Some(w) = weak.upgrade() {
                w.invoke_quit_app();
            }
        });
        window
            .window()
            .on_close_requested(|| slint::CloseRequestResponse::HideWindow);
        let timer = slint::Timer::default();
        let weak = window.as_weak();
        let tray_weak = tray.as_weak();
        let h = hud.as_weak();
        let last_wake = std::cell::Cell::new(0);
        let idle_since = std::cell::Cell::new(std::time::Instant::now());
        timer.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_millis(100),
            move || {
                if let (Some(w), Some(t)) = (weak.upgrade(), tray_weak.upgrade()) {
                    t.set_microphone(w.get_voice_enabled());
                    if let Some(h) = h.upgrade() {
                        let woke =
                            last_wake.replace(w.get_wake_generation()) != w.get_wake_generation();
                        h.set_status(w.get_status());
                        h.set_transcript(w.get_voice_transcript());
                        h.set_subtitle(w.get_speech_subtitle());
                        h.set_asking(w.get_confirm_open());
                        h.set_request_id(w.get_confirm_request_id());
                        h.set_prompt(
                            format!("{}：{}", w.get_confirm_label(), w.get_confirm_reason()).into(),
                        );
                        if h.window().is_visible()
                            || should_open(woke, w.get_voice_enabled(), w.get_confirm_open())
                        {
                            placement.show(&h);
                        }
                        if keeps_auto_hud_active(
                            woke,
                            w.get_voice_enabled(),
                            w.get_voice_state().as_str(),
                            w.get_busy(),
                            w.get_speech_active(),
                            w.get_confirm_open(),
                        ) {
                            idle_since.set(std::time::Instant::now());
                        } else if should_hide(
                            manual.get(),
                            w.get_confirm_open(),
                            w.get_voice_enabled(),
                            idle_since.get().elapsed().as_secs(),
                        ) {
                            let _ = h.hide();
                        }
                    }
                }
            },
        );
        Ok(Self {
            _tray: tray,
            _hud: hud,
            _timer: timer,
        })
    }
}

// Only an accepted wake or a live ask may open an automatically hidden HUD.
fn should_open(accepted_wake: bool, microphone_active: bool, pending_ask: bool) -> bool {
    pending_ask || (accepted_wake && microphone_active)
}

#[cfg(test)]
mod tests {
    use super::should_open;
    #[test]
    fn late_wake_after_microphone_shutdown_cannot_open_hud() {
        assert!(!should_open(true, false, false));
    }
    #[test]
    fn pending_ask_remains_accessible_without_microphone() {
        assert!(should_open(false, false, true));
    }
    #[test]
    fn ordinary_status_or_subtitle_update_cannot_reopen_hidden_hud() {
        assert!(!should_open(false, true, false));
        assert!(should_open(true, true, false));
    }
}

fn should_hide(manual: bool, asking: bool, microphone: bool, idle_seconds: u64) -> bool {
    !manual && !asking && (!microphone || idle_seconds >= 4)
}
fn keeps_auto_hud_active(
    woke: bool,
    microphone: bool,
    voice_state: &str,
    busy: bool,
    speech: bool,
    asking: bool,
) -> bool {
    asking
        || (microphone
            && (woke || matches!(voice_state, "capturing" | "transcribing") || busy || speech))
}
#[cfg(test)]
mod visibility_tests {
    use super::*;
    #[test]
    fn long_capture_and_transcription_only_expire_after_completion_or_failure() {
        for terminal in ["listening", "error"] {
            let mut last_active = 0;
            for second in 0..30 {
                let state = match second {
                    0..=9 => "capturing",
                    10..=19 => "transcribing",
                    _ => terminal,
                };
                if keeps_auto_hud_active(second == 0, true, state, false, false, false) {
                    last_active = second;
                }
                assert_eq!(
                    should_hide(false, false, true, second - last_active),
                    second >= 23
                );
            }
        }
    }
    #[test]
    fn agent_speech_and_ask_keep_active_but_microphone_off_semantics_remain() {
        assert!(keeps_auto_hud_active(
            false,
            true,
            "listening",
            true,
            false,
            false
        ));
        assert!(keeps_auto_hud_active(
            false,
            true,
            "listening",
            false,
            true,
            false
        ));
        assert!(keeps_auto_hud_active(
            false, false, "off", false, false, true
        ));
        assert!(!keeps_auto_hud_active(
            false,
            false,
            "capturing",
            true,
            true,
            false
        ));
        assert!(should_hide(false, false, false, 0));
    }
    #[test]
    fn manual_survives_idle_and_microphone_off() {
        assert!(!should_hide(true, false, false, 20));
    }
    #[test]
    fn ask_survives_timeout() {
        assert!(!should_hide(false, true, false, 20));
    }
    #[test]
    fn auto_expires() {
        assert!(should_hide(false, false, true, 4));
        assert!(!should_hide(false, false, true, 3));
    }
}

/// Explicit user navigation only. Automatic HUD reveals must never call this.
pub(super) fn activate_main(window: &AppWindow) {
    use slint::winit_030::WinitWindowAccessor;
    if let Err(error) = window.show() {
        window.set_status(format!("无法打开主窗口：{error}").into());
        return;
    }
    window.window().with_winit_window(|native| {
        native.set_minimized(false);
        native.focus_window();
    });
}
#[cfg(all(test, windows))]
mod navigation_tests {
    use super::*;
    use slint::winit_030::WinitWindowAccessor;
    #[test]
    #[ignore = "requires interactive Windows desktop; briefly focuses a test main window"]
    fn explicit_navigation_restores_and_focuses_main() {
        slint::BackendSelector::new()
            .backend_name("winit".into())
            .select()
            .unwrap();
        let window = AppWindow::new().unwrap();
        window.show().unwrap();
        let timer = slint::Timer::default();
        let step = std::cell::Cell::new(0);
        timer.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_millis(500),
            move || {
                match step.get() {
                    0 => {
                        window.window().with_winit_window(|w| w.set_minimized(true));
                    }
                    1 => activate_main(&window),
                    2 => {
                        window.window().with_winit_window(|w| {
                            assert_ne!(w.is_minimized(), Some(true));
                            assert!(
                                w.has_focus(),
                                "explicit main navigation must activate the window"
                            );
                        });
                        activate_main(&window);
                    }
                    _ => {
                        window.hide().unwrap();
                        slint::quit_event_loop().unwrap();
                    }
                }
                step.set(step.get() + 1);
            },
        );
        slint::run_event_loop_until_quit().unwrap();
    }
}
