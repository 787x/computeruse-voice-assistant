//! 音频前端的纯函数部分：声道下混、样本格式转换、重采样、RMS 电平。
//!
//! 这里**不碰任何设备**，输入输出都是普通 `Vec<f32>`，所以可以确定性地单测。
//! 采集线程只做「格式转换 + 下混 + 重采样」，模型推理一律不在这里做。
//!
//! 重采样刻意不用「每隔 N 个样本取一个」的朴素做法：
//! 那样 44.1k → 16k 会把语音频谱折回去，KWS 和 Whisper 都会明显变差。
//! 这里用带抗混叠低通的加窗 sinc 插值（rubato 的 `SincFixedIn`），
//! 与旧版 `tools/verify-wake-runtime.mjs` 里「手写插值糊掉关键词」的实测结论一致。

/// 语音流水线统一的采样率。
pub const TARGET_SAMPLE_RATE: u32 = 16_000;

/// 把交错的多声道样本下混成单声道：等权平均。
///
/// 等权平均而不是「只取第 0 声道」：USB 麦克风阵列经常把有效信号放在非 0 声道，
/// 只取第 0 声道会得到近乎静音。等权平均对常见立体声/单声道都成立。
pub fn downmix_interleaved(samples: &[f32], channels: usize) -> Vec<f32> {
    if channels <= 1 {
        return samples.to_vec();
    }
    let frames = samples.len() / channels;
    let mut out = Vec::with_capacity(frames);
    let scale = 1.0 / channels as f32;
    for frame in 0..frames {
        let base = frame * channels;
        let sum: f32 = samples[base..base + channels].iter().sum();
        out.push(sum * scale);
    }
    out
}

/// 有符号整型样本转 `[-1, 1]` 浮点。
///
/// `bits` 是**有效位深**（i16 为 16、i24 为 24、i64 为 64），不是存储宽度：
/// CPAL 的 `I24` 用 4 字节存 24 位有效数据，按 32 位归一化会让音量小 256 倍。
///
/// 64 位也必须真的换算：以前 `bits > 32` 直接返回 0.0，`I64` 设备会得到
/// **整段全零**的音频——VAD 永远不触发、KWS 永远听不到，而且没有任何报错。
/// 用 `f64` 的 2 的幂做分母（对 2^n 精确），64 位也能线性归一化。
pub fn int_to_float(value: i64, bits: u32) -> f32 {
    if bits == 0 || bits > 64 {
        return 0.0;
    }
    // 除以 2^(bits-1) 而不是 2^(bits-1)-1：负数端点也能落到 -1.0，
    // 并保持线性（用 max 归一化会让正负半轴增益不一致）。
    let scale = 2f64.powi(bits as i32 - 1);
    (value as f64 / scale).clamp(-1.0, 1.0) as f32
}

/// 一帧的 RMS 电平（dBFS）。用于电平显示与 VAD 的粗判。
pub fn rms_db(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return -120.0;
    }
    let sum: f32 = samples.iter().map(|value| value * value).sum();
    let mean = sum / samples.len() as f32;
    if !mean.is_finite() || mean <= 0.0 {
        return -120.0;
    }
    (10.0 * mean.log10()).max(-120.0)
}

/// 峰值（`0.0..=1.0` 之外也如实返回，调用方自己决定要不要削顶）。
pub fn peak(samples: &[f32]) -> f32 {
    samples
        .iter()
        .fold(0.0f32, |acc, value| acc.max(value.abs()))
}

use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};

/// 流式重采样器：44.1k / 48k / 32k 等设备采样率 → 16k。
///
/// 用 rubato 的 `SincFixedIn`（固定输入块、变化输出块）。它内部保留滤波器状态，
/// 所以可以一块一块地喂，块边界不会有咔哒声或相位跳变。
///
/// 采样率本来就是 16k 时不构造重采样器，直接透传（避免无意义开销与延迟）。
pub struct StreamingResampler {
    inner: Option<SincFixedIn<f32>>,
    /// 攒够一个输入块才送进 rubato；不足的部分留在缓冲里。
    pending: Vec<f32>,
    /// rubato 当前要求的输入帧数（构造时固定）。
    chunk_frames: usize,
    input_rate: u32,
    /// 已丢弃的样本数（输出端）。设备失效重建时用于对齐时间。
    total_in: u64,
    total_out: u64,
}

