//! 语音播放：有界缓冲 + Windows 原生输出（cpal → WASAPI 共享模式）。
//!
//! 三条硬约束（任务书）：
//!  1. **音频输出回调里绝不等待**网络、推理或 Slint 线程：回调只从内存队列里取样本，
//!     取不到就输出静音并计数（下溢），不会阻塞 WASAPI；
//!  2. **有界缓冲**：队列有明确上限，超限时生产端等待，而不是无限堆积内存；
//!  3. **"播完"以缓冲真正排空为准**：HTTP 结束 / 模型生成结束都只表示"没有更多音频了"，
//!     只有队列里最后一个样本被设备消费掉，才算正常播完。
//!
//! 取消（停止播报）是**立即**的：清空队列 + 丢弃旧 `speech_id` 的后续入队。
//! 设备与线程由 [`PlayerHandle`] 独占持有，Drop 时停止输出并释放设备。
//!
//! 这一层不知道"谁在说话"：所有音频都带 `speech_id`，跨回合串音由身份核对挡住。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::voice::pcm::RateConverter;

/// 缓冲上限（秒，按设备采样率换算）。
///
/// 10 秒足够吸收网络抖动与本地推理的停顿；再多就说明设备没有在消费
/// （被禁用、被独占、已拔出），这时让生产端等待比无限堆内存更安全。
pub const MAX_BUFFER_SECONDS: f32 = 10.0;

/// 缓冲低水位（秒）：低于它才继续合成下一句，避免"先攒一整段再开口"。
pub const LOW_WATER_SECONDS: f32 = 4.0;

/// 播放生命周期。对用户可观察，所以状态码稳定（Slint 侧按字符串匹配）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaybackState {
    /// 没有播报。
    Idle,
    /// 正在合成（还没有可播的音频）。
    Synthesizing,
    /// 已有音频进入缓冲，但设备还没开始出声。
    Buffering,
    /// 设备正在出声。
    Playing,
    /// 缓冲真正排空，正常播完。
    Finished,
    /// 被用户停止（停止播报 / 取消任务 / 换回合）。
    Stopped,
    /// 失败（合成失败、设备不可用/断开）。
    Failed(String),
}

impl PlaybackState {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Synthesizing => "synthesizing",
            Self::Buffering => "buffering",
            Self::Playing => "playing",
            Self::Finished => "finished",
            Self::Stopped => "stopped",
            Self::Failed(_) => "failed",
        }
    }

    pub fn label(&self) -> String {
        match self {
            Self::Idle => "未播报".to_string(),
            Self::Synthesizing => "正在合成…".to_string(),
            Self::Buffering => "正在缓冲…".to_string(),
            Self::Playing => "正在播报".to_string(),
            Self::Finished => "播报结束".to_string(),
            Self::Stopped => "已停止播报".to_string(),
            Self::Failed(message) => format!("播报失败：{message}"),
        }
    }

    pub fn is_active(&self) -> bool {
        matches!(self, Self::Synthesizing | Self::Buffering | Self::Playing)
    }
}

/// 输出设备的实际参数（如实显示，不是用户期望值）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub device_name: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub sample_format: String,
}

/// 播放失败原因（要能直接显示给用户）。
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum PlayerError {
    #[error("没有可用的扬声器输出设备")]
    NoDevice,
    #[error("读取输出设备配置失败：{0}")]
    Config(String),
    #[error("无法打开输出设备“{device}”：{error}")]
    Open { device: String, error: String },
    #[error("输出设备不可用：{0}")]
    Stream(String),
    #[error("打开输出设备超时")]
    Timeout,
}

/// 队列内部状态（只被短临界区保护；音频回调用 `try_lock`，拿不到就出静音）。
struct Core {
    /// 当前播放的语音身份。0 = 空闲。
    speech_id: u64,
    queue: VecDeque<f32>,
    /// 本次语音已没有更多音频（合成/网络结束）。
    end_marked: bool,
    /// 设备真的消费过样本（用于区分 Buffering / Playing）。
    started: bool,
    stopped: bool,
    failed: Option<String>,
}

