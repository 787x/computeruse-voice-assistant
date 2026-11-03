//! 一次播报的编排：句子顺序 → 合成 → 有界缓冲 → 设备输出。
//!
//! 职责边界：
//!  - 决定"播什么"：只播**经过确认的用户可见文本**（由调用方传入），
//!    自己不做任何猜测（不读 reasoning、不读工具 JSON）；
//!  - 决定"怎么播"：按整句顺序合成，边到边播，缓冲满就等（背压）；
//!  - 决定"什么时候算播完"：交给 [`crate::voice::playback`] 的缓冲排空判定；
//!  - 决定"什么时候停"：停止播报 / 取消任务 / 新回合开始。
//!
//! 身份：一次播报有唯一 `speech_id`，并绑定发起它的 Agent 回合（`turn`）。
//! 新播报一开始就换号：旧播报的合成任务、迟到的 PCM、监控任务全部作废，
//! 不会出现"上一轮的话在新一轮里接着说"。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::cancel::CancelToken;
use crate::secret::SecretStore;

use super::playback::{
    DeviceInfo, Feed, PlaybackShared, PlaybackState, PlayerHandle, LOW_WATER_SECONDS,
};
use super::tts::{
    build_engine, prepare_speech, SpeechChunk, SpeechId, SynthRequest, TtsEngine, TtsEvent,
    TtsSettings, TurnId,
};

/// 播报层发给界面的事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpeechEvent {
    /// 生命周期变化（界面据此显示状态、决定要不要打开打断聆听）。
    State {
        speech_id: SpeechId,
        turn: TurnId,
        state: PlaybackState,
    },
    /// 当前正在播的那一句（字幕）。
    Subtitle { speech_id: SpeechId, text: String },
    /// 需要让用户知道的提示（文本被截断、设备信息等）。
    Notice { speech_id: SpeechId, text: String },
}

/// 播报运行时（工作线程持有）。
///
/// 引擎与播放设备都是**懒加载、进程内复用**：模型与流不会每句重建。
pub struct SpeechRuntime {
    events: tokio::sync::mpsc::UnboundedSender<SpeechEvent>,
    engine: Option<Arc<dyn TtsEngine>>,
    /// 引擎不可用的原因（缺 Key / 缺模型 / 加载失败）。只禁用播报。
    engine_error: Option<String>,
    player: Option<PlayerHandle>,
    shared: Option<Arc<PlaybackShared>>,
    speaking: Option<Speaking>,
    next_speech_id: SpeechId,
    /// 最近一次播报的全部句子（回声防护用）。
    recent: Arc<Mutex<Vec<String>>>,
    /// 当前配置（切后端/换音色后重建引擎）。
    settings: TtsSettings,
}

/// 正在进行的这一次播报。
struct Speaking {
    speech_id: SpeechId,
    turn: TurnId,
    cancel: CancelToken,
    task: tokio::task::JoinHandle<()>,
}

impl SpeechRuntime {
    pub fn new(events: tokio::sync::mpsc::UnboundedSender<SpeechEvent>) -> Self {
        Self {
            events,
            engine: None,
            engine_error: None,
            player: None,
            shared: None,
            speaking: None,
            next_speech_id: 0,
            recent: Arc::new(Mutex::new(Vec::new())),
            settings: TtsSettings::default(),
        }
    }

    /// 应用配置：停掉当前播报，重建引擎（只有后端/音色/模型路径变化时才需要）。
    ///
    /// 失败**不影响**文本 Agent、CUA 与权限：只把播报标记为不可用并返回原因。
    pub fn apply_settings(
        &mut self,
        settings: &TtsSettings,
        secret: &SecretStore,
    ) -> Result<(), String> {
        self.stop();
        self.settings = settings.clone();
        self.engine = None;
        self.engine_error = None;
        if !settings.enabled || settings.backend == super::tts::TtsBackend::Off {
            return Ok(());
        }
        match build_engine(settings, secret) {
            Ok(engine) => {
                let engine: Arc<dyn TtsEngine> = Arc::from(engine);
                self.engine = Some(engine.clone());
                // 后台预热：本地模型 + CUDA 的首次初始化要几秒，放到用户还在
                // 配置的时候做完，第一次播报就不用等。失败只记日志，不影响可用性。
                let _ = std::thread::Builder::new()
                    .name("cuava-tts-warmup".to_string())
                    .spawn(move || engine.warm_up());
                Ok(())
            }
            Err(error) => {
                self.engine_error = Some(error.clone());
                Err(error)
            }
        }
    }