/// rubato 每次处理的输入帧数。40ms 一块：延迟可控，CPU 也不至于每 10ms 跑一次滤波。
const RESAMPLE_CHUNK_FRAMES: usize = 640;

impl StreamingResampler {
    pub fn new(input_rate: u32) -> Result<Self, String> {
        if input_rate == 0 {
            return Err("设备采样率无效（0）".to_string());
        }
        if input_rate == TARGET_SAMPLE_RATE {
            return Ok(Self {
                inner: None,
                pending: Vec::new(),
                chunk_frames: RESAMPLE_CHUNK_FRAMES,
                input_rate,
                total_in: 0,
                total_out: 0,
            });
        }
        let ratio = TARGET_SAMPLE_RATE as f64 / input_rate as f64;
        let parameters = SincInterpolationParameters {
            sinc_len: 128,
            f_cutoff: 0.925,
            interpolation: SincInterpolationType::Cubic,
            oversampling_factor: 128,
            window: WindowFunction::BlackmanHarris2,
        };
        let inner = SincFixedIn::<f32>::new(ratio, 1.0, parameters, RESAMPLE_CHUNK_FRAMES, 1)
            .map_err(|error| format!("无法构造重采样器（{input_rate} → 16000）：{error}"))?;
        Ok(Self {
            inner: Some(inner),
            pending: Vec::with_capacity(RESAMPLE_CHUNK_FRAMES * 2),
            chunk_frames: RESAMPLE_CHUNK_FRAMES,
            input_rate,
            total_in: 0,
            total_out: 0,
        })
    }

    pub fn input_rate(&self) -> u32 {
        self.input_rate
    }

    pub fn is_passthrough(&self) -> bool {
        self.inner.is_none()
    }

    /// 喂一块**单声道**样本（已下混），返回这次能输出的 16k 样本。
    ///
    /// 输出长度不固定（与输入块长度、滤波延迟有关），调用方不要假设一一对应。
    pub fn push(&mut self, mono: &[f32]) -> Result<Vec<f32>, String> {
        self.total_in += mono.len() as u64;
        let Some(inner) = self.inner.as_mut() else {
            self.total_out += mono.len() as u64;
            return Ok(mono.to_vec());
        };

        self.pending.extend_from_slice(mono);
        let mut out: Vec<f32> = Vec::new();
        while self.pending.len() >= self.chunk_frames {
            let chunk: Vec<f32> = self.pending.drain(..self.chunk_frames).collect();
            let processed = inner
                .process(&[chunk], None)
                .map_err(|error| format!("重采样失败：{error}"))?;
            if let Some(channel) = processed.first() {
                out.extend_from_slice(channel);
            }
        }
        self.total_out += out.len() as u64;
        Ok(out)
    }

    /// 收尾：把滤波延迟里剩下的样本冲出来。停止采集时调用一次。
    pub fn flush(&mut self) -> Result<Vec<f32>, String> {
        let Some(inner) = self.inner.as_mut() else {
            return Ok(Vec::new());
        };
        if self.pending.is_empty() {
            return Ok(Vec::new());
        }
        // 不足一块也要处理：补零进去，再从中截掉补出来的部分。
        let mut padded = std::mem::take(&mut self.pending);
        let pad = self.chunk_frames - padded.len();
        padded.resize(self.chunk_frames, 0.0);
        let processed = inner
            .process(&[padded], None)
            .map_err(|error| format!("重采样收尾失败：{error}"))?;
        let mut out = processed.into_iter().next().unwrap_or_default();
        // 按比例截掉补零对应的输出，避免在句尾塞进一段静音。
        let keep = (out.len() as f64 * (self.chunk_frames - pad) as f64 / self.chunk_frames as f64)
            .round() as usize;
        out.truncate(keep.min(out.len()));
        self.total_out += out.len() as u64;
        Ok(out)
    }

