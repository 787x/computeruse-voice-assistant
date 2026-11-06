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

use std::sync::atomic::{AtomicBool, Ordering};
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
    /// 这一段是否还在推进（终态后变 `false`）。
    ///
    /// 监控任务在后台把状态推到 `Finished` / `Failed`，但 `SpeechRuntime`
    /// 本身在另一个任务里，拿不到那次状态更新的写入权。用共享原子表达
    /// "这一段已经结束"，`is_speaking()` 与 `sync_listen_mode()` 才能立刻
    /// 反映真实运行状态，而不是等下一次 `speak()` / `stop()` 才收敛。
    active: Arc<AtomicBool>,
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
    ///
    /// **必须以"这一段是否还在推进"为准，而不是"有没有 `Speaking` 句柄"**：
    /// 播报的终态（Finished / Failed / Stopped）由监控任务判定，句柄留在
    /// 工作线程这边不会自己消失。若这里只看句柄，播报结束后 `is_speaking()`
    /// 会永远为真，`sync_listen_mode` 就一直停在 `BargeIn`——麦克风再也
    /// 不要求唤醒词，这是审查里发现的 P1。
    pub fn is_speaking(&self) -> bool {
        self.speaking
            .as_ref()
            .is_some_and(|item| item.active.load(Ordering::SeqCst))
    }

    /// 当前有效播报的身份（没有正在进行的播报时为 `None`）。
    pub fn current_speech_id(&self) -> Option<SpeechId> {
        self.speaking
            .as_ref()
            .filter(|item| item.active.load(Ordering::SeqCst))
            .map(|item| item.speech_id)
    }

    /// 把这一段播报收敛到终态：作废身份、清空音频、通知界面。
    ///
    /// 只对**当前** `speech_id` 生效：旧播报迟到的终态不能结束新的播报。
    /// `report` 决定要不要再发一次 `Stopped`——自然完成（Finished）已经
    /// 由监控任务报过，这里不能再把它改写成"用户主动停止"。
    fn settle(&mut self, speech_id: SpeechId, terminal: PlaybackState, report: bool) -> bool {
        let Some(speaking) = self.speaking.as_ref() else {
            return false;
        };
        if speaking.speech_id != speech_id {
            return false;
        }
        speaking.active.store(false, Ordering::SeqCst);
        let speaking = self.speaking.take().expect("上面刚判过非空");
        speaking.cancel.cancel();
        speaking.task.abort();
        if let Some(shared) = &self.shared {
            shared.stop();
        }
        if report {
            let _ = self.events.send(SpeechEvent::State {
                speech_id,
                turn: speaking.turn,
                state: terminal,
            });
            let _ = self.events.send(SpeechEvent::Subtitle {
                speech_id,
                text: String::new(),
            });
        }
        true
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
        let mut plan = prepare_speech(text);
        let budget = engine.text_byte_budget();
        plan.sentences = plan
            .sentences
            .into_iter()
            .flat_map(|sentence| {
                let mut pieces = Vec::new();
                let mut part = String::new();
                for c in sentence.chars() {
                    if part.len() + c.len_utf8() > budget && !part.is_empty() {
                        pieces.push(std::mem::take(&mut part));
                    }
                    part.push(c);
                }
                if !part.is_empty() {
                    pieces.push(part);
                }
                pieces
            })
            .collect();
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
        let volume = self.settings.volume;
        let engine_for_task = engine.clone();
        let active = Arc::new(AtomicBool::new(true));
        // 字幕队列：合成端写入"哪一句从哪一帧开始"，监控端在设备真的播到
        // 那一帧时才切换字幕。旧播报的条目会被身份核对丢弃。
        let cues = Arc::new(Mutex::new(SubtitleCues::default()));

        // 合成任务：一句一句地合成，边到边入队。
        let synth_shared = shared.clone();
        let synth_cancel = cancel.clone();
        let synth_cues = cues.clone();
        let task = tokio::spawn(async move {
            let result = synthesize_all(
                engine_for_task,
                synth_shared.clone(),
                synth_cues,
                speech_id,
                turn,
                sentences,
                style,
                volume,
                synth_cancel.clone(),
            )
            .await;
            match result {
                Ok(()) => synth_shared.mark_end(speech_id),
                Err(SynthFailure::Cancelled) => {}
                // 旧播报的合成失败不能把新播报判成失败。
                Err(SynthFailure::Failed(message)) => synth_shared.fail_for(speech_id, message),
            }
        });

        // 监控任务：把"排空/停止/失败"翻译成界面状态，并负责收敛生命周期。
        let monitor_shared = shared;
        let monitor_events = events.clone();
        let monitor_cancel = cancel.clone();
        let monitor_active = active.clone();
        let monitor_cues = cues.clone();
        tokio::spawn(async move {
            monitor(
                monitor_shared,
                monitor_events,
                monitor_cues,
                speech_id,
                turn,
                monitor_cancel,
                monitor_active,
            )
            .await;
        });

        self.speaking = Some(Speaking {
            speech_id,
            turn,
            cancel,
            task,
            active,
        });
        Ok(speech_id)
    }

    /// 停止播报（**只停声音**：不取消 Agent 回合，不结算任何权限请求）。
    pub fn stop(&mut self) {
        let Some(speaking) = self.speaking.as_ref() else {
            return;
        };
        let speech_id = speaking.speech_id;
        // 已经自然播完 / 已经失败的那一段：只回收句柄，**不**再报一次 `Stopped`。
        // 把"正常播完"改写成"用户主动停止"会让界面状态和实际发生的事对不上。
        let still_running = speaking.active.load(Ordering::SeqCst);
        self.settle(speech_id, PlaybackState::Stopped, still_running);
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
    cues: Arc<Mutex<SubtitleCues>>,
    speech_id: SpeechId,
    turn: TurnId,
    sentences: Vec<String>,
    style: String,
    volume: f32,
    cancel: CancelToken,
) -> Result<(), SynthFailure> {
    let mut feed = Feed::new(speech_id);
    for (index, sentence) in sentences.iter().enumerate() {
        if cancel.is_cancelled() {
            return Err(SynthFailure::Cancelled);
        }
        let request = SynthRequest {
            speech_id,
            turn,
            sentence: index,
            text: sentence.clone(),
            style: style.clone(),
            cancel: cancel.clone(),
        };
        let mut stream = engine.start(request).map_err(SynthFailure::Failed)?;
        let mut first_chunk = true;
        loop {
            let event = tokio::select! {
                _ = cancel.cancelled() => return Err(SynthFailure::Cancelled),
                event = stream.recv() => event,
            };
            let Some(event) = event else { break };
            match event {
                TtsEvent::Chunk(mut chunk) => {
                    if volume != 1.0 {
                        for sample in &mut chunk.samples {
                            *sample *= volume;
                        }
                    }
                    if first_chunk {
                        /*
                         * 字幕与**实际播放**对齐：记下这一句的第一个样本会落在
                         * 设备缓冲的哪个位置（= 已经入队的帧数）。上一句在
                         * `push_chunk` 返回时已经全部入队，所以此刻的位置就是
                         * 这一句的起点。字幕由监控任务在设备真正消费到这里时
                         * 才切换——不是合成请求开始时就切换。
                         */
                        cues.lock().unwrap_or_else(|e| e.into_inner()).push(Cue {
                            speech_id,
                            start_frame: shared.progress().pushed_frames,
                            text: sentence.clone(),
                        });
                        first_chunk = false;
                    }
                    push_chunk(&shared, &mut feed, &chunk, &cancel).await?;
                }
                TtsEvent::Finished => break,
                TtsEvent::Failed(message) => return Err(SynthFailure::Failed(message)),
            }
        }
        // 一句话没有任何音频（后端静默失败）：仍要保证会有字幕/无字幕一致，
        // 不补发字幕——没有声音就不该显示"正在读这句"。
        if first_chunk && !cancel.is_cancelled() {
            eprintln!("[tts] 第 {index} 句没有产出音频，跳过字幕");
        }
    }
    // 收尾：把重采样器里剩下的样本冲出来（缓冲满就等）。
    finish_feed(&shared, &mut feed, &cancel).await?;
    Ok(())
}

