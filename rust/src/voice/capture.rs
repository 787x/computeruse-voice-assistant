//! 麦克风采集：cpal（Windows 上走 WASAPI 共享模式）。
//!
//! 音频回调里**只做**三件事：格式转换 → 声道下混 → 推进有界队列。
//! 不做重采样、不做推理、不做磁盘 I/O、不长时间持锁——这些都在音频线程上完成。
//!
//! 背压策略：队列入队失败时**不阻塞回调**，而是累加溢出计数。
//! 阻塞回放会让 WASAPI 停止供数，整段音频都会失真；把已经失真的音频送去识别，
//! 比明确丢弃并重置这一句更糟。上层看到溢出后丢弃当前句、回到监听态。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::voice::pcm::{downmix_interleaved, int_to_float, TARGET_SAMPLE_RATE};

/// 有界队列容量（帧）：约 4 秒 16k 单声道。
///
/// 4 秒足够吸收一次 Whisper 推理期间采集的音频；再多就说明下游真的卡住了，
/// 这时候丢整句比无限堆积更安全（内存有界）。
pub const QUEUE_CAPACITY_FRAMES: usize = TARGET_SAMPLE_RATE as usize * 4;

/// 采集层错误（要能直接显示给用户）。
#[derive(Debug, Clone, thiserror::Error)]
pub enum CaptureError {
    #[error("没有可用的麦克风输入设备")]
    NoDevice,
    #[error("设备配置读取失败：{0}")]
    Config(String),
    #[error("无法打开麦克风“{device}”：{error}")]
    Open { device: String, error: String },
    #[error("麦克风采集失败：{0}")]
    Stream(String),
    #[error("设备已断开或不可用：{0}")]
    DeviceLost(String),
}

/// 采集线程看到的设备信息。
#[derive(Debug, Clone, PartialEq)]
pub struct CaptureInfo {
    pub device_name: String,
    pub sample_rate: u32,
    pub sample_format: String,
}

/// 一帧到音频线程的数据。
#[derive(Debug, Clone)]
pub struct CapturedChunk {
    /// 设备原始采样率下的**单声道**样本（尚未重采样）。
    pub samples: Vec<f32>,
}

/// 回调与音频线程之间的共享状态。
pub struct CaptureShared {
    /// 有界队列（标准库 `SyncSender`：满了返回错误而不是阻塞）。
    tx: std::sync::mpsc::SyncSender<CapturedChunk>,
    /// 溢出次数（队列满而丢弃的帧数）。
    overflow_frames: AtomicU64,
    /// 设备侧报错（回调线程写入，音频线程/界面读取）。
    error: std::sync::Mutex<Option<String>>,
    /// 要求停止（Drop 时置位；回调看到它就直接返回）。
    stop: AtomicBool,
}

impl CaptureShared {
    pub fn overflow_frames(&self) -> u64 {
        self.overflow_frames.load(Ordering::Relaxed)
    }

    pub fn take_overflow_frames(&self) -> u64 {
        self.overflow_frames.swap(0, Ordering::Relaxed)
    }

    pub fn take_error(&self) -> Option<String> {
        self.error.lock().ok().and_then(|mut guard| guard.take())
    }

    pub fn should_stop(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    fn report_error(&self, message: String) {
        if let Ok(mut guard) = self.error.lock() {
            // 只保留第一条：设备断开后回调会连续报错，后面的没有新信息。
            if guard.is_none() {
                *guard = Some(message);
            }
        }
    }
}

/// 一个正在运行的采集流。
///
/// Drop 时停止并释放设备——调用方不需要额外记得关麦克风。
pub struct Capture {
    /// `cpal::Stream` 不是 `Send`，只能在创建它的线程上持用。
    /// 所有操作都在工作线程完成，所以这里用 `Option` 并由 Drop 释放。
    stream: Option<cpal::Stream>,
    shared: Arc<CaptureShared>,
    receiver: Option<std::sync::mpsc::Receiver<CapturedChunk>>,
    info: CaptureInfo,
}

impl Capture {
    /// 列出可用的输入设备名（界面选择用）。
    pub fn input_devices() -> Vec<String> {
        let host = cpal::default_host();
        let Ok(devices) = host.input_devices() else {
            return Vec::new();
        };
        devices
            .filter_map(|device| device.description().ok().map(|d| d.name().to_string()))
            .collect()
    }