/// 播放缓冲 + 统计。音频回调、生产端、界面三方共享。
pub struct PlaybackShared {
    inner: Mutex<Core>,
    device_rate: u32,
    channels: u16,
    capacity_frames: usize,
    /// 设备已消费的帧数（回调写入，无锁读取）。
    consumed_frames: AtomicU64,
    /// 生产端已入队的帧数（用于展示与测试）。
    pushed_frames: AtomicU64,
    /// 回调取不到样本的次数（设备在跑但缓冲空）。
    underruns: AtomicU64,
    /// 回调拿不到锁的次数（生产端正在入队，属于正常竞争）。
    lock_misses: AtomicU64,
}

/// 一次播报的进度快照（给监控任务与测试用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Progress {
    pub speech_id: u64,
    pub started: bool,
    pub end_marked: bool,
    pub buffered_frames: usize,
    pub consumed_frames: u64,
    pub pushed_frames: u64,
    pub stopped: bool,
    pub failed: Option<String>,
}

impl Progress {
    /// 正常播完：**没有更多音频 + 队列已空 + 设备确实消费过**。
    pub fn drained(&self) -> bool {
        self.speech_id != 0 && self.end_marked && self.buffered_frames == 0 && self.started
    }
}

impl PlaybackShared {
    /// 建立一个播放缓冲。
    ///
    /// 生产代码由设备线程用设备参数调用；测试可以直接用它当"可控假扬声器"
    /// （手工 `pull` 模拟回调节奏），从而在**没有真实声卡**的机器上验证
    /// "播完以排空为准"这类语义。
    pub fn new(device_rate: u32, channels: u16) -> Self {
        let capacity_frames = (device_rate as f32 * MAX_BUFFER_SECONDS) as usize;
        Self {
            inner: Mutex::new(Core {
                speech_id: 0,
                queue: VecDeque::new(),
                end_marked: false,
                started: false,
                stopped: false,
                failed: None,
            }),
            device_rate,
            channels,
            capacity_frames,
            consumed_frames: AtomicU64::new(0),
            pushed_frames: AtomicU64::new(0),
            underruns: AtomicU64::new(0),
            lock_misses: AtomicU64::new(0),
        }
    }

