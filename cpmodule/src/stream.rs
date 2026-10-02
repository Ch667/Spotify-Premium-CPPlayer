//! WAV 流：librespot `Sink` 写入 → 磁盘临时文件 → axum 按序读取（HTTP Range 友好）。
//!
//! 设计要点：
//! - **背压**：`Sink::write` 在「已生成 - 已消费 > `READ_WINDOW`」时阻塞，
//!   宿主暂停拉流 → 生成自动停滞（librespot 内部管线随之缓冲），恢复拉流即续播，
//!   保证暂停/恢复后位置一致。
//! - **定长**：WAV 头携带按曲目时长计算的精确总长；提前结束时补静音到定长，
//!   使 HTTP `Content-Length` 恒定，宿主播放器可发起 Range seek。
//! - **seek 策略**：每次带 Range 的 /stream 请求都从该位置重启播放
//!   （见 `engine`），旧流被替换并标记 done，读端读到提前 EOF 后正常收尾。

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use librespot_playback::audio_backend::{Sink, SinkError, SinkResult};
use librespot_playback::convert::Converter;
use librespot_playback::decoder::AudioPacket;

/// 生成端允许领先读端的最大字节数（约 6 秒 44.1k/16bit/双声道）。
const READ_WINDOW: u64 = 1 << 20;

pub const SAMPLE_RATE: u32 = 44_100;
pub const CHANNELS: u16 = 2;
pub const BITS: u16 = 16;
/// 44100 × 2ch × 16bit
pub const BYTES_PER_MS: u64 = 176;

/// 44 字节标准 PCM WAV 头。
pub fn wav_header(data_len: u32) -> [u8; 44] {
    let mut h = [0u8; 44];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&(36 + data_len).to_le_bytes());
    h[8..12].copy_from_slice(b"WAVE");
    h[12..16].copy_from_slice(b"fmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes());
    h[20..22].copy_from_slice(&1u16.to_le_bytes()); // PCM
    h[22..24].copy_from_slice(&CHANNELS.to_le_bytes());
    h[24..28].copy_from_slice(&SAMPLE_RATE.to_le_bytes());
    let byte_rate = SAMPLE_RATE * CHANNELS as u32 * (BITS / 8) as u32;
    h[28..32].copy_from_slice(&byte_rate.to_le_bytes());
    let block_align = CHANNELS * (BITS / 8);
    h[32..34].copy_from_slice(&block_align.to_le_bytes());
    h[34..36].copy_from_slice(&BITS.to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data_len.to_le_bytes());
    h
}

/// 曲目时长（毫秒）→ 流总字节数（含 WAV 头）。
pub fn stream_total_bytes(duration_ms: i32) -> u64 {
    44 + duration_ms.max(0) as u64 * BYTES_PER_MS
}

#[derive(Debug)]
pub enum StreamReadError {
    Failed,
    OutOfRange { total: u64 },
}

impl std::fmt::Display for StreamReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StreamReadError::Failed => write!(f, "audio stream failed"),
            StreamReadError::OutOfRange { total } => {
                write!(f, "range start beyond stream end ({total})")
            }
        }
    }
}

impl std::error::Error for StreamReadError {}

struct Progress {
    /// 已写入文件的总字节数（含 44 字节 WAV 头）。
    generated: u64,
    /// 读端已消费到的最大偏移（背压依据）。
    consumed: u64,
    done: bool,
}

/// 一条曲目的音频流状态。写端：`StreamSink`（librespot 播放线程）；
/// 读端：axum 的 /stream 处理器。
pub struct TrackStream {
    pub(crate) path: PathBuf,
    total: u64,
    state: Mutex<Progress>,
    cond: Condvar,
    failed: AtomicBool,
    writer: Mutex<Option<File>>,
}

impl TrackStream {
    /// 创建新流并写入 WAV 头。`duration_ms` 来自曲目元数据。
    pub fn create(dir: &std::path::Path, track_id: &str, duration_ms: i32) -> std::io::Result<Arc<Self>> {
        let total = stream_total_bytes(duration_ms);
        let path = dir.join(format!("cp_spotify_{track_id}.wav"));
        let mut file = File::create(&path)?;
        file.write_all(&wav_header((total - 44) as u32))?;
        Ok(Arc::new(Self {
            path,
            total,
            state: Mutex::new(Progress { generated: 44, consumed: 44, done: false }),
            cond: Condvar::new(),
            failed: AtomicBool::new(false),
            writer: Mutex::new(Some(file)),
        }))
    }

    pub fn total(&self) -> u64 {
        self.total
    }

    pub fn mark_failed(&self) {
        self.failed.store(true, Ordering::SeqCst);
        self.finish();
    }

    /// 生成端结束：补静音到定长并释放写端。
    pub fn finish(&self) {
        let mut st = self.state.lock().unwrap();
        if st.done {
            return;
        }
        if let Some(file) = self.writer.lock().unwrap().take() {
            let mut file = file;
            if st.generated < self.total {
                let pad = self.total - st.generated;
                let zeros = vec![0u8; 64 * 1024];
                let mut left = pad;
                while left > 0 {
                    let n = left.min(zeros.len() as u64) as usize;
                    if file.write_all(&zeros[..n]).is_err() {
                        break;
                    }
                    st.generated += n as u64;
                    left -= n as u64;
                }
            }
        }
        st.done = true;
        self.cond.notify_all();
    }

    /// 读端消费推进（背压解除）。
    fn advance_consumed(&self, pos: u64) {
        let mut st = self.state.lock().unwrap();
        if pos > st.consumed {
            st.consumed = pos;
            self.cond.notify_all();
        }
    }

