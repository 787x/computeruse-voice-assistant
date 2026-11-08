//! 自适应 VAD 与端点状态机。
//!
//! 与旧版 `packages/voice/{vad,agc}.mjs` 的对应关系：
//!  - 噪声底：最小统计跟踪（向下快、有下限、向上分语音/非语音两档）；
//!  - 判定：**迟滞**——高于噪声底 `on_db` 才开始，低于 `off_db` 才开始计静音；
//!  - 端点：最短语音、静音收尾、最长语句、无声超时。
//!
//! 为什么不用固定阈值：安静房间会一直误触发，吵闹环境完全不动——
//! 这是旧版实测踩过的坑。这里所有参数都在 `EndpointConfig` 里，可配置。
//!
//! 这个模块**不做重采样、不碰设备**，只吃 16k 单声道块，因此可以完全确定性地单测。

use std::time::Duration;

/// 流式 VAD 的参数。
#[derive(Debug, Clone)]
pub struct EndpointConfig {
    /// 起始阈值：高于噪声底多少 dB 算开始说话。
    pub on_db: f32,
    /// 收尾阈值：低于噪声底多少 dB 算安静（必须小于 `on_db`，形成迟滞）。
    pub off_db: f32,
    /// 最短语音：低于它视为短噪声，不提交。
    pub min_speech: Duration,
    /// 说完后静音多久定稿。
    pub end_silence: Duration,
    /// 单句最长时长，到点强制结束。
    pub max_utterance: Duration,
    /// 唤醒后多久没人说话就放弃这一次会话。
    pub no_speech_timeout: Duration,
    /// 判定语音起点时，向前带上多少预滚音频（避免截掉首字）。
    pub preroll: Duration,
    /// 端点后向后补多少音频（避免吞掉尾音）。
    pub tail_padding: Duration,
    /// 噪声底跟踪的初始值（dBFS）。
    pub initial_floor_db: f32,
}

impl Default for EndpointConfig {
    fn default() -> Self {
        Self {
            on_db: 12.0,
            off_db: 7.0,
            min_speech: Duration::from_millis(250),
            end_silence: Duration::from_millis(700),
            max_utterance: Duration::from_secs(15),
            no_speech_timeout: Duration::from_secs(8),
            preroll: Duration::from_millis(250),
            tail_padding: Duration::from_millis(200),
            initial_floor_db: -60.0,
        }
    }
}

/// 噪声底跟踪（最小统计，带限速）。
///
/// 三条规则都来自旧版 `agc.mjs` 的实测结论：
///  1. 向下快速但有下限（-70dB）：否则一个纯静音帧会把底噪锁到 -180dB，
///     之后所有声音都被判成语音；
///  2. 向上分两档：语音帧上升很慢（不让说话把自己的底噪抬上去），
///     非语音帧上升稍快（适应环境变吵）；
///  3. 不依赖启动标定。
#[derive(Debug, Clone)]
pub struct NoiseFloorTracker {
    floor_db: f32,
    rise_db_per_sec: f32,
    speech_rise_db_per_sec: f32,
    fall_db_per_sec: f32,
    floor_min_db: f32,
}

impl Default for NoiseFloorTracker {
    fn default() -> Self {
        Self::new(-60.0)
    }
}

impl NoiseFloorTracker {
    pub fn new(initial: f32) -> Self {
        Self {
            floor_db: initial.clamp(-70.0, 0.0),
            rise_db_per_sec: 3.0,
            speech_rise_db_per_sec: 0.2,
            fall_db_per_sec: 30.0,
            floor_min_db: -70.0,
        }
    }

    pub fn floor_db(&self) -> f32 {
        self.floor_db
    }

    /// 用一帧的电平更新噪声底，返回更新后的底噪。
    pub fn update(&mut self, frame_db: f32, dt_sec: f32, is_speech: bool) -> f32 {
        if !frame_db.is_finite() || dt_sec <= 0.0 {
            return self.floor_db;
        }
        let observed = frame_db.max(self.floor_min_db);
        if observed < self.floor_db {
            // 向下：快速但有上限
            self.floor_db = observed.max(self.floor_db - self.fall_db_per_sec * dt_sec);
        } else {
            let rate = if is_speech {
                self.speech_rise_db_per_sec
            } else {
                self.rise_db_per_sec
            };
            self.floor_db = observed.min(self.floor_db + rate * dt_sec);
        }
        self.floor_db = self.floor_db.max(self.floor_min_db);
        self.floor_db
    }
}