    pub fn device_rate(&self) -> u32 {
        self.device_rate
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Core> {
        // 临界区里只有队列操作，不会 panic；中毒也继续用（丢一次播报比死锁好）。
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 开始一段新语音：清掉上一段残留（旧 `speech_id` 的后续入队会被拒绝）。
    pub fn begin(&self, speech_id: u64) {
        let mut core = self.lock();
        core.speech_id = speech_id;
        core.queue.clear();
        core.end_marked = false;
        core.started = false;
        core.stopped = false;
        core.failed = None;
        self.consumed_frames.store(0, Ordering::SeqCst);
        self.pushed_frames.store(0, Ordering::SeqCst);
        self.underruns.store(0, Ordering::SeqCst);
        self.lock_misses.store(0, Ordering::SeqCst);
    }

    /// 入队一段**设备采样率、单声道**的样本。
    ///
    /// 返回实际接受的帧数：缓冲有界，接不下时返回小于 `samples.len()` 的值，
    /// 生产端应当等待（[`Self::buffered_seconds`] 降到低水位）再继续。
    /// `speech_id` 不是当前语音时**一个样本都不接受**（过期音频绝不入队）。
    pub fn push(&self, speech_id: u64, samples: &[f32]) -> usize {
        if samples.is_empty() {
            return 0;
        }
        let mut core = self.lock();
        if core.speech_id != speech_id || core.stopped || core.failed.is_some() {
            return 0;
        }
        let space = self.capacity_frames.saturating_sub(core.queue.len());
        let take = space.min(samples.len());
        core.queue.extend(samples[..take].iter().copied());
        self.pushed_frames.fetch_add(take as u64, Ordering::Relaxed);
        take
    }

    /// 标记这一段已经没有更多音频（合成结束 / 网络结束）。
    pub fn mark_end(&self, speech_id: u64) {
        let mut core = self.lock();
        if core.speech_id == speech_id {
            core.end_marked = true;
        }
    }

    /// 立即停止：清空队列并作废当前语音。返回被作废的 `speech_id`。
    pub fn stop(&self) -> u64 {
        let mut core = self.lock();
        let previous = core.speech_id;
        core.queue.clear();
        core.end_marked = false;
        core.stopped = true;
        core.speech_id = 0;
        previous
    }

    /// 设备/驱动报错：把这一段标记为失败（文本结果不受影响）。
    pub fn fail(&self, message: String) {
        let mut core = self.lock();
        if core.failed.is_none() {
            core.failed = Some(message);
        }
        core.queue.clear();
    }

    pub fn buffered_frames(&self) -> usize {
        self.lock().queue.len()
    }

    pub fn buffered_seconds(&self) -> f32 {
        self.buffered_frames() as f32 / self.device_rate as f32
    }

    pub fn progress(&self) -> Progress {
        let core = self.lock();
        Progress {
            speech_id: core.speech_id,
            started: core.started,
            end_marked: core.end_marked,
            buffered_frames: core.queue.len(),
            consumed_frames: self.consumed_frames.load(Ordering::Relaxed),
            pushed_frames: self.pushed_frames.load(Ordering::Relaxed),
            stopped: core.stopped,
            failed: core.failed.clone(),
        }
    }

    /// 音频回调专用：把最多 `out.len()` 个**单声道**样本写进 `out`，返回写入数。
    ///
    /// 绝不阻塞：拿不到锁就输出静音（这类竞争只发生在生产端入队的瞬间）。
    /// 测试里手工调用它，就等于"设备按自己的节奏消费样本"。
    pub(crate) fn pull(&self, out: &mut [f32]) -> usize {
        let Ok(mut core) = self.inner.try_lock() else {
            self.lock_misses.fetch_add(1, Ordering::Relaxed);
            out.fill(0.0);
            return 0;
        };
        let mut written = 0;
        while written < out.len() {
            match core.queue.pop_front() {
                Some(sample) => {
                    out[written] = sample;
                    written += 1;
                    core.started = true;
                }
                None => break,
            }
        }
        if written == 0 {
            out.fill(0.0);
            self.underruns.fetch_add(1, Ordering::Relaxed);
        } else {
            for slot in out.iter_mut().skip(written) {
                *slot = 0.0;
            }
        }
        self.consumed_frames
            .fetch_add(written as u64, Ordering::Relaxed);
        written
    }

    pub fn underruns(&self) -> u64 {
        self.underruns.load(Ordering::Relaxed)
    }

    pub fn lock_misses(&self) -> u64 {
        self.lock_misses.load(Ordering::Relaxed)
    }
}

/// 把 TTS 的音频转换成设备格式后入队（重采样在生产端完成，回调里不做滤波）。
///
/// 一个 `Feed` 对应**一段**语音：它记住源格式，遇到采样率变化（不同后端、
/// 不同模型）就重建转换器，不会把两种采样率混着播。
pub struct Feed {
    speech_id: u64,
    source_rate: u32,
    source_channels: u16,
    converter: Option<RateConverter>,
    /// 单声道待入队样本（重采样输出）。
    pending: Vec<f32>,
}

impl Feed {
    pub fn new(speech_id: u64) -> Self {
        Self {
            speech_id,
            source_rate: 0,
            source_channels: 0,
            converter: None,
            pending: Vec::new(),
        }
    }

    pub fn speech_id(&self) -> u64 {
        self.speech_id
    }

    /// 送入一个音频块（源采样率、源声道数），返回已入队到设备缓冲的设备侧帧数。
    ///
    /// 返回 0 可能表示"缓冲已满"（调用方应等待）或"这一段已被作废"——
    /// 调用方用 [`PlaybackShared::progress`] 区分。
    pub fn push_chunk(
        &mut self,
        shared: &PlaybackShared,
        samples: &[f32],
        sample_rate: u32,
        channels: u16,
    ) -> Result<usize, String> {
        if sample_rate == 0 || channels == 0 {
            return Err(format!("音频格式无效：{sample_rate} Hz / {channels} 声道"));
        }
        // 源格式换了：重建转换器（同一段语音里不该发生，但不同后端切换时会）。
        if self.converter.is_none() || self.source_rate != sample_rate {
            self.converter = Some(RateConverter::new(sample_rate, shared.device_rate())?);
            self.source_rate = sample_rate;
            self.source_channels = channels;
            self.pending.clear();
        }
        let mono: Vec<f32> = if channels == 1 {
            samples.to_vec()
        } else {
            crate::voice::pcm::downmix_interleaved(samples, channels as usize)
        };
        let converted = self.converter.as_mut().expect("上面刚建好").push(&mono)?;
        self.pending.extend_from_slice(&converted);
        Ok(self.drain(shared))
    }

    /// 把 `pending` 里能放下的部分入队。
    fn drain(&mut self, shared: &PlaybackShared) -> usize {
        if self.pending.is_empty() {
            return 0;
        }
        let accepted = shared.push(self.speech_id, &self.pending);
        if accepted > 0 {
            self.pending.drain(..accepted);
        }
        accepted
    }

    /// 这段音频结束：把重采样器里剩下的样本冲出来并入队。
    pub fn finish(&mut self, shared: &PlaybackShared) -> Result<usize, String> {
        let tail = match self.converter.as_mut() {
            Some(converter) => converter.flush()?,
            None => Vec::new(),
        };
        self.pending.extend_from_slice(&tail);
        Ok(self.drain(shared))
    }

    /// 还有没入队的样本（缓冲满时会出现）。
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }
}

/// 输出设备句柄：持有一个长期存在的播放流与一个消费线程。
///
/// `cpal::Stream` 不是 `Send`，所以流在**自己的线程**里创建并持有；
/// 句柄只暴露共享缓冲与停止开关。
pub struct PlayerHandle {
    shared: Arc<PlaybackShared>,
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
    info: DeviceInfo,
}

impl PlayerHandle {
    /// 打开输出设备（`device_name` 为空 = 系统默认输出）。
    ///
    /// 失败时返回可读原因：调用方据此**只禁用播报**，文本与 CUA 不受影响。
    pub fn start(device_name: &str) -> Result<Self, PlayerError> {
        let (tx, rx) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let wanted = device_name.trim().to_string();

        let join = std::thread::Builder::new()
            .name("cuava-tts-output".to_string())
            .spawn(move || match open_output(&wanted, &stop_thread) {
                Ok((stream, shared, info)) => {
                    let _ = tx.send(Ok((shared, info)));
                    // 一直持有流，直到上层要求停止（Drop 时置位）。
                    while !stop_thread.load(Ordering::SeqCst) {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    drop(stream);
                }
                Err(error) => {
                    let _ = tx.send(Err(error));
                }
            })
            .map_err(|error| PlayerError::Stream(format!("无法创建播放线程：{error}")))?;

        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok((shared, info))) => Ok(Self {
                shared,
                stop,
                join: Some(join),
                info,
            }),
            Ok(Err(error)) => {
                stop.store(true, Ordering::SeqCst);
                let _ = join.join();
                Err(error)
            }
            Err(_) => {
                stop.store(true, Ordering::SeqCst);
                Err(PlayerError::Timeout)
            }
        }
    }