    /// 引擎是否可用（界面据此决定"试音"按钮与提示）。
    pub fn is_available(&self) -> bool {
        self.engine.is_some()
    }

    pub fn engine_error(&self) -> Option<&str> {
        self.engine_error.as_deref()
    }

    pub fn engine_label(&self) -> Option<String> {
        self.engine.as_ref().map(|engine| engine.label())
    }

    pub fn device_info(&self) -> Option<DeviceInfo> {
        self.player.as_ref().map(|player| player.info().clone())
    }

    /// 是否正在播报（含合成/缓冲）。
    pub fn is_speaking(&self) -> bool {
        self.speaking.is_some()
    }

    pub fn current_speech_id(&self) -> Option<SpeechId> {
        self.speaking.as_ref().map(|item| item.speech_id)
    }

    /// 最近一次播报的句子（回声防护：判断识别结果是不是我们自己在说）。
    pub fn recent_sentences(&self) -> Vec<String> {
        self.recent
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    /// 播报一段文本。返回本次播报的 `speech_id`。
    ///
    /// 调用方必须传入**已确认的用户可见文本**；空文本直接拒绝（不发请求）。
    pub fn speak(&mut self, turn: TurnId, text: &str) -> Result<SpeechId, String> {
        let engine = self.engine.clone().ok_or_else(|| {
            self.engine_error
                .clone()
                .unwrap_or_else(|| "语音播报未开启".to_string())
        })?;
        let plan = prepare_speech(text);
        if plan.is_empty() {
            return Err("没有可播报的文本".to_string());
        }
        // 上一段还没结束：直接停掉（新回合绝不接在旧音频后面）。
        self.stop();

        let shared = self.ensure_playback()?;
        self.next_speech_id += 1;
        let speech_id = self.next_speech_id;
        shared.begin(speech_id);
        *self.recent.lock().unwrap_or_else(|e| e.into_inner()) = plan.sentences.clone();

        if plan.dropped_chars > 0 {
            let _ = self.events.send(SpeechEvent::Notice {
                speech_id,
                text: format!(
                    "回复较长，只播报前 {} 字（另有 {} 字未播报）",
                    super::tts::MAX_SPEECH_CHARS,
                    plan.dropped_chars
                ),
            });
        }

        let cancel = CancelToken::new();
        let events = self.events.clone();
        let sentences = plan.sentences.clone();
        let style = self.settings.mimo_style.clone();
        let engine_for_task = engine.clone();

        // 合成任务：一句一句地合成，边到边入队。
        let synth_shared = shared.clone();
        let synth_cancel = cancel.clone();
        let synth_events = events.clone();
        let task = tokio::spawn(async move {
            let result = synthesize_all(
                engine_for_task,
                synth_shared.clone(),
                synth_events.clone(),
                speech_id,
                turn,
                sentences,
                style,
                synth_cancel.clone(),
            )
            .await;
            match result {
                Ok(()) => synth_shared.mark_end(speech_id),
                Err(SynthFailure::Cancelled) => {}
                Err(SynthFailure::Failed(message)) => synth_shared.fail(message),
            }
        });

        // 监控任务：把"排空/停止/失败"翻译成界面状态。
        let monitor_shared = shared;
        let monitor_events = events.clone();
        let monitor_cancel = cancel.clone();
        tokio::spawn(async move {
            monitor(
                monitor_shared,
                monitor_events,
                speech_id,
                turn,
                monitor_cancel,
            )
            .await;
        });

        self.speaking = Some(Speaking {
            speech_id,
            turn,
            cancel,
            task,
        });
        Ok(speech_id)
    }

    /// 停止播报（**只停声音**：不取消 Agent 回合，不结算任何权限请求）。
    pub fn stop(&mut self) {
        if let Some(speaking) = self.speaking.take() {
            speaking.cancel.cancel();
            speaking.task.abort();
            if let Some(shared) = &self.shared {
                shared.stop();
            }
            let _ = self.events.send(SpeechEvent::State {
                speech_id: speaking.speech_id,
                turn: speaking.turn,
                state: PlaybackState::Stopped,
            });
            let _ = self.events.send(SpeechEvent::Subtitle {
                speech_id: speaking.speech_id,
                text: String::new(),
            });
        }
    }

    /// 退出：停止播报、释放设备、丢弃引擎（本地模型随之释放）。
    pub fn shutdown(&mut self) {
        self.stop();
        if let Some(mut player) = self.player.take() {
            player.shutdown();
        }
        self.shared = None;
        self.engine = None;
        self.recent
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    /// 懒打开输出设备（失败只影响播报）。
    fn ensure_playback(&mut self) -> Result<Arc<PlaybackShared>, String> {
        if let Some(shared) = &self.shared {
            return Ok(shared.clone());
        }
        let player = PlayerHandle::start(&self.settings.output_device).map_err(|error| {
            self.engine_error = Some(error.to_string());
            error.to_string()
        })?;
        let shared = player.shared();
        self.player = Some(player);
        self.shared = Some(shared.clone());
        Ok(shared)
    }

    /// 供测试：接一个不依赖真实设备的播放缓冲（手动 `pull` 当假扬声器）。
    #[cfg(test)]
    pub fn attach_playback_for_test(&mut self, shared: Arc<PlaybackShared>) {
        self.shared = Some(shared);
    }

    /// 供测试：直接注入引擎，跳过模型/Key 检查。
    #[cfg(test)]
    pub fn attach_engine_for_test(&mut self, engine: Arc<dyn TtsEngine>) {
        self.engine = Some(engine);
    }
}

/// 合成失败的两种结局。
enum SynthFailure {
    Cancelled,
    Failed(String),
}

/// 逐句合成：只有**当前** `speech_id` 的音频会进缓冲。
#[allow(clippy::too_many_arguments)]
async fn synthesize_all(
    engine: Arc<dyn TtsEngine>,
    shared: Arc<PlaybackShared>,
    events: tokio::sync::mpsc::UnboundedSender<SpeechEvent>,
    speech_id: SpeechId,
    turn: TurnId,
    sentences: Vec<String>,
    style: String,
    cancel: CancelToken,
) -> Result<(), SynthFailure> {
    let mut feed = Feed::new(speech_id);
    for (index, sentence) in sentences.iter().enumerate() {
        if cancel.is_cancelled() {
            return Err(SynthFailure::Cancelled);
        }
        let _ = events.send(SpeechEvent::Subtitle {
            speech_id,
            text: sentence.clone(),
        });
        let request = SynthRequest {
            speech_id,
            turn,
            sentence: index,
            text: sentence.clone(),
            style: style.clone(),
            cancel: cancel.clone(),
        };
        let mut stream = engine.start(request).map_err(SynthFailure::Failed)?;
        loop {
            let event = tokio::select! {
                _ = cancel.cancelled() => return Err(SynthFailure::Cancelled),
                event = stream.recv() => event,
            };
            let Some(event) = event else { break };
            match event {
                TtsEvent::Chunk(chunk) => {
                    push_chunk(&shared, &mut feed, &chunk, &cancel).await?;
                }
                TtsEvent::Finished => break,
                TtsEvent::Failed(message) => return Err(SynthFailure::Failed(message)),
            }
        }
    }
    // 收尾：把重采样器里剩下的样本冲出来（缓冲满就等）。
    finish_feed(&shared, &mut feed, &cancel).await?;
    Ok(())
}

/// 入队一个音频块；缓冲满时**等待**而不是丢音频、也不是阻塞音频回调。
async fn push_chunk(
    shared: &Arc<PlaybackShared>,
    feed: &mut Feed,
    chunk: &SpeechChunk,
    cancel: &CancelToken,
) -> Result<(), SynthFailure> {
    let total = chunk.samples.len();
    let mut offset = 0usize;
    while offset < total {
        if cancel.is_cancelled() {
            return Err(SynthFailure::Cancelled);
        }
        let accepted = feed
            .push_chunk(
                shared,
                &chunk.samples[offset..],
                chunk.sample_rate,
                chunk.channels,
            )
            .map_err(SynthFailure::Failed)?;
        offset += accepted;
        if feed.has_pending() || offset < total {
            wait_for_space(shared, cancel).await?;
        }
    }
    Ok(())
}

async fn finish_feed(
    shared: &Arc<PlaybackShared>,
    feed: &mut Feed,
    cancel: &CancelToken,
) -> Result<(), SynthFailure> {
    feed.finish(shared).map_err(SynthFailure::Failed)?;
    while feed.has_pending() {
        if cancel.is_cancelled() {
            return Err(SynthFailure::Cancelled);
        }
        wait_for_space(shared, cancel).await?;
        feed.finish(shared).map_err(SynthFailure::Failed)?;
    }
    Ok(())
}

/// 等缓冲降到低水位。超时上限只是防止设备彻底不动时永远等下去。
async fn wait_for_space(
    shared: &Arc<PlaybackShared>,
    cancel: &CancelToken,
) -> Result<(), SynthFailure> {
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    loop {
        if cancel.is_cancelled() {
            return Err(SynthFailure::Cancelled);
        }
        if shared.buffered_seconds() < LOW_WATER_SECONDS {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(SynthFailure::Failed(
                "播放缓冲一直没有被消费（输出设备可能被禁用或占用）".to_string(),
            ));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// 播报监控：状态只在**真的变化**时上报，播完以缓冲排空为准。
async fn monitor(
    shared: Arc<PlaybackShared>,
    events: tokio::sync::mpsc::UnboundedSender<SpeechEvent>,
    speech_id: SpeechId,
    turn: TurnId,
    cancel: CancelToken,
) {
    let mut last: Option<PlaybackState> = None;
    loop {
        if cancel.is_cancelled() {
            return;
        }
        let progress = shared.progress();
        // 已经被新的播报取代：立刻退出，不再影响界面。
        if progress.speech_id != speech_id {
            if last.is_some() {
                let _ = events.send(SpeechEvent::State {
                    speech_id,
                    turn,
                    state: PlaybackState::Stopped,
                });
            }
            return;
        }
        let state = if let Some(error) = progress.failed.clone() {
            PlaybackState::Failed(error)
        } else if progress.drained() {
            PlaybackState::Finished
        } else if progress.started {
            PlaybackState::Playing
        } else if progress.pushed_frames > 0 {
            PlaybackState::Buffering
        } else {
            PlaybackState::Synthesizing
        };
        if last.as_ref() != Some(&state) {
            last = Some(state.clone());
            let _ = events.send(SpeechEvent::State {
                speech_id,
                turn,
                state: state.clone(),
            });
            if state == PlaybackState::Finished || matches!(state, PlaybackState::Failed(_)) {
                let _ = events.send(SpeechEvent::Subtitle {
                    speech_id,
                    text: String::new(),
                });
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice::playback::PlaybackShared;
    use crate::voice::tts::{AudioFormat, SpeechStream};

    /// 可注入的假引擎：把预设的音频块一次性吐出去，不联网、不推理。
    struct FakeEngine {
        chunks: Vec<Vec<f32>>,
        sample_rate: u32,
        fail: Option<String>,
        started: Arc<Mutex<Vec<String>>>,
    }

    impl TtsEngine for FakeEngine {
        fn label(&self) -> String {
            "fake".to_string()
        }

        fn start(&self, request: SynthRequest) -> Result<SpeechStream, String> {
            self.started.lock().unwrap().push(request.text.clone());
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let chunks = self.chunks.clone();
            let rate = self.sample_rate;
            let fail = self.fail.clone();
            let speech_id = request.speech_id;
            let turn = request.turn;
            let sentence = request.sentence;
            tokio::spawn(async move {
                if let Some(message) = fail {
                    let _ = tx.send(TtsEvent::Failed(message));
                    return;
                }
                for samples in chunks {
                    let _ = tx.send(TtsEvent::Chunk(SpeechChunk {
                        speech_id,
                        turn,
                        sentence,
                        sample_rate: rate,
                        channels: 1,
                        format: AudioFormat::Pcm16Le,
                        samples,
                    }));
                }
                let _ = tx.send(TtsEvent::Finished);
            });
            Ok(SpeechStream::new(rx))
        }
    }

    fn runtime_with(
        engine: Arc<dyn TtsEngine>,
    ) -> (
        SpeechRuntime,
        tokio::sync::mpsc::UnboundedReceiver<SpeechEvent>,
        Arc<PlaybackShared>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut runtime = SpeechRuntime::new(tx);
        // 假扬声器：24k 单声道，测试手动 pull。
        let shared = Arc::new(PlaybackShared::new(24_000, 1));
        runtime.attach_playback_for_test(shared.clone());
        runtime.attach_engine_for_test(engine);
        (runtime, rx, shared)
    }

    fn collect_states(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<SpeechEvent>,
    ) -> Vec<PlaybackState> {
        let mut states = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let SpeechEvent::State { state, .. } = event {
                states.push(state);
            }
        }
        states
    }

    /// **核心**：状态必须如实反映"设备真的播完了"，而不是"合成结束了"。
    #[tokio::test]
    async fn state_becomes_finished_only_when_buffer_drains() {
        let started = Arc::new(Mutex::new(Vec::new()));
        let engine = Arc::new(FakeEngine {
            chunks: vec![vec![0.1; 2400], vec![0.1; 2400]],
            sample_rate: 24_000,
            fail: None,
            started: started.clone(),
        });
        let (mut runtime, mut rx, shared) = runtime_with(engine);
        let speech_id = runtime.speak(1, "第一句。第二句。").unwrap();
        assert_eq!(speech_id, 1);
        // 合成在独立任务里跑：等它把两句都发起（最多 1 秒），不要靠 sleep 猜。
        for _ in 0..200 {
            if started.lock().unwrap().len() >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(started.lock().unwrap().len(), 2, "两句都要合成");
        assert!(runtime.is_speaking());

        // 等合成把音频塞进缓冲（还没被消费）。
        for _ in 0..50 {
            if shared.progress().end_marked {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(shared.progress().end_marked);
        let states = collect_states(&mut rx);
        assert!(
            !states.contains(&PlaybackState::Finished),
            "还没出声就不能报播完：{states:?}"
        );

        // 假扬声器开始消费，直到排空。
        let mut sink = vec![0.0f32; 2400];
        for _ in 0..10 {
            shared.pull(&mut sink);
            tokio::time::sleep(Duration::from_millis(10)).await;
            if shared.progress().drained() {
                break;
            }
        }
        for _ in 0..50 {
            if collect_states(&mut rx).contains(&PlaybackState::Finished) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(shared.progress().drained());
    }

    /// 停止播报：立即清空缓冲并发出 Stopped，不影响任何其它东西。
    #[tokio::test]
    async fn stop_clears_audio_and_emits_stopped() {
        let engine = Arc::new(FakeEngine {
            chunks: vec![vec![0.2; 12_000]],
            sample_rate: 24_000,
            fail: None,
            started: Arc::new(Mutex::new(Vec::new())),
        });
        let (mut runtime, mut rx, shared) = runtime_with(engine);
        runtime.speak(1, "很长的一句话").unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        runtime.stop();
        assert!(!runtime.is_speaking());
        assert_eq!(shared.buffered_frames(), 0);
        let states = collect_states(&mut rx);
        assert!(states.contains(&PlaybackState::Stopped), "{states:?}");
    }

    /// 合成失败只影响播报：状态进 Failed 并带可读原因。
    #[tokio::test]
    async fn synthesis_failure_is_reported() {
        let engine = Arc::new(FakeEngine {
            chunks: Vec::new(),
            sample_rate: 24_000,
            fail: Some("网络断了".to_string()),
            started: Arc::new(Mutex::new(Vec::new())),
        });
        let (mut runtime, mut rx, _shared) = runtime_with(engine);
        runtime.speak(1, "你好").unwrap();
        let mut failed = None;
        for _ in 0..50 {
            for state in collect_states(&mut rx) {
                if matches!(state, PlaybackState::Failed(_)) {
                    failed = Some(state);
                }
            }
            if failed.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let fail = failed.expect("必须上报失败");
        assert!(fail.label().contains("网络断了"), "{:?}", fail);
    }

    /// 换回合：新播报一开始就换号，旧音频一个样本都不能留在缓冲里。
    #[tokio::test]
    async fn new_speech_replaces_previous_audio() {
        let engine = Arc::new(FakeEngine {
            chunks: vec![vec![0.3; 12_000]],
            sample_rate: 24_000,
            fail: None,
            started: Arc::new(Mutex::new(Vec::new())),
        });
        let (mut runtime, _rx, shared) = runtime_with(engine);
        let first = runtime.speak(1, "第一轮").unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        let second = runtime.speak(2, "第二轮").unwrap();
        assert!(second > first);
        assert_eq!(shared.progress().speech_id, second);
        // 旧号迟到的入队被拒绝。
        assert_eq!(shared.push(first, &[0.5; 10]), 0);
        assert_eq!(shared.push(second, &[0.5; 10]), 10);
    }

    /// 空文本不发起任何合成（不发请求、不占设备）。
    #[tokio::test]
    async fn empty_text_is_not_synthesized() {
        let started = Arc::new(Mutex::new(Vec::new()));
        let engine = Arc::new(FakeEngine {
            chunks: vec![vec![0.0; 240]],
            sample_rate: 24_000,
            fail: None,
            started: started.clone(),
        });
        let (mut runtime, _rx, _shared) = runtime_with(engine);
        assert!(runtime.speak(1, "   ").is_err());
        assert!(runtime.speak(1, "").is_err());
        assert!(started.lock().unwrap().is_empty());
    }

    /// 没配引擎时给出可读原因，而不是静默无声。
    #[tokio::test]
    async fn missing_engine_is_explained() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut runtime = SpeechRuntime::new(tx);
        let error = runtime.speak(1, "你好").unwrap_err();
        assert!(error.contains("未开启"), "{error}");
    }

    /// 超长回复要如实告知被截断，并仍然播报前一部分。
    #[tokio::test]
    async fn long_text_is_truncated_with_notice() {
        let engine = Arc::new(FakeEngine {
            chunks: vec![vec![0.05; 480]],
            sample_rate: 24_000,
            fail: None,
            started: Arc::new(Mutex::new(Vec::new())),
        });
        let (mut runtime, mut rx, _shared) = runtime_with(engine);
        let text = "这是一段很长的回复。".repeat(200);
        runtime.speak(1, &text).unwrap();
        let mut notice = None;
        for _ in 0..20 {
            while let Ok(event) = rx.try_recv() {
                if let SpeechEvent::Notice { text, .. } = event {
                    notice = Some(text);
                }
            }
            if notice.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(notice.unwrap().contains("只播报前"));
    }
}