/// 一条字幕提示：设备消费到 `start_frame` 时，界面应当显示 `text`。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Cue {
    speech_id: SpeechId,
    start_frame: u64,
    text: String,
}

/// 待切换的字幕队列（合成任务写、监控任务消费）。
#[derive(Debug, Default)]
struct SubtitleCues {
    pending: std::collections::VecDeque<Cue>,
}

impl SubtitleCues {
    fn push(&mut self, cue: Cue) {
        self.pending.push_back(cue);
    }

    fn clear(&mut self) {
        self.pending.clear();
    }

    /// 取出所有已经该显示的字幕（只保留最后一条：一次跳过两句时不必逐条闪）。
    ///
    /// `start_frame` 是这一句第一个样本在设备时间线上的位置（= 前面已经入队的
    /// 帧数）。只有当设备**确实消费到这一句之后**（`consumed > start_frame`）
    /// 才切换：字幕跟着实际出声走，而不是跟着合成请求走。
    fn take_ready(&mut self, speech_id: SpeechId, consumed_frames: u64) -> Option<String> {
        let mut ready = None;
        while let Some(front) = self.pending.front() {
            // 旧播报的字幕一律丢弃，绝不覆盖新播报。
            if front.speech_id != speech_id {
                self.pending.pop_front();
                continue;
            }
            if front.start_frame >= consumed_frames {
                break;
            }
            ready = self.pending.pop_front().map(|cue| cue.text);
        }
        ready
    }
}