    pub fn totals(&self) -> (u64, u64) {
        (self.total_in, self.total_out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f32, rate: u32, seconds: f32) -> Vec<f32> {
        let n = (rate as f32 * seconds) as usize;
        (0..n)
            .map(|i| (2.0 * std::f32::consts::PI * freq * i as f32 / rate as f32).sin() * 0.5)
            .collect()
    }

    /// 归一化能量：用来判断重采样后还是不是同一个信号。
    fn energy(samples: &[f32]) -> f32 {
        samples.iter().map(|v| v * v).sum::<f32>() / samples.len().max(1) as f32
    }

    #[test]
    fn downmix_averages_channels() {
        // 立体声：左 1.0 / 右 0.0 → 0.5；单声道原样返回。
        let stereo = vec![1.0, 0.0, -1.0, -1.0];
        assert_eq!(downmix_interleaved(&stereo, 2), vec![0.5, -1.0]);
        assert_eq!(downmix_interleaved(&stereo, 1), stereo.clone());
        // 声道数不整除时，最后不完整的一帧被丢掉（不 panic、不读越界）。
        assert_eq!(downmix_interleaved(&[0.2, 0.4, 0.6], 2), vec![0.6f32 * 0.5]);
    }

    #[test]
    fn int_scaling_uses_effective_bit_depth() {
        assert!((int_to_float(i16::MAX as i64, 16) - 0.999_969_5).abs() < 1e-6);
        assert_eq!(int_to_float(i16::MIN as i64, 16), -1.0);
        assert_eq!(int_to_float(0, 16), 0.0);
        // 24 位有效位、32 位存储：按 24 位归一化，而不是按 32 位。
        assert!((int_to_float(1 << 22, 24) - 0.5).abs() < 1e-6);
        // 越界/非法位深不 panic。
        assert_eq!(int_to_float(1, 0), 0.0);
        assert_eq!(int_to_float(i64::MAX, 16), 1.0);
    }

    /// **回归**：64 位整型样本不能静默变成全零。
    ///
    /// CPAL 的 `I64` 采样格式会走到这里；以前 `bits > 32` 直接返回 0.0，
    /// 于是整段音频是静音，表现为「麦克风没坏但永远听不到」。
    #[test]
    fn int64_samples_are_not_silently_zero() {
        assert_eq!(int_to_float(0, 64), 0.0);
        assert_eq!(int_to_float(i64::MIN, 64), -1.0);
        assert!((int_to_float(i64::MAX, 64) - 1.0).abs() < 1e-6);
        // 半幅：2^62 对应 0.5
        assert!((int_to_float(1i64 << 62, 64) - 0.5).abs() < 1e-6);
        // 小信号也必须保留（不是被 clamp 或截断成 0）
        assert!(int_to_float(1i64 << 30, 64) > 0.0);

        // 所有整数位深在「中点」都必须给出非零幅度：
        // 任何一个变成 0 都意味着该采样格式的音频是静音。
        for bits in [8u32, 16, 24, 32, 64] {
            let half = if bits == 64 {
                1i64 << 62
            } else {
                1i64 << (bits - 2)
            };
            let value = int_to_float(half, bits);
            assert!(
                (value - 0.5).abs() < 1e-6,
                "{bits} 位半幅应当映射到 0.5，实际 {value}"
            );
        }

        // 超过 64 位不是任何真实设备格式：仍然返回 0，但不会 panic。
        assert_eq!(int_to_float(i64::MAX, 65), 0.0);
    }

    #[test]
    fn rms_and_peak_are_sane() {
        assert_eq!(rms_db(&[]), -120.0);
        assert_eq!(rms_db(&[0.0, 0.0]), -120.0);
        // 幅度 0.5 的正弦：RMS = 0.5/√2 ≈ 0.354，即 -9.03 dBFS。
        let s = sine(440.0, 16_000, 0.1);
        assert!((rms_db(&s) - (-9.03)).abs() < 0.2, "rms = {}", rms_db(&s));
        // 满幅正弦（幅度 1.0）才是 -3.01 dBFS。
        let full = sine(440.0, 16_000, 0.1)
            .iter()
            .map(|v| v * 2.0)
            .collect::<Vec<_>>();
        assert!(
            (rms_db(&full) - (-3.01)).abs() < 0.2,
            "rms = {}",
            rms_db(&full)
        );
        assert!((peak(&s) - 0.5).abs() < 0.01);
    }

    #[test]
    fn passthrough_at_16k_is_exact() {
        let mut resampler = StreamingResampler::new(16_000).unwrap();
        assert!(resampler.is_passthrough());
        let input = sine(440.0, 16_000, 0.05);
        let out = resampler.push(&input).unwrap();
        assert_eq!(out, input, "16k 输入必须原样透传");
        assert!(resampler.flush().unwrap().is_empty());
    }

    #[test]
    fn rejects_invalid_rate() {
        assert!(StreamingResampler::new(0).is_err());
    }

    /// 核心回归：44.1k / 48k 重采样后仍是同一个 440Hz 音，
    /// 长度按比例变化，能量不明显走样（丢样本的朴素做法会把能量和音高搞坏）。
    #[test]
    fn resamples_44k_and_48k_preserving_signal() {
        for rate in [44_100u32, 48_000, 32_000] {
            let second = 1.0;
            let input = sine(440.0, rate, second);
            let mut resampler = StreamingResampler::new(rate).unwrap();
            assert!(!resampler.is_passthrough());

            let mut out = Vec::new();
            // 模拟设备回调：每次 441/480/320 帧，分多次喂。
            let step = (rate as usize) / 100;
            for chunk in input.chunks(step) {
                out.extend(resampler.push(chunk).unwrap());
            }
            out.extend(resampler.flush().unwrap());

            let expected = input.len() as f64 * 16_000.0 / rate as f64;
            assert!(
                (out.len() as f64 - expected).abs() < expected * 0.02,
                "{rate}Hz: 输出 {} 帧，期望约 {expected:.0}",
                out.len()
            );
            // 去掉滤波器启动/收尾的边界，比较中段的能量。
            let mid = &out[out.len() / 4..out.len() * 3 / 4];
            let input_energy = energy(&input[input.len() / 4..input.len() * 3 / 4]);
            let ratio = energy(mid) / input_energy;
            assert!(
                (ratio - 1.0).abs() < 0.15,
                "{rate}Hz: 重采样后能量比 {ratio:.3} 偏离 1.0 太多"
            );
            // 输出必须仍在 [-1, 1] 附近（sinc 过冲允许少量）。
            assert!(peak(mid) < 0.8, "{rate}Hz: 峰值 {} 异常", peak(mid));
        }
    }

    /// 高频（接近原采样率奈奎斯特）必须被低通压下去，而不是折叠成可听频率。
    /// 这是「不能只丢样本」的直接证据。
    #[test]
    fn resampling_suppresses_images_above_nyquist() {
        let rate = 48_000u32;
        // 20kHz 在 48k 下合法；重采样到 16k 后 >8kHz 的内容应被滤掉。
        let input = sine(20_000.0, rate, 1.0);
        let mut resampler = StreamingResampler::new(rate).unwrap();
        let mut out = Vec::new();
        for chunk in input.chunks(480) {
            out.extend(resampler.push(chunk).unwrap());
        }
        out.extend(resampler.flush().unwrap());
        let mid = &out[out.len() / 4..out.len() * 3 / 4];
        let input_energy = energy(&input);
        let ratio = energy(mid) / input_energy;
        assert!(ratio < 0.05, "20kHz 混叠压制不足，残余能量比 {ratio:.4}");
    }

    #[test]
    fn totals_track_samples() {
        let mut resampler = StreamingResampler::new(16_000).unwrap();
        let _ = resampler.push(&vec![0.1; 160]).unwrap();
        assert_eq!(resampler.totals(), (160, 160));
    }
}