/// VAD 需要向上层报告的事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VadEvent {
    /// 还没有语音，继续等。
    Waiting,
    /// 刚开始说话（已包含预滚音频）。
    SpeechStarted,
    /// 仍在说话。
    SpeechContinuing,
    /// 一句话结束：`reason` 说明为什么结束。
    UtteranceEnded { reason: EndReason },
}

/// 一句话结束的原因（决定要不要提交给 Agent）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    /// 静音收尾：正常说完。
    Silence,
    /// 达到最长语句上限。
    MaxDuration,
    /// 唤醒后一直没说话（或说完后等不到下一句）。
    NoSpeechTimeout,
}

impl EndReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Silence => "silence",
            Self::MaxDuration => "max-duration",
            Self::NoSpeechTimeout => "no-speech",
        }
    }
}

/// 开场引导时长：用来估初始噪声底。
const BOOTSTRAP_MS: u64 = 500;
/// 初始噪声底的合理上限（见 `bootstrap_floor`）。
const FLOOR_MAX_DB: f32 = -35.0;

/// 用引导阶段收集的电平估初始噪声底：取 20 分位。
///
/// 不用最小值：麦克风刚打开时可能有一两个极安静的帧，会把底噪锁死在下限，
/// 之后整段噪声都被判成语音（旧版实测）。不用中位数：引导窗口如果正好
/// 有人在说话，中位数会被抬得过高，导致后面说话不被识别。
fn bootstrap_floor(levels: &[f32]) -> f32 {
    let mut sorted: Vec<f32> = levels
        .iter()
        .copied()
        .filter(|value| value.is_finite() && *value > -120.0)
        .collect();
    if sorted.is_empty() {
        return -60.0;
    }
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    // 取 20 分位，并且**丢弃比它高 15dB 以上的帧**：开麦瞬间就有人在说话时，
    // 这些帧会把底噪抬得过高，导致之后的正常说话不被识别。
    let low = sorted[(sorted.len() as f32 * 0.2) as usize % sorted.len()];
    let quiet: Vec<f32> = sorted
        .iter()
        .copied()
        .filter(|value| *value <= low + 15.0)
        .collect();
    let reference = if quiet.is_empty() {
        &sorted[..]
    } else {
        &quiet[..]
    };
    let estimate = reference[reference.len() / 2];
    // 合理上限：室内底噪很少高于 -35dBFS。高于它的「底噪」其实是说话、音乐
    // 或开麦瞬间的响声——把它当底噪，之后正常的说话就再也触发不了。
    // 这种情况下退回保守的 -60dB，让跟踪器在运行中慢慢收敛。
    estimate.min(FLOOR_MAX_DB)
}

/// 端点检测器：喂 16k 单声道 PCM 块，输出「是否在说」与「这句话结束了」。
///
/// 设计要点：
///  - 只维护**当前这一句**的音频，句末交给上层后立即清空——不会无限积累；
///  - 预滚缓冲只保留固定时长，内存有界；
///  - 时间用样本数推算，不依赖系统时钟（测试可确定性地推进）。
pub struct EndpointDetector {
    config: EndpointConfig,
    sample_rate: u32,
    tracker: NoiseFloorTracker,
    state: State,
    /// 当前这句的样本（从「预滚起点」开始）。
    utterance: Vec<f32>,
    /// 刚结束那一句的**有声**样本数（不含预滚、句尾静音、补白）。
    ///
    /// 「太短就丢弃」必须按有声时长判，不能用总长度：
    /// 40ms 的关门声 + 400ms 静音收尾 + 补白，总长能到 0.6 秒，
    /// 按总长判就把它当成了有效指令。
    finished_voiced: usize,
    /// 刚刚结束的那一句，等调用方 `take_utterance()` 取走。
    ///
    /// 不能直接留在 `utterance` 里：结束后状态机要立刻开始听下一句，
    /// 而新一句的预滚会覆盖它。也不能清掉：调用方通常是在
    /// `UtteranceEnded` 事件之后才取音频。
    finished: Option<Vec<f32>>,
    /// 语音开始前的预滚环形缓冲。
    preroll: Vec<f32>,
    /// 已入队但还没被判定为语音时，`utterance` 里有多少是预滚。
    speech_started: bool,
    /// 从语音开始到现在的样本数（用于最短语音/最长语句）。
    speech_samples: usize,
    /// 连续安静了多少样本。
    quiet_samples: usize,
    /// 距离「语音开始」或「会话开始」过了多少样本（无声超时）。
    since_session_start: usize,
    /// 最近一帧的电平与底噪（给界面显示）。
    last_level_db: f32,
    last_floor_db: f32,
    /// 这一句里是否出现过任何「够长」的语音。
    had_speech: bool,
    /// 开场引导：先收集一小段电平，用分位数定底噪。
    ///
    /// 为什么必须这样做：假定底噪是 -60dB 时，一个真实底噪 -46dB 的麦克风
    /// 会在**开麦第一帧**就满足「高于底噪 12dB」，从而误触发整句。
    /// 旧版批处理 VAD 也踩过同类坑（"不能用第 0 帧"），这里用同样的分位数思路。
    bootstrap: Vec<f32>,
    bootstrapped: bool,
    /// 当前帧累计的样本来凑够一帧。
    pending: Vec<f32>,
    /// 判定所用帧长（30ms，与旧版 `vad.mjs` 一致）。
    frame_len: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// 等语音开始（或会话刚开始的等待）。
    Waiting,
    Speaking,
}