/// 入队一个音频块；缓冲满时**等待**而不是丢音频、也不是阻塞音频回调。
///
/// 关键：偏移按**源样本消耗量**推进（`FeedProgress::consumed`），不是按设备侧
/// 入队帧数。24k → 48k 时后者是前者的两倍，用它推进会让同一段源音频被重复
/// 重采样（拖音）或跳过（丢字）。
async fn push_chunk(
    shared: &Arc<PlaybackShared>,
    feed: &mut Feed,
    chunk: &SpeechChunk,
    cancel: &CancelToken,
) -> Result<(), SynthFailure> {
    let total = chunk.samples.len();
    let mut offset = 0usize;
    while offset < total || feed.has_pending() {
        if cancel.is_cancelled() {
            return Err(SynthFailure::Cancelled);
        }
        let progress = feed
            .push_chunk(
                shared,
                &chunk.samples[offset..],
                chunk.sample_rate,
                chunk.channels,
            )
            .map_err(SynthFailure::Failed)?;
        offset += progress.consumed;
        // 缓冲满时生产端**不重复转换源输入**：只等设备把已经转换好的 PCM 排出去。
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
    // 第一次调用冲重采样尾部；之后的循环只是继续排出**已经转换好**的样本，
    // 不会再 flush 一次（重复 flush 会往缓冲里塞重复的尾音）。
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
///
/// 这个任务同时负责三件必须一起成立的事：
///  1. 把"设备真的排空了"翻译成 `Finished`（不是"合成结束"）；
///  2. 到终态时把 `active` 置 `false`，让 `is_speaking()` / 聆听模式
///     立刻回到真实状态（否则播报结束后麦克风一直停在打断模式）；
///  3. 按设备**实际消费进度**切换字幕，而不是按合成请求的开始。
#[allow(clippy::too_many_arguments)]
async fn monitor(
    shared: Arc<PlaybackShared>,
    events: tokio::sync::mpsc::UnboundedSender<SpeechEvent>,
    cues: Arc<Mutex<SubtitleCues>>,
    speech_id: SpeechId,
    turn: TurnId,
    cancel: CancelToken,
    active: Arc<AtomicBool>,
) {
    let mut last: Option<PlaybackState> = None;
    loop {
        if cancel.is_cancelled() {
            active.store(false, Ordering::SeqCst);
            return;
        }
        let progress = shared.progress();
        // 已经被新的播报取代：不再影响界面，也不能把新播报当成自己的。
        if progress.speech_id != speech_id {
            active.store(false, Ordering::SeqCst);
            // 被 `stop()` / `settle()` 主动作废的那一次已经报过终态，
            // 这里不能再补一条 `Stopped`（否则界面会看到两次终态）。
            if cancel.is_cancelled() {
                return;
            }
            if last.is_some() {
                let _ = events.send(SpeechEvent::State {
                    speech_id,
                    turn,
                    state: PlaybackState::Stopped,
                });
                let _ = events.send(SpeechEvent::Subtitle {
                    speech_id,
                    text: String::new(),
                });
            }
            return;
        }
        // 字幕跟着设备已消费的帧数走：这一句的音频真的轮到了才切换。
        let ready = cues
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take_ready(speech_id, progress.consumed_frames);
        if let Some(text) = ready {
            let _ = events.send(SpeechEvent::Subtitle { speech_id, text });
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
                // 终态：运行状态与界面状态必须一起收敛。
                active.store(false, Ordering::SeqCst);
                cues.lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clear();
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

    /// 收集事件，同时保留字幕（按到达顺序）。
    fn collect_all(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<SpeechEvent>,
    ) -> (Vec<PlaybackState>, Vec<String>) {
        let mut states = Vec::new();
        let mut subtitles = Vec::new();
        while let Ok(event) = rx.try_recv() {
            match event {
                SpeechEvent::State { state, .. } => states.push(state),
                SpeechEvent::Subtitle { text, .. } => subtitles.push(text),
                SpeechEvent::Notice { .. } => {}
            }
        }
        (states, subtitles)
    }

    /// 等所有音频入队且合成结束（`end_marked`），最多 1 秒。
    async fn wait_until_end_marked(shared: &PlaybackShared) {
        for _ in 0..200 {
            if shared.progress().end_marked {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("合成一直没有结束");
    }

    /// 像设备那样按固定块消费，直到缓冲排空。
    fn drain_device(shared: &PlaybackShared, frames: usize) {
        let mut sink = vec![0.0f32; frames];
        for _ in 0..10_000 {
            if shared.progress().drained() {
                return;
            }
            if shared.pull(&mut sink) == 0 {
                break;
            }
        }
    }

    #[tokio::test]
    async fn pr14_volume_changes_real_pcm_without_changing_text() {
        for volume in [0.0, 0.5, 1.0] {
            let started = Arc::new(Mutex::new(Vec::new()));
            let engine = Arc::new(FakeEngine {
                chunks: vec![vec![0.8; 2400]],
                sample_rate: 24_000,
                fail: None,
                started: started.clone(),
            });
            let (mut runtime, _rx, shared) = runtime_with(engine);
            runtime.settings.volume = volume;
            runtime.speak(1, "音量测试。").unwrap();
            wait_until_end_marked(&shared).await;
            let mut output = vec![0.0; 100];
            assert!(shared.pull(&mut output) > 0);
            assert!(output.iter().all(|v| (*v - 0.8 * volume).abs() < 0.0001));
            assert_eq!(started.lock().unwrap().as_slice(), &["音量测试"]);
            runtime.stop();
        }
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

    /// **P1 回归**：自然播完后 `is_speaking()` 必须变成 `false`。
    ///
    /// 旧实现只看 `self.speaking.is_some()`，而那个句柄只有在 `stop()` / 下一次
    /// `speak()` 时才会被清掉：播报结束后麦克风会一直停在打断聆听模式。
    #[tokio::test]
    async fn finished_speech_is_no_longer_reported_as_speaking() {
        let engine = Arc::new(FakeEngine {
            chunks: vec![vec![0.1; 2400]],
            sample_rate: 24_000,
            fail: None,
            started: Arc::new(Mutex::new(Vec::new())),
        });
        let (mut runtime, mut rx, shared) = runtime_with(engine);
        let speech_id = runtime.speak(1, "播报结束就恢复唤醒词。").unwrap();
        wait_until_end_marked(&shared).await;
        assert!(runtime.is_speaking(), "还在播的时候必须为真");

        drain_device(&shared, 2400);
        // 监控任务最多 20ms 一轮：等它把终态推出来。
        let mut finished = false;
        for _ in 0..100 {
            let (states, _) = collect_all(&mut rx);
            if states.contains(&PlaybackState::Finished) {
                finished = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(finished, "正常播完必须上报 Finished");
        assert!(
            !runtime.is_speaking(),
            "播报结束后 is_speaking() 必须为 false（否则麦克风会停在打断模式）"
        );
        assert_eq!(runtime.current_speech_id(), None);
        // 自然完成不能被改写成"用户主动停止"。
        assert!(
            !collect_states(&mut rx).contains(&PlaybackState::Stopped),
            "自然播完不能再报一次 Stopped"
        );
        // 再 stop() 一次也不该冒出新的事件。
        runtime.stop();
        assert!(
            collect_states(&mut rx).is_empty(),
            "已结束的播报不该再报终态"
        );
        assert_eq!(speech_id, 1);
    }

    /// 失败也要收敛生命周期：`is_speaking()` 变假，界面拿到 `Failed`。
    #[tokio::test]
    async fn failed_speech_converges_the_lifecycle() {
        let engine = Arc::new(FakeEngine {
            chunks: Vec::new(),
            sample_rate: 24_000,
            fail: Some("合成失败".to_string()),
            started: Arc::new(Mutex::new(Vec::new())),
        });
        let (mut runtime, mut rx, _shared) = runtime_with(engine);
        runtime.speak(1, "这一句会失败").unwrap();
        let mut failed = false;
        for _ in 0..100 {
            let (states, _) = collect_all(&mut rx);
            if states
                .iter()
                .any(|state| matches!(state, PlaybackState::Failed(_)))
            {
                failed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(failed, "合成失败必须上报");
        assert!(!runtime.is_speaking(), "失败后 is_speaking() 必须为 false");
    }

    /// **P1 回归**：A 停止后立刻 start B，A 迟到的终态不能结束 B。
    #[tokio::test]
    async fn late_terminal_event_from_previous_speech_does_not_end_the_new_one() {
        let started = Arc::new(Mutex::new(Vec::new()));
        let engine = Arc::new(FakeEngine {
            chunks: vec![vec![0.2; 4800]],
            sample_rate: 24_000,
            fail: None,
            started: started.clone(),
        });
        let (mut runtime, _rx, shared) = runtime_with(engine);
        let first = runtime.speak(1, "第一段").unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        let second = runtime.speak(2, "第二段").unwrap();
        assert!(second > first);
        assert!(runtime.is_speaking());
        assert_eq!(runtime.current_speech_id(), Some(second));
        // 旧播报的失败/结束尝试一律不能作用于新播报。
        shared.fail_for(first, "旧播报的失败".to_string());
        assert!(shared.progress().failed.is_none());
        assert!(runtime.is_speaking(), "旧播报的终态不能结束新播报");
        assert_eq!(runtime.current_speech_id(), Some(second));
    }

    /// **P2 回归**：字幕跟随**实际播放**，不是合成请求开始。
    ///
    /// 句 1 还在缓冲里没出声时，不能提前显示句 2。
    #[tokio::test]
    async fn subtitle_waits_for_its_own_audio_to_play() {
        let started = Arc::new(Mutex::new(Vec::new()));
        let engine = Arc::new(FakeEngine {
            // 每句 2400 帧 @24k = 100ms
            chunks: vec![vec![0.1; 2400]],
            sample_rate: 24_000,
            fail: None,
            started: started.clone(),
        });
        let (mut runtime, mut rx, shared) = runtime_with(engine);
        runtime.speak(1, "第一句。第二句。").unwrap();
        // 合成会跑完两句（字幕 cue 都已经排进队列）。
        for _ in 0..200 {
            if started.lock().unwrap().len() >= 2 && shared.progress().end_marked {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(started.lock().unwrap().len(), 2, "两句都要合成");

        // 设备还没消费任何样本：一条字幕都不该出现。
        let (_, subtitles) = collect_all(&mut rx);
        assert!(
            subtitles.is_empty(),
            "音频还没开始播就不该显示字幕：{subtitles:?}"
        );

        // 播掉第一句（设备侧 2400 帧 @24k，句 1 的 cue 在 0）。
        let mut sink = vec![0.0f32; 1200];
        for _ in 0..3 {
            shared.pull(&mut sink);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let (_, subtitles) = collect_all(&mut rx);
        assert!(
            subtitles.iter().any(|text| text == "第一句"),
            "第一句开始播时必须显示它：{subtitles:?}"
        );
        assert!(
            !subtitles.iter().any(|text| text == "第二句"),
            "第二句还没轮到，不能提前显示：{subtitles:?}"
        );

        // 播到第二句。
        for _ in 0..6 {
            shared.pull(&mut sink);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let (_, subtitles) = collect_all(&mut rx);
        assert!(
            subtitles.iter().any(|text| text == "第二句"),
            "第二句真正开始播时必须切换过去：{subtitles:?}"
        );
    }

    /// 停止后字幕要被清空（不能把上一句留在屏幕上）。
    #[tokio::test]
    async fn stop_clears_the_subtitle() {
        let engine = Arc::new(FakeEngine {
            chunks: vec![vec![0.1; 12_000]],
            sample_rate: 24_000,
            fail: None,
            started: Arc::new(Mutex::new(Vec::new())),
        });
        let (mut runtime, mut rx, _shared) = runtime_with(engine);
        runtime
            .speak(1, "很长的一句话，说到一半会被停掉。")
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        runtime.stop();
        let (states, subtitles) = collect_all(&mut rx);
        assert!(states.contains(&PlaybackState::Stopped), "{states:?}");
        assert!(
            subtitles.iter().any(|text| text.is_empty()),
            "停止后必须清空字幕：{subtitles:?}"
        );
    }

    /// 停止播报不触碰任何权限确认：语音层只负责自己那一段。
    ///
    /// 这条用"没有播报时 `stop()` 是幂等的空操作"来固定：
    /// 它不取消回合、不结算确认，也不会凭空产生终态事件。
    #[tokio::test]
    async fn stop_without_speech_is_a_no_op() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut runtime = SpeechRuntime::new(tx);
        runtime.stop();
        runtime.stop();
        assert!(!runtime.is_speaking());
        assert_eq!(runtime.current_speech_id(), None);
        assert!(rx.try_recv().is_err(), "没有播报时不该发出任何事件");
    }
}
