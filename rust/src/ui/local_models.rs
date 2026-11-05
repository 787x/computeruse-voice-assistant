//! 原生文件选择只修改草稿；模型检查复用语音运行时的路径解析。
use crate::{config::AppPaths, AppWindow};
use slint::ComponentHandle;

pub fn wire(window: &AppWindow, paths: AppPaths) {
    let weak = window.as_weak();
    window.on_check_local_models(move || {
        if let Some(window) = weak.upgrade() {
            window.set_local_model_check(check(&window, &paths).into());
        }
    });
    let weak = window.as_weak();
    window.on_choose_local_path(move |kind| {
        let Some(window) = weak.upgrade() else {
            return;
        };
        use slint::winit_030::WinitWindowAccessor;
        let dialog = window.window().with_winit_window(|parent| {
            rfd::FileDialog::new()
                .set_parent(parent)
                .set_title("选择本地模型")
        });
        let Some(dialog) = dialog else {
            return;
        };
        let weak = weak.clone();
        std::thread::spawn(move || {
            let result = match kind {
                0 | 3 => dialog.pick_folder(),
                1 => dialog.add_filter("Whisper", &["bin"]).pick_file(),
                2 => dialog.add_filter("关键词", &["txt"]).pick_file(),
                _ => None,
            }
            .map(|path| path.to_string_lossy().into_owned());
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(window) = weak.upgrade() {
                    if let Some(path) = result {
                        if !window.get_settings_open() {
                            return;
                        }
                        match kind {
                            0 => window.set_kws_dir(path.into()),
                            1 => window.set_whisper_file(path.into()),
                            2 => window.set_keywords_file(path.into()),
                            3 => window.set_tts_local_dir(path.into()),
                            _ => return,
                        }
                        window.set_settings_dirty(true);
                        window.invoke_check_local_models();
                    }
                }
            });
        });
    });
}

fn check(window: &AppWindow, paths: &AppPaths) -> String {
    let voice = crate::voice::settings::VoicePaths::resolve(paths);
    let describe = |label: &str, result: crate::voice::settings::Resolved| match result {
        crate::voice::settings::Resolved::Found(path) => {
            format!("{label}：{}（文件检查通过）", path.display())
        }
        crate::voice::settings::Resolved::Missing(error) => format!("{label}：{error}"),
    };
    let keywords = window.get_keywords_file();
    let keywords_path = if keywords.trim().is_empty() {
        voice.keywords_file()
    } else {
        keywords.as_str().into()
    };
    [
        describe(
            "KWS",
            voice.resolve_kws_model(window.get_kws_dir().as_str()),
        ),
        describe(
            "Whisper",
            voice.resolve_whisper_model(window.get_whisper_file().as_str()),
        ),
        format!(
            "关键词：{}（{}）",
            keywords_path.display(),
            if keywords_path.is_file() {
                "存在"
            } else {
                "文件缺失；启用语音时可生成默认关键词"
            }
        ),
        match crate::voice::tts::local::resolve_model_dir(
            window.get_tts_local_dir().as_str(),
            paths,
        ) {
            Ok(layout) => format!("TTS：{}", layout.describe()),
            Err(error) => format!("TTS：{error}"),
        },
    ]
    .join("\n")
}