    /// 开始采集。`device_name` 为空表示系统默认输入设备。
    pub fn start(device_name: &str) -> Result<Self, CaptureError> {
        let host = cpal::default_host();
        let device = if device_name.trim().is_empty() {
            host.default_input_device().ok_or(CaptureError::NoDevice)?
        } else {
            let wanted = device_name.trim();
            let mut found = None;
            for candidate in host
                .input_devices()
                .map_err(|error| CaptureError::Config(format!("枚举输入设备失败：{error}")))?
            {
                let name = candidate
                    .description()
                    .map(|d| d.name().to_string())
                    .unwrap_or_default();
                if name == wanted {
                    found = Some(candidate);
                    break;
                }
            }
            found.ok_or_else(|| CaptureError::Config(format!("找不到麦克风“{wanted}”")))?
        };

        let device_label = device
            .description()
            .map(|d| d.name().to_string())
            .unwrap_or_else(|_| "未知设备".to_string());

        let supported = device
            .default_input_config()
            .map_err(|error| CaptureError::Config(format!("读取默认输入配置失败：{error}")))?;
        let sample_format = supported.sample_format();
        let config: cpal::StreamConfig = supported.into();
        let sample_rate = config.sample_rate;
        let channels = config.channels;

        if sample_rate == 0 || channels == 0 {
            return Err(CaptureError::Config(format!(
                "设备返回了无效配置：{sample_rate} Hz / {channels} 声道"
            )));
        }

        let (tx, rx) = std::sync::mpsc::sync_channel::<CapturedChunk>(QUEUE_CAPACITY_FRAMES / 160);
        let shared = Arc::new(CaptureShared {
            tx,
            overflow_frames: AtomicU64::new(0),
            error: std::sync::Mutex::new(None),
            stop: AtomicBool::new(false),
        });

        let callback_shared = shared.clone();
        let error_shared = shared.clone();
        let device_for_error = device_label.clone();

        // 用一个统一的闭包把「交错样本」转成单声道再入队。
        // 采样格式分发只发生在**建立流**时（按 T 泛型），回调里没有 match。
        let stream = build_stream(
            &device,
            &config,
            sample_format,
            callback_shared,
            error_shared,
            device_for_error,
        )?;

        stream
            .play()
            .map_err(|error| CaptureError::Stream(format!("启动采集失败：{error}")))?;

        Ok(Self {
            stream: Some(stream),
            shared,
            receiver: Some(rx),
            info: CaptureInfo {
                device_name: device_label,
                sample_rate,
                sample_format: format!("{sample_format:?}"),
            },
        })
    }

    pub fn info(&self) -> &CaptureInfo {
        &self.info
    }

    pub fn shared(&self) -> &Arc<CaptureShared> {
        &self.shared
    }

    pub fn receiver(&self) -> Option<&std::sync::mpsc::Receiver<CapturedChunk>> {
        self.receiver.as_ref()
    }

    /// 取走接收端（交给音频线程，采集结构只负责持有 stream）。
    pub fn take_receiver(&mut self) -> Option<std::sync::mpsc::Receiver<CapturedChunk>> {
        self.receiver.take()
    }

    /// 主动停止：释放设备并让音频线程的 `recv` 返回错误。
    pub fn stop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        self.stream.take();
        // 关闭发送端：音频线程看到断开就知道采集结束了。
        // SyncSender 无法主动 close，但 drop stream + receiver 结束即可。
        self.receiver.take();
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        self.stream.take();
    }
}

fn build_stream(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    format: cpal::SampleFormat,
    shared: Arc<CaptureShared>,
    error_shared: Arc<CaptureShared>,
    device_label: String,
) -> Result<cpal::Stream, CaptureError> {
    use cpal::SampleFormat as F;
    let error_callback = move |error: cpal::Error| {
        error_shared.report_error(format!("{error}"));
    };

    macro_rules! open {
        ($ty:ty, $convert:expr) => {{
            let convert = $convert;
            let callback_shared = shared.clone();
            let channels = config.channels as usize;
            device
                .build_input_stream::<$ty, _, _>(
                    config.clone(),
                    move |data: &[$ty], _info| {
                        if callback_shared.should_stop() {
                            return;
                        }
                        let mut mono: Vec<f32> = Vec::with_capacity(data.len() / channels.max(1));
                        for frame in data.chunks_exact(channels) {
                            let mut sum = 0.0f32;
                            for sample in frame {
                                sum += convert(*sample);
                            }
                            mono.push(sum / channels as f32);
                        }
                        if mono.is_empty() {
                            return;
                        }
                        if callback_shared
                            .tx
                            .try_send(CapturedChunk { samples: mono })
                            .is_err()
                        {
                            // 队列满：计数但**不阻塞回调**。
                            callback_shared
                                .overflow_frames
                                .fetch_add(1, Ordering::Relaxed);
                        }
                    },
                    error_callback.clone(),
                    None,
                )
                .map_err(|error| CaptureError::Open {
                    device: device_label.clone(),
                    error: error.to_string(),
                })
        }};
    }

    match format {
        F::I8 => open!(i8, |v: i8| int_to_float(v as i64, 8)),
        F::I16 => open!(i16, |v: i16| int_to_float(v as i64, 16)),
        F::I32 => open!(i32, |v: i32| int_to_float(v as i64, 32)),
        F::I64 => open!(i64, |v: i64| int_to_float(v, 64)),
        F::U8 => open!(u8, |v: u8| (v as f32 - 128.0) / 128.0),
        F::U16 => open!(u16, |v: u16| { (v as f32 - 32768.0) / 32768.0 }),
        F::U32 => open!(u32, |v: u32| {
            (v as f64 / u32::MAX as f64 * 2.0 - 1.0) as f32
        }),
        F::U64 => open!(u64, |v: u64| {
            (v as f64 / u64::MAX as f64 * 2.0 - 1.0) as f32
        }),
        F::F32 => open!(f32, |v: f32| v),
        F::F64 => open!(f64, |v: f64| v as f32),
        other => Err(CaptureError::Config(format!("不支持的采样格式：{other:?}"))),
    }
}