    fn wait_for_end(&self, end: u64) -> Result<u64, StreamReadError> {
        let mut st = self.state.lock().unwrap();
        loop {
            if st.generated >= end {
                return Ok(end);
            }
            if self.failed.load(Ordering::SeqCst) {
                return Err(StreamReadError::Failed);
            }
            if st.done {
                // 已定长补齐，generated == total
                return Ok(st.generated.min(end).max(st.consumed));
            }
            st = self.cond.wait(st).unwrap();
        }
    }

    /// 顺序读取 `[start, end)`，按需等待生成端推进。
    pub fn read_sequential(
        self: Arc<Self>,
        start: u64,
        end: u64,
    ) -> impl futures_util::Stream<Item = Result<bytes::Bytes, StreamReadError>> {
        futures_util::stream::unfold(
            (self, start, end),
            |(stream, pos, end)| async move {
                if pos >= end {
                    return None;
                }
                let chunk_end = (pos + 256 * 1024).min(end);
                match stream.wait_for_end(chunk_end) {
                    Ok(actual_end) => {
                        if actual_end <= pos {
                            return None; // 提前 EOF（补齐后仍不足，防御）
                        }
                        stream.advance_consumed(actual_end);
                        let data = read_file_range(&stream.path, pos, actual_end).await;
                        Some((data, (stream, actual_end, end)))
                    }
                    Err(e) => Some((Err(e), (stream, end, end))),
                }
            },
        )
    }
}

async fn read_file_range(path: &std::path::Path, start: u64, end: u64) -> Result<bytes::Bytes, StreamReadError> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut file = File::open(&path).map_err(|_| StreamReadError::Failed)?;
        file.seek(SeekFrom::Start(start)).map_err(|_| StreamReadError::Failed)?;
        let mut buf = vec![0u8; (end - start) as usize];
        file.read_exact(&mut buf).map_err(|_| StreamReadError::Failed)?;
        Ok(bytes::Bytes::from(buf))
    })
    .await
    .map_err(|_| StreamReadError::Failed)?
}

/// librespot 播放线程 → 文件。每次 `Player::new` 创建一个实例，
/// 写入目标由共享槽位（当前 TrackStream）在写时决定。
pub struct StreamSink {
    cell: Arc<tokio::sync::Mutex<Option<Arc<TrackStream>>>>,
}

impl StreamSink {
    /// `cell` 与 engine 的当前流槽位是同一个。
    pub fn new(cell: Arc<tokio::sync::Mutex<Option<Arc<TrackStream>>>>) -> Self {
        Self { cell }
    }
}

impl Sink for StreamSink {
    fn write(&mut self, packet: AudioPacket, converter: &mut Converter) -> SinkResult<()> {
        let samples = packet
            .samples()
            .map_err(|_| SinkError::OnWrite("raw packet in sink".into()))?;
        let s16: Vec<i16> = converter.f64_to_s16(samples);

        // 取当前流；槽位为空（未 prepare / 已被替换）时丢弃本包。
        let stream = match self.cell.try_lock() {
            Ok(guard) => guard.clone(),
            Err(_) => return Ok(()),
        };
        let Some(stream) = stream else {
            return Ok(());
        };

        let mut bytes = Vec::with_capacity(s16.len() * 2);
        for s in s16 {
            bytes.extend_from_slice(&s.to_le_bytes());
        }

        // 背压：领先读端超过窗口则等待。锁顺序：state 锁短促持有。
        {
            let mut st = stream.state.lock().unwrap();
            loop {
                let ahead = st.generated + bytes.len() as u64 - st.consumed;
                if ahead <= READ_WINDOW || st.done || stream.failed.load(Ordering::SeqCst) {
                    break;
                }
                st = stream.cond.wait(st).unwrap();
            }
        }

        if let Some(file) = stream.writer.lock().unwrap().as_mut() {
            if file.write_all(&bytes).is_ok() {
                let mut st = stream.state.lock().unwrap();
                st.generated += bytes.len() as u64;
                drop(st);
                stream.cond.notify_all();
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_header_layout() {
        let h = wav_header(176_400); // 恰好 1 秒
        assert_eq!(&h[0..4], b"RIFF");
        assert_eq!(&h[8..12], b"WAVE");
        assert_eq!(&h[12..16], b"fmt ");
        assert_eq!(u32::from_le_bytes(h[16..20].try_into().unwrap()), 16);
        assert_eq!(u16::from_le_bytes(h[20..22].try_into().unwrap()), 1);
        assert_eq!(u16::from_le_bytes(h[22..24].try_into().unwrap()), CHANNELS);
        assert_eq!(u32::from_le_bytes(h[24..28].try_into().unwrap()), SAMPLE_RATE);
        assert_eq!(u32::from_le_bytes(h[28..32].try_into().unwrap()), 176_400);
        assert_eq!(u16::from_le_bytes(h[32..34].try_into().unwrap()), 4);
        assert_eq!(u16::from_le_bytes(h[34..36].try_into().unwrap()), 16);
        assert_eq!(&h[36..40], b"data");
        assert_eq!(u32::from_le_bytes(h[40..44].try_into().unwrap()), 176_400);
        // RIFF 尺寸 = 36 + data_len
        assert_eq!(u32::from_le_bytes(h[4..8].try_into().unwrap()), 36 + 176_400);
    }

    #[test]
    fn total_bytes_math() {
        assert_eq!(stream_total_bytes(0), 44);
        assert_eq!(stream_total_bytes(1000), 44 + 1000 * BYTES_PER_MS);
        assert_eq!(stream_total_bytes(-5), 44);
    }
}