    pub fn shared(&self) -> Arc<PlaybackShared> {
        self.shared.clone()
    }

    pub fn info(&self) -> &DeviceInfo {
        &self.info
    }

    /// 停止输出并释放设备（等所有权所属线程收尾，有上限）。
    pub fn shutdown(&mut self) {
        self.shared.stop();
        self.stop.store(true, Ordering::SeqCst);
        if let Some(join) = self.join.take() {
            let deadline = std::time::Instant::now() + Duration::from_millis(500);
            while !join.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            if join.is_finished() {
                let _ = join.join();
            }
        }
    }
}

impl Drop for PlayerHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// 打开设备并按它的采样格式建立输出流。
fn open_output(
    device_name: &str,
    stop: &Arc<AtomicBool>,
) -> Result<(cpal::Stream, Arc<PlaybackShared>, DeviceInfo), PlayerError> {
    let host = cpal::default_host();
    let device = if device_name.is_empty() {
        host.default_output_device().ok_or(PlayerError::NoDevice)?
    } else {
        let mut found = None;
        for candidate in host
            .output_devices()
            .map_err(|error| PlayerError::Config(format!("枚举输出设备失败：{error}")))?
        {
            let name = candidate
                .description()
                .map(|d| d.name().to_string())
                .unwrap_or_default();
            if name == device_name {
                found = Some(candidate);
                break;
            }
        }
        found.ok_or_else(|| {
            PlayerError::Config(format!("找不到扬声器“{device_name}”，请改用系统默认设备"))
        })?
    };

    let label = device
        .description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| "未知设备".to_string());
    let supported = device
        .default_output_config()
        .map_err(|error| PlayerError::Config(format!("读取默认输出配置失败：{error}")))?;
    let sample_format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    if config.sample_rate == 0 || config.channels == 0 {
        return Err(PlayerError::Config(format!(
            "设备返回了无效配置：{} Hz / {} 声道",
            config.sample_rate, config.channels
        )));
    }

    let shared = Arc::new(PlaybackShared::new(
        config.sample_rate,
        config.channels as u16,
    ));
    let stream = build_output_stream(
        &device,
        &config,
        sample_format,
        shared.clone(),
        stop,
        &label,
    )?;
    stream
        .play()
        .map_err(|error| PlayerError::Stream(format!("启动输出失败：{error}")))?;

    let info = DeviceInfo {
        device_name: label,
        sample_rate: config.sample_rate,
        channels: config.channels as u16,
        sample_format: format!("{sample_format:?}"),
    };
    Ok((stream, shared, info))
}