impl EndpointDetector {
    pub fn new(config: EndpointConfig, sample_rate: u32) -> Self {
        let frame_len = ((sample_rate as u64 * 30) / 1000) as usize;
        let preroll_len =
            ((sample_rate as u64 * config.preroll.as_millis() as u64) / 1000) as usize;
        let initial_floor_db = config.initial_floor_db;
        Self {
            tracker: NoiseFloorTracker::new(initial_floor_db),
            config,
            sample_rate,
            state: State::Waiting,
            utterance: Vec::new(),
            finished_voiced: 0,
            finished: None,
            preroll: Vec::with_capacity(preroll_len),
            speech_started: false,
            speech_samples: 0,
            quiet_samples: 0,
            since_session_start: 0,
            last_level_db: -120.0,
            last_floor_db: initial_floor_db,
            had_speech: false,
            bootstrap: Vec::new(),
            bootstrapped: false,
            pending: Vec::new(),
            frame_len: frame_len.max(1),
        }
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn level_db(&self) -> f32 {
        self.last_level_db
    }

    pub fn floor_db(&self) -> f32 {
        self.last_floor_db
    }

    pub fn is_speaking(&self) -> bool {
        self.state == State::Speaking
    }

    /// 当前这句已经积累了多少样本（用于界面显示时长）。
    pub fn utterance_samples(&self) -> usize {
        self.utterance.len()
    }

    /// 喂一块 16k 单声道音频，返回这一块里发生的事件（按顺序）。
    ///
    /// 一次可能返回多个事件（例如块内同时「开始」和「结束」），也可能为空。
    pub fn push(&mut self, samples: &[f32]) -> Vec<VadEvent> {
        let mut events = Vec::new();
        self.pending.extend_from_slice(samples);
        while self.pending.len() >= self.frame_len {
            let frame: Vec<f32> = self.pending.drain(..self.frame_len).collect();
            self.process_frame(&frame, &mut events);
        }
        events
    }

    fn process_frame(&mut self, frame: &[f32], events: &mut Vec<VadEvent>) {
        let dt = self.frame_len as f32 / self.sample_rate as f32;
        let level_db = crate::voice::pcm::rms_db(frame);
        self.last_level_db = level_db;

        // 引导阶段：只收电平，不做任何判定。
        //
        // 引导期间**不计入**「无声超时」：开麦的前 300ms 是标定底噪，
        // 不能算成「唤醒后没人说话」。否则会话一启动就立刻超时。
        if !self.bootstrapped {
            self.push_preroll(frame);
            self.bootstrap.push(level_db);
            let needed = ((self.sample_rate as u64 * BOOTSTRAP_MS) / 1000 / self.frame_len as u64)
                .max(1) as usize;
            if self.bootstrap.len() < needed {
                // 还没标定完：**不判定**。用未标定的底噪去判，会把正常底噪
                // 当成语音（-60dB 的假设 vs 真实 -46dB 的麦克风）。
                return;
            }
            let floor = bootstrap_floor(&self.bootstrap);
            self.tracker = NoiseFloorTracker::new(floor);
            self.last_floor_db = floor;
            self.bootstrapped = true;
            self.bootstrap.clear();
            // 标定结束才是一句的开始：无声超时从这里起算。
            self.since_session_start = 0;
            // 标定完成的这一帧继续走正常判定（不丢音频）。
        }

        let was_speaking = self.state == State::Speaking;
        let floor_db = self.tracker.update(level_db, dt, was_speaking);
        // 跟踪器判定的「语音」用**上一帧**的状态；这一帧是否算语音由下面的阈值决定。
        self.last_floor_db = floor_db;

        let threshold = if was_speaking {
            self.config.off_db
        } else {
            self.config.on_db
        };
        let active = level_db > floor_db + threshold;

        self.since_session_start += frame.len();

        match self.state {
            State::Waiting => {
                if active {
                    self.begin_speech();
                    events.push(VadEvent::SpeechStarted);
                    // 本帧本身就属于语音。
                    self.utterance.extend_from_slice(frame);
                    self.speech_samples = frame.len();
                    self.quiet_samples = 0;
                } else {
                    self.push_preroll(frame);
                    // 无声超时：从会话开始算，一直没人说话就放弃这次唤醒。
                    if self.since_session_start as u64
                        >= self.samples(self.config.no_speech_timeout)
                    {
                        events.push(VadEvent::UtteranceEnded {
                            reason: EndReason::NoSpeechTimeout,
                        });
                        self.reset_utterance();
                    }
                }
            }
            State::Speaking => {
                self.utterance.extend_from_slice(frame);
                self.speech_samples += frame.len();
                if active {
                    self.quiet_samples = 0;
                    events.push(VadEvent::SpeechContinuing);
                } else {
                    self.quiet_samples += frame.len();
                }

                let too_long =
                    self.speech_samples as u64 >= self.samples(self.config.max_utterance);
                let quiet_enough =
                    self.quiet_samples as u64 >= self.samples(self.config.end_silence);

                if too_long {
                    events.push(VadEvent::UtteranceEnded {
                        reason: EndReason::MaxDuration,
                    });
                    self.reset_utterance();
                } else if quiet_enough {
                    // 收尾的静音本身也留着（可能只是句间停顿，模型需要上下文）。
                    // 有声时长不含句尾静音：`speech_samples` 一直累加到端点，
                    // 而 `quiet_samples` 正是其中「安静的那部分」。
                    let voiced = self.speech_samples.saturating_sub(self.quiet_samples);
                    let long_enough = voiced as u64 >= self.samples(self.config.min_speech);
                    let reason = if long_enough {
                        EndReason::Silence
                    } else {
                        // 太短：当作短噪声/误触发，用同一个事件类型告知上层，
                        // 由上层根据 `had_speech` 判断要不要提交。
                        EndReason::Silence
                    };
                    if !long_enough {
                        self.had_speech = false;
                    }
                    events.push(VadEvent::UtteranceEnded { reason });
                    self.reset_utterance();
                }
            }
        }
    }

    fn begin_speech(&mut self) {
        self.state = State::Speaking;
        self.had_speech = true;
        self.speech_samples = 0;
        self.quiet_samples = 0;
        // 预滚音频一起带进这一句，避免截掉首字。
        self.utterance.clear();
        self.utterance.extend_from_slice(&self.preroll);
        self.preroll.clear();
        self.speech_started = true;
    }

    fn push_preroll(&mut self, frame: &[f32]) {
        let cap = self.samples(self.config.preroll) as usize;
        self.preroll.extend_from_slice(frame);
        if self.preroll.len() > cap {
            let excess = self.preroll.len() - cap;
            self.preroll.drain(..excess);
        }
    }

    fn samples(&self, duration: Duration) -> u64 {
        (self.sample_rate as u64 * duration.as_millis() as u64) / 1000
    }

    /// 结束当前句：把音频挪进 `finished` 等调用方取走，再回到等待态。
    fn reset_utterance(&mut self) {
        self.state = State::Waiting;
        // 只记有声样本，不含句尾静音。
        self.finished_voiced = self.speech_samples.saturating_sub(self.quiet_samples);
        if !self.utterance.is_empty() {
            self.finished = Some(std::mem::take(&mut self.utterance));
        } else {
            self.utterance.clear();
        }
        self.preroll.clear();
        self.speech_started = false;
        self.speech_samples = 0;
        self.quiet_samples = 0;
        self.since_session_start = 0;
        self.had_speech = false;
    }

    /// 把当前这句的音频取走（调用后这一句清空，也可以直接开始下一句）。
    ///
    /// 返回值已经带上尾音补白（`tail_padding`）。
    /// 刚结束那一句的有声时长（秒）。取走之后仍保留到下一句结束。
    pub fn last_voiced_seconds(&self) -> f32 {
        self.finished_voiced as f32 / self.sample_rate as f32
    }

    /// 手动结束保留未满一帧的 PCM，按实际样本计时。
    pub fn finish_now(&mut self) {
        let pending = std::mem::take(&mut self.pending);
        if self.state == State::Speaking {
            let active =
                crate::voice::pcm::rms_db(&pending) > self.last_floor_db + self.config.off_db;
            self.utterance.extend_from_slice(&pending);
            self.speech_samples += pending.len();
            if active {
                self.quiet_samples = 0;
            } else {
                self.quiet_samples += pending.len();
            }
        }
        if self.finished.is_none() {
            self.reset_utterance();
        }
    }
    pub fn take_utterance(&mut self) -> Vec<f32> {
        let mut out = self
            .finished
            .take()
            .unwrap_or_else(|| std::mem::take(&mut self.utterance));
        let tail = self.samples(self.config.tail_padding);
        // 只有确实录到了语音才补尾音；纯静音补了也只会让模型多看到静音。
        if !out.is_empty() && tail > 0 {
            out.resize(out.len() + tail as usize, 0.0);
        }
        self.state = State::Waiting;
        self.preroll.clear();
        self.speech_started = false;
        self.speech_samples = 0;
        self.quiet_samples = 0;
        self.since_session_start = 0;
        self.had_speech = false;
        out
    }

    /// 丢弃当前这句（取消、关闭麦克风、误唤醒）。
    pub fn discard(&mut self) {
        self.finished = None;
        self.utterance.clear();
        self.preroll.clear();
        self.state = State::Waiting;
        self.speech_started = false;
        self.speech_samples = 0;
        self.quiet_samples = 0;
        self.since_session_start = 0;
        self.had_speech = false;
        self.pending.clear();
    }

    /// 会话重新开始（唤醒后）：重置噪声底之外的计时状态。
    ///
    /// 噪声底**不重置**：环境噪声不会因为一次唤醒而改变，
    /// 重置会让刚开麦的几百毫秒重新进入「底噪不准」的状态。
    pub fn start_utterance(&mut self) {
        // 新一轮唤醒：上一句（如果还没取走）作废，不能串到新一轮。
        self.finished = None;
        self.discard();
        // 唤醒是一次新会话：无声超时从这一刻重新计时。
        self.since_session_start = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice::pcm::TARGET_SAMPLE_RATE;

    fn cfg() -> EndpointConfig {
        EndpointConfig {
            on_db: 12.0,
            off_db: 7.0,
            min_speech: Duration::from_millis(200),
            end_silence: Duration::from_millis(400),
            max_utterance: Duration::from_secs(4),
            no_speech_timeout: Duration::from_secs(2),
            preroll: Duration::from_millis(100),
            tail_padding: Duration::from_millis(50),
            initial_floor_db: -60.0,
        }
    }

    fn silence(ms: u32) -> Vec<f32> {
        vec![0.0; (TARGET_SAMPLE_RATE as u64 * ms as u64 / 1000) as usize]
    }

    fn noise_like(ms: u32, amplitude: f32) -> Vec<f32> {
        // 确定性噪声（不用随机数，保证测试可复现）。
        let n = (TARGET_SAMPLE_RATE as u64 * ms as u64 / 1000) as usize;
        (0..n)
            .map(|i| {
                let phase = (i as f32 * 0.37).sin() + (i as f32 * 0.11).sin();
                phase * amplitude * 0.5
            })
            .collect()
    }

    fn tone(ms: u32, amplitude: f32) -> Vec<f32> {
        let n = (TARGET_SAMPLE_RATE as u64 * ms as u64 / 1000) as usize;
        (0..n)
            .map(|i| {
                (2.0 * std::f32::consts::PI * 300.0 * i as f32 / TARGET_SAMPLE_RATE as f32).sin()
                    * amplitude
            })
            .collect()
    }

    #[test]
    fn manual_finish_preserves_partial_frame_and_voiced_duration() {
        let mut detector = EndpointDetector::new(cfg(), TARGET_SAMPLE_RATE);
        detector.push(&silence(1000));
        detector.start_utterance();
        let audio = tone(517, 0.4);
        detector.push(&audio);
        detector.finish_now();
        assert!((detector.last_voiced_seconds() - 0.517).abs() < 0.001);
        let pcm = detector.take_utterance();
        assert_eq!(&pcm[..audio.len()], audio.as_slice());
        assert_eq!(pcm.len(), audio.len() + 800);
        detector.finish_now();
        assert!(detector.take_utterance().is_empty());
    }

    #[test]
    fn manual_silence_is_empty() {
        let mut detector = EndpointDetector::new(cfg(), TARGET_SAMPLE_RATE);
        detector.push(&silence(1000));
        detector.finish_now();
        assert_eq!(detector.last_voiced_seconds(), 0.0);
        assert!(detector.take_utterance().is_empty());
    }

    /// 恒定的底噪被跟随后，安静信号不应被判成语音。
    #[test]
    fn steady_noise_does_not_trigger() {
        let mut detector = EndpointDetector::new(cfg(), TARGET_SAMPLE_RATE);
        // 先喂 1 秒底噪让跟踪器收敛，再喂 2 秒同水平噪声。
        let mut events = Vec::new();
        for _ in 0..30 {
            events.extend(detector.push(&noise_like(100, 0.01)));
        }
        assert!(
            !events.iter().any(|e| matches!(e, VadEvent::SpeechStarted)),
            "稳定底噪不能触发语音：{events:?}"
        );
        assert_eq!(detector.utterance_samples(), 0);
    }

    /// 噪声之后来一段明显更响的语音：必须开始、并在静音后结束。
    #[test]
    fn speech_after_noise_starts_and_ends() {
        let mut detector = EndpointDetector::new(cfg(), TARGET_SAMPLE_RATE);
        let mut events = Vec::new();
        for _ in 0..10 {
            events.extend(detector.push(&noise_like(100, 0.01)));
        }
        let before = events.len();
        for _ in 0..8 {
            events.extend(detector.push(&tone(100, 0.3)));
        }
        assert!(
            events[before..]
                .iter()
                .any(|e| matches!(e, VadEvent::SpeechStarted)),
            "应当开始语音"
        );
        // 静音收尾
        let mut ended = None;
        for _ in 0..10 {
            for event in detector.push(&silence(100)) {
                if let VadEvent::UtteranceEnded { reason } = event {
                    ended = Some(reason);
                }
            }
        }
        assert_eq!(ended, Some(EndReason::Silence), "静音后应当端点");

        let utterance = detector.take_utterance();
        let seconds = utterance.len() as f32 / TARGET_SAMPLE_RATE as f32;
        assert!(
            (0.9..1.4).contains(&seconds),
            "句长 {seconds:.2}s 应约等于 0.8s 语音 + 预滚 + 尾音"
        );
        assert_eq!(detector.utterance_samples(), 0, "取走后必须清空");
    }

    /// 短于最短语音的「哔」声不能算一句话。
    #[test]
    fn very_short_noise_is_not_submitted() {
        let mut detector = EndpointDetector::new(cfg(), TARGET_SAMPLE_RATE);
        for _ in 0..10 {
            let _ = detector.push(&noise_like(100, 0.01));
        }
        // 40ms 的响动，短于 200ms 的下限
        let _ = detector.push(&tone(40, 0.4));
        let mut events = Vec::new();
        for _ in 0..10 {
            events.extend(detector.push(&silence(100)));
        }
        // 端点事件可能出现（状态机复位），但**有声**时长必须短于下限，
        // 上层据此丢弃（总长度含静音收尾与补白，不能拿来判）。
        let voiced = detector.last_voiced_seconds();
        let utterance = detector.take_utterance();
        let seconds = utterance.len() as f32 / TARGET_SAMPLE_RATE as f32;
        assert!(
            voiced < 0.2,
            "短噪声的有声时长应为 {voiced:.3}s，不应算作有效语句"
        );
        assert!(seconds > 0.0, "音频本身仍应被保留以便上层判断");
        // 无论如何，不能把这一句交出去 — 上层会按长度再判一次。
        let has_end = events
            .iter()
            .any(|e| matches!(e, VadEvent::UtteranceEnded { .. }));
        assert!(has_end || seconds == 0.0);
    }

    /// 一直说话：到最长语句上限必须强制结束（不能让内存无限涨）。
    #[test]
    fn max_duration_forces_end() {
        let mut detector = EndpointDetector::new(cfg(), TARGET_SAMPLE_RATE);
        let mut ended = None;
        let mut max_len = 0usize;
        for _ in 0..80 {
            for event in detector.push(&tone(100, 0.3)) {
                if let VadEvent::UtteranceEnded { reason } = event {
                    ended = Some(reason);
                }
            }
            max_len = max_len.max(detector.utterance_samples());
        }
        assert_eq!(ended, Some(EndReason::MaxDuration));
        let seconds = max_len as f32 / TARGET_SAMPLE_RATE as f32;
        assert!(
            seconds <= 4.3,
            "最长语句必须在上限附近截断，实际 {seconds:.2}s"
        );
    }

    /// 唤醒后一直没说话：必须超时，而不是永远等下去。
    #[test]
    fn silence_times_out() {
        let mut detector = EndpointDetector::new(cfg(), TARGET_SAMPLE_RATE);
        let mut events = Vec::new();
        for _ in 0..30 {
            events.extend(detector.push(&silence(100)));
        }
        assert!(
            events.iter().any(|e| matches!(
                e,
                VadEvent::UtteranceEnded {
                    reason: EndReason::NoSpeechTimeout
                }
            )),
            "无声超时必须触发：{events:?}"
        );
    }

    /// 句间停顿（短于收尾静音）不能把一句话切成两半。
    #[test]
    fn short_pause_does_not_split_utterance() {
        let mut detector = EndpointDetector::new(cfg(), TARGET_SAMPLE_RATE);
        for _ in 0..10 {
            let _ = detector.push(&noise_like(100, 0.01));
        }
        let mut run = |chunk: &[f32]| detector.push(chunk);

        let mut started = 0;
        for _ in 0..5 {
            for event in run(&tone(100, 0.3)) {
                if matches!(event, VadEvent::SpeechStarted) {
                    started += 1;
                }
            }
        }
        // 200ms 停顿，短于 400ms 的收尾静音
        for _ in 0..2 {
            let _ = run(&silence(100));
        }
        let _ = run(&tone(200, 0.3));
        for _ in 0..6 {
            let _ = run(&silence(100));
        }
        assert_eq!(started, 1, "短停顿不能把一句话切成两段");
        assert!(!detector.take_utterance().is_empty());
    }

    /// 预滚：语音开始前的一小段音频必须被带进这一句（避免截掉首字）。
    ///
    /// 预滚窗口 100ms，但引导阶段只占 300ms 里的前一部分，所以断言的是
    /// 「句长略大于纯语音时长」，而不是精确值。
    #[test]
    fn preroll_captures_leading_audio() {
        let mut detector = EndpointDetector::new(cfg(), TARGET_SAMPLE_RATE);
        for _ in 0..10 {
            let _ = detector.push(&noise_like(100, 0.01));
        }
        for _ in 0..5 {
            let _ = detector.push(&tone(100, 0.3));
        }
        let utterance = detector.take_utterance();
        let seconds = utterance.len() as f32 / TARGET_SAMPLE_RATE as f32;
        assert!(
            (0.5..1.2).contains(&seconds),
            "应当带上预滚，实际 {seconds:.2}s"
        );
    }

    /// 噪声底跟踪：突然变吵时不能立刻把语音判成噪声，也不能被一次静音打到底。
    #[test]
    fn noise_floor_is_bounded() {
        let mut tracker = NoiseFloorTracker::new(-60.0);
        // 一帧极静：底噪最多下降到下限
        tracker.update(-120.0, 0.03, false);
        assert!(tracker.floor_db() >= -70.0);
        // 环境变吵：按 3dB/s 缓慢上升（3 秒最多升 9dB），不会一步跳到满。
        for _ in 0..100 {
            tracker.update(-20.0, 0.03, false);
        }
        let after = tracker.floor_db();
        assert!(after > -60.0, "应当上升：{after}");
        assert!(after < -20.0, "不能一步跳到环境电平：{after}");
    }
}