/// 把设备原始采样率下的单声道样本重采样成 16k。
///
/// 单独抽出来，方便音频线程按块调用，也方便测试不走设备。
pub struct FrontEnd {
    pub resampler: crate::voice::pcm::StreamingResampler,
    pub info: CaptureInfo,
    /// 上溢后需要丢弃当前句：由协调器读取并清空。
    pub dropped_due_to_overflow: bool,
}

impl FrontEnd {
    pub fn new(info: CaptureInfo) -> Result<Self, String> {
        Ok(Self {
            resampler: crate::voice::pcm::StreamingResampler::new(info.sample_rate)?,
            info,
            dropped_due_to_overflow: false,
        })
    }

    /// 处理一块设备音频，返回 16k 单声道样本。
    pub fn process(&mut self, chunk: &[f32]) -> Result<Vec<f32>, String> {
        self.resampler.push(chunk)
    }
}

/// 供测试：把「交错 + 整型」转换成单声道 f32，复用与真实回调**相同**的函数。
pub fn convert_interleaved_to_mono(samples: &[f32], channels: usize) -> Vec<f32> {
    downmix_interleaved(samples, channels)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_is_bounded_and_overflow_is_counted() {
        let (tx, _rx) = std::sync::mpsc::sync_channel::<CapturedChunk>(2);
        let shared = CaptureShared {
            tx,
            overflow_frames: AtomicU64::new(0),
            error: std::sync::Mutex::new(None),
            stop: AtomicBool::new(false),
        };
        // 前两次成功，之后溢出——而且**不阻塞**（否则这里会卡死）。
        assert!(shared
            .tx
            .try_send(CapturedChunk { samples: vec![0.0] })
            .is_ok());
        assert!(shared
            .tx
            .try_send(CapturedChunk { samples: vec![0.0] })
            .is_ok());
        if shared
            .tx
            .try_send(CapturedChunk { samples: vec![0.0] })
            .is_err()
        {
            shared.overflow_frames.fetch_add(1, Ordering::Relaxed);
        }
        assert_eq!(shared.take_overflow_frames(), 1);
        assert_eq!(shared.overflow_frames(), 0, "取走后计数清零");
    }

    #[test]
    fn first_error_is_kept_until_taken() {
        let (tx, _rx) = std::sync::mpsc::sync_channel::<CapturedChunk>(1);
        let shared = CaptureShared {
            tx,
            overflow_frames: AtomicU64::new(0),
            error: std::sync::Mutex::new(None),
            stop: AtomicBool::new(false),
        };
        shared.report_error("设备断开".to_string());
        shared.report_error("又一条错误".to_string());
        assert_eq!(shared.take_error().as_deref(), Some("设备断开"));
        assert!(shared.take_error().is_none());
    }

    #[test]
    fn frontend_reports_unsupported_rate() {
        let info = CaptureInfo {
            device_name: "test".to_string(),
            sample_rate: 0,
            sample_format: "F32".to_string(),
        };
        assert!(FrontEnd::new(info).is_err());
    }

    #[test]
    fn frontend_resamples_by_device_rate() {
        let info = CaptureInfo {
            device_name: "test".to_string(),
            sample_rate: 48_000,
            sample_format: "F32".to_string(),
        };
        let mut frontend = FrontEnd::new(info).unwrap();
        let mut out = Vec::new();
        for _ in 0..10 {
            out.extend(frontend.process(&vec![0.1; 480]).unwrap());
        }
        let seconds = out.len() as f32 / 16_000.0;
        assert!((seconds - 0.1).abs() < 0.03, "实际 {seconds:.3}s");
    }
}