fn build_output_stream(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    format: cpal::SampleFormat,
    shared: Arc<PlaybackShared>,
    stop: &Arc<AtomicBool>,
    device_label: &str,
) -> Result<cpal::Stream, PlayerError> {
    use cpal::SampleFormat as F;

    macro_rules! open {
        ($ty:ty, $convert:expr) => {{
            let convert = $convert;
            let callback_shared = shared.clone();
            let callback_stop = stop.clone();
            let channels = config.channels as usize;
            let mut mono: Vec<f32> = Vec::new();
            device
                .build_output_stream::<$ty, _, _>(
                    config.clone(),
                    move |data: &mut [$ty], _info| {
                        let frames = data.len() / channels.max(1);
                        // 停止后立刻出静音：不再消费队列（旧语音不会再"漏"出来）。
                        if callback_stop.load(Ordering::SeqCst) {
                            data.fill(<$ty>::default());
                            return;
                        }
                        if mono.len() < frames {
                            mono.resize(frames, 0.0);
                        }
                        let filled = callback_shared.pull(&mut mono[..frames]);
                        for (index, frame) in data.chunks_mut(channels).enumerate() {
                            let value = if index < filled { mono[index] } else { 0.0 };
                            let converted = convert(value);
                            for slot in frame.iter_mut() {
                                *slot = converted;
                            }
                        }
                    },
                    // 设备错误：记进共享状态，播报报失败；文本结果不受影响。
                    {
                        let shared = shared.clone();
                        move |error| shared.fail(format!("输出设备错误：{error}"))
                    },
                    None,
                )
                .map_err(|error| PlayerError::Open {
                    device: device_label.to_string(),
                    error: error.to_string(),
                })
        }};
    }

    match format {
        F::I8 => open!(i8, |v: f32| (v.clamp(-1.0, 1.0) * i8::MAX as f32) as i8),
        F::I16 => open!(i16, |v: f32| (v.clamp(-1.0, 1.0) * i16::MAX as f32) as i16),
        F::I32 => open!(i32, |v: f32| (v.clamp(-1.0, 1.0) as f64 * i32::MAX as f64)
            as i32),
        F::I64 => open!(i64, |v: f32| (v.clamp(-1.0, 1.0) as f64 * i64::MAX as f64)
            as i64),
        F::U8 => open!(u8, |v: f32| {
            ((v.clamp(-1.0, 1.0) * 127.0) + 128.0) as u8
        }),
        F::U16 => open!(u16, |v: f32| {
            ((v.clamp(-1.0, 1.0) * 32767.0) + 32768.0) as u16
        }),
        F::U32 => open!(u32, |v: f32| {
            ((v.clamp(-1.0, 1.0) as f64 * 0.5 + 0.5) * u32::MAX as f64) as u32
        }),
        F::U64 => open!(u64, |v: f32| {
            ((v.clamp(-1.0, 1.0) as f64 * 0.5 + 0.5) * u64::MAX as f64) as u64
        }),
        F::F32 => open!(f32, |v: f32| v.clamp(-1.0, 1.0)),
        F::F64 => open!(f64, |v: f32| v.clamp(-1.0, 1.0) as f64),
        other => Err(PlayerError::Config(format!(
            "不支持的输出采样格式：{other:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 可控假扬声器：用设备采样率 48k、单声道，手工 `pull` 模拟回调节奏。
    fn fake_shared() -> Arc<PlaybackShared> {
        Arc::new(PlaybackShared::new(48_000, 1))
    }

    fn feed_sine(feed: &mut Feed, shared: &PlaybackShared, frames: usize) {
        let samples: Vec<f32> = (0..frames).map(|i| (i as f32 * 0.05).sin() * 0.4).collect();
        let mut offset = 0;
        // 缓冲有界：接不下就"等设备消费一点"，与真实生产端行为一致。
        let mut guard = 0;
        while offset < samples.len() {
            let accepted = feed
                .push_chunk(shared, &samples[offset..], 48_000, 1)
                .unwrap();
            offset += accepted;
            if accepted == 0 {
                let mut sink = vec![0.0; 480];
                shared.pull(&mut sink);
                guard += 1;
                assert!(guard < 10_000, "缓冲一直满，生产端会永远等下去");
            }
        }
    }

    /// **核心**：「播完」以队列真正排空为准——标记结束但缓冲里还有样本时不能算完。
    #[test]
    fn finished_only_after_buffer_drains() {
        let shared = fake_shared();
        shared.begin(7);
        let mut feed = Feed::new(7);
        feed_sine(&mut feed, &shared, 4800); // 100ms
        let _ = feed.finish(&shared).unwrap();
        shared.mark_end(7);

        let progress = shared.progress();
        assert!(progress.end_marked);
        assert!(progress.buffered_frames > 0);
        assert!(!progress.drained(), "缓冲里还有样本时不能算播完");

        let mut sink = vec![0.0f32; 480];
        let mut pulled = 0usize;
        while !shared.progress().drained() {
            pulled += shared.pull(&mut sink);
            assert!(pulled <= 4800 + 1, "不能凭空多出样本");
        }
        assert_eq!(pulled, 4800);
        let final_progress = shared.progress();
        assert_eq!(final_progress.consumed_frames, 4800);
        assert!(final_progress.started);
    }

    /// 没消费过任何样本（设备没跑）时不能报"播完"。
    #[test]
    fn empty_buffer_without_playback_is_not_finished() {
        let shared = fake_shared();
        shared.begin(1);
        shared.mark_end(1);
        assert!(shared.progress().buffered_frames == 0);
        assert!(!shared.progress().drained(), "设备没消费过，不能算播完");
    }

    /// 停止立即清空队列，并作废当前语音身份；旧 `speech_id` 的后续入队被拒绝。
    #[test]
    fn stop_clears_queue_and_rejects_stale_audio() {
        let shared = fake_shared();
        shared.begin(3);
        let mut feed = Feed::new(3);
        feed_sine(&mut feed, &shared, 2400);
        assert!(shared.buffered_frames() > 0);

        let previous = shared.stop();
        assert_eq!(previous, 3);
        assert_eq!(shared.buffered_frames(), 0);
        assert!(shared.progress().stopped);
        assert!(!shared.progress().drained());

        // 停止后旧身份一个样本都不许进队。
        assert_eq!(shared.push(3, &[0.5; 100]), 0);
        // 新语音可以正常开始。
        shared.begin(4);
        assert_eq!(shared.push(4, &[0.5; 100]), 100);
    }

    /// 跨回合串音：旧 `speech_id` 的迟到音频不得进入新回合。
    #[test]
    fn late_chunks_from_previous_speech_are_dropped() {
        let shared = fake_shared();
        shared.begin(10);
        let mut stale = Feed::new(10);
        shared.stop();
        shared.begin(11);

        // 旧那一段还在往外吐音频：一律被播放层丢掉，一个样本都不许进队。
        let accepted = stale
            .push_chunk(&shared, &[0.2f32; 480], 48_000, 1)
            .expect("格式合法");
        assert_eq!(accepted, 0, "旧语音不能入队");
        assert_eq!(shared.buffered_frames(), 0, "旧语音不能入队");
        // 新那一段照常进队。
        assert_eq!(shared.push(11, &[0.1; 10]), 10);
    }

    /// 有界缓冲：满了就返回 0（生产端等待），不会无限增长。
    #[test]
    fn buffer_is_bounded() {
        let shared = fake_shared();
        shared.begin(1);
        let capacity = (48_000.0 * MAX_BUFFER_SECONDS) as usize;
        assert_eq!(shared.push(1, &vec![0.1; capacity + 1000]), capacity);
        assert_eq!(shared.push(1, &[0.1; 10]), 0);
        assert!(shared.buffered_seconds() <= MAX_BUFFER_SECONDS);
    }

    /// 24k 立体声（交错）→ 48k 单声道：设备侧帧数按比例翻倍，且不再有第二个声道。
    #[test]
    fn feed_resamples_and_downmixes_source_audio() {
        let shared = fake_shared();
        shared.begin(2);
        let mut feed = Feed::new(2);
        let frames = 2400usize; // 100ms @24k
        let interleaved: Vec<f32> = (0..frames)
            .flat_map(|i| {
                let value = (i as f32 * 0.05).sin() * 0.4;
                [value, value]
            })
            .collect();
        // 一次性送完整段：重采样输出比设备缓冲先满时会留在 `pending` 里，
        // 这里像真实生产端那样"等设备消费一点再继续入队"。
        let mut buffered = feed.push_chunk(&shared, &interleaved, 24_000, 2).unwrap();
        let mut guard = 0;
        while feed.has_pending() {
            let mut sink = vec![0.0; 480];
            shared.pull(&mut sink);
            buffered += feed.push_chunk(&shared, &[], 24_000, 2).unwrap();
            guard += 1;
            assert!(guard < 10_000, "缓冲一直满，生产端会永远等下去");
        }
        // 收尾（重采样滤波延迟）同样要排空。
        buffered += feed.finish(&shared).unwrap();
        while feed.has_pending() {
            let mut sink = vec![0.0; 480];
            shared.pull(&mut sink);
            buffered += feed.finish(&shared).unwrap();
            guard += 1;
            assert!(guard < 20_000, "收尾时缓冲一直满");
        }
        let expected = 4800;
        assert!(
            (buffered as f64 - expected as f64).abs() < expected as f64 * 0.03,
            "24k/100ms 转 48k 后应约 {expected} 帧，实际 {buffered}"
        );
        assert!(!feed.has_pending() || feed.pending_len() < 64);
    }

    #[test]
    fn invalid_format_is_rejected_not_silently_dropped() {
        let shared = fake_shared();
        shared.begin(1);
        let mut feed = Feed::new(1);
        assert!(feed.push_chunk(&shared, &[0.1], 0, 1).is_err());
        assert!(feed.push_chunk(&shared, &[0.1], 24_000, 0).is_err());
    }

    /// 回调绝不阻塞：拿不到锁就出静音并计数（用 try_lock 直接验证）。
    #[test]
    fn callback_never_blocks_on_producer_lock() {
        let shared = fake_shared();
        shared.begin(1);
        let guard = shared.inner.lock().unwrap();
        let mut sink = vec![1.0f32; 128];
        let written = shared.pull(&mut sink);
        assert_eq!(written, 0);
        assert!(sink.iter().all(|value| *value == 0.0));
        assert_eq!(shared.lock_misses(), 1);
        drop(guard);
        // 生产端释放后恢复正常。
        shared.push(1, &[0.2; 8]);
        assert_eq!(shared.pull(&mut sink), 8);
    }

    #[test]
    fn state_labels_are_user_readable() {
        assert_eq!(PlaybackState::Synthesizing.code(), "synthesizing");
        assert!(PlaybackState::Playing.is_active());
        assert!(!PlaybackState::Finished.is_active());
        assert!(PlaybackState::Failed("设备断开".into())
            .label()
            .contains("设备断开"));
    }
}
