//! Seekable PCM WAV staging file. Decoder output starts at a virtual byte offset;
//! readers apply HTTP ranges in that same representation. A bounded read-ahead
//! window stalls the decoder without blocking Tokio's executor workers.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use librespot_playback::audio_backend::{Sink, SinkError, SinkResult};
use librespot_playback::convert::Converter;
use librespot_playback::decoder::AudioPacket;

const READ_WINDOW: u64 = 1 << 20;
pub const SAMPLE_RATE: u32 = 44_100;
pub const CHANNELS: u16 = 2;
pub const BITS: u16 = 16;
pub const BLOCK_ALIGN: u64 = CHANNELS as u64 * (BITS / 8) as u64;
pub const BYTE_RATE: u64 = SAMPLE_RATE as u64 * BLOCK_ALIGN;
pub const HEADER_LEN: u64 = 44;

/// Round to complete stereo frames, not a truncated integer bytes/ms rate.
pub fn pcm_bytes_at_ms(ms: u64) -> u64 {
    ms * SAMPLE_RATE as u64 / 1000 * BLOCK_ALIGN
}

pub fn position_ms_for_byte(start: u64) -> u32 {
    let frames = start.saturating_sub(HEADER_LEN) / BLOCK_ALIGN;
    (frames * 1000 / SAMPLE_RATE as u64).min(u32::MAX as u64) as u32
}

pub fn wav_header(data_len: u32) -> [u8; 44] {
    let mut h = [0u8; 44];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&(36 + data_len).to_le_bytes());
    h[8..12].copy_from_slice(b"WAVE");
    h[12..16].copy_from_slice(b"fmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes());
    h[20..22].copy_from_slice(&1u16.to_le_bytes());
    h[22..24].copy_from_slice(&CHANNELS.to_le_bytes());
    h[24..28].copy_from_slice(&SAMPLE_RATE.to_le_bytes());
    h[28..32].copy_from_slice(&(BYTE_RATE as u32).to_le_bytes());
    h[32..34].copy_from_slice(&(BLOCK_ALIGN as u16).to_le_bytes());
    h[34..36].copy_from_slice(&BITS.to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data_len.to_le_bytes());
    h
}

pub fn stream_total_bytes(duration_ms: i32) -> u64 {
    let max_data_len = (u32::MAX as u64 - 36) / BLOCK_ALIGN * BLOCK_ALIGN;
    HEADER_LEN + pcm_bytes_at_ms(duration_ms.max(0) as u64).min(max_data_len)
}

#[derive(Debug)]
pub enum StreamReadError {
    Failed,
}

impl std::fmt::Display for StreamReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "audio stream failed or cancelled")
    }
}
impl std::error::Error for StreamReadError {}

struct Progress {
    generated: u64,
    consumed: u64,
    done: bool,
    failed: bool,
    awaiting_position: bool,
    // One mutex owns both file and counters: no inverse state/writer lock order.
    writer: Option<File>,
}

pub struct TrackStream {
    pub(crate) path: PathBuf,
    total: u64,
    state: Mutex<Progress>,
    cond: Condvar,
}

impl TrackStream {
    pub fn create_at(
        dir: &std::path::Path,
        track_id: &str,
        duration_ms: i32,
        position_ms: u32,
    ) -> std::io::Result<Arc<Self>> {
        static NEXT_FILE: AtomicU64 = AtomicU64::new(0);
        let total = stream_total_bytes(duration_ms);
        let id = NEXT_FILE.fetch_add(1, Ordering::Relaxed);
        // Never truncate a file still referenced by an earlier same-track reader.
        let path = dir.join(format!(
            "cp_spotify_{track_id}_{}_{}.wav",
            std::process::id(),
            id
        ));
        let mut file = File::options().write(true).create_new(true).open(&path)?;
        file.write_all(&wav_header((total - HEADER_LEN) as u32))?;
        let generated = (HEADER_LEN + pcm_bytes_at_ms(position_ms as u64)).min(total);
        file.seek(SeekFrom::Start(generated))?;
        Ok(Arc::new(Self {
            path,
            total,
            state: Mutex::new(Progress {
                generated,
                consumed: generated,
                done: false,
                failed: false,
                awaiting_position: false,
                writer: Some(file),
            }),
            cond: Condvar::new(),
        }))
    }

    pub fn total(&self) -> u64 {
        self.total
    }

    pub fn await_decoder_position(&self) {
        self.state.lock().unwrap().awaiting_position = true;
    }

    /// Decoder seeks may land earlier than requested. Establish its actual
    /// virtual offset before permitting the first sample packet into the file.
    pub fn set_decoder_position(&self, position_ms: u32) {
        let mut st = self.state.lock().unwrap();
        let offset = (HEADER_LEN + pcm_bytes_at_ms(position_ms as u64)).min(self.total);
        if st.awaiting_position && !st.done {
            if let Some(file) = st.writer.as_mut() {
                if file.seek(SeekFrom::Start(offset)).is_err() {
                    st.failed = true;
                }
            }
            st.generated = offset;
            st.consumed = offset;
            st.awaiting_position = false;
            self.cond.notify_all();
        }
    }

    pub fn mark_failed(&self) {
        let mut st = self.state.lock().unwrap();
        st.failed = true;
        st.done = true;
        st.writer.take();
        self.cond.notify_all();
    }

    /// Replacement/disconnection must wake a blocked writer, not pad the old song.
    pub fn cancel(&self) {
        self.mark_failed();
    }

    /// Successful decoder EOF: sparse zero-extension preserves fixed HTTP length.
    pub fn finish(&self) {
        let mut st = self.state.lock().unwrap();
        if st.done {
            return;
        }
        if let Some(file) = st.writer.take() {
            if file.set_len(self.total).is_ok() {
                st.generated = self.total;
            } else {
                st.failed = true;
            }
        }
        st.done = true;
        self.cond.notify_all();
    }

    fn advance_consumed(&self, pos: u64) {
        let mut st = self.state.lock().unwrap();
        st.consumed = st.consumed.max(pos);
        self.cond.notify_all();
    }

    fn wait_for_end(&self, end: u64) -> Result<(), StreamReadError> {
        let mut st = self.state.lock().unwrap();
        loop {
            if st.failed {
                return Err(StreamReadError::Failed);
            }
            if !st.awaiting_position && st.generated >= end {
                return Ok(());
            }
            if st.done {
                return Err(StreamReadError::Failed);
            }
            st = self.cond.wait(st).unwrap();
        }
    }

    pub fn read_sequential(
        self: Arc<Self>,
        start: u64,
        end: u64,
    ) -> impl futures_util::Stream<Item = Result<bytes::Bytes, StreamReadError>> {
        let guard = ReaderGuard(self.clone());
        futures_util::stream::unfold(
            (self, start, end, guard),
            |(stream, pos, end, guard)| async move {
                if pos >= end {
                    return None;
                }
                let chunk_end = (pos + 256 * 1024).min(end);
                let reader = stream.clone();
                // Condvar + filesystem I/O belong on blocking workers, never Tokio's
                // two HTTP/event executor threads.
                let data = tokio::task::spawn_blocking(move || {
                    reader.wait_for_end(chunk_end)?;
                    let mut file = File::open(&reader.path).map_err(|_| StreamReadError::Failed)?;
                    file.seek(SeekFrom::Start(pos))
                        .map_err(|_| StreamReadError::Failed)?;
                    let mut buf = vec![0; (chunk_end - pos) as usize];
                    file.read_exact(&mut buf)
                        .map_err(|_| StreamReadError::Failed)?;
                    reader.advance_consumed(chunk_end);
                    Ok(bytes::Bytes::from(buf))
                })
                .await
                .unwrap_or(Err(StreamReadError::Failed));
                let next = if data.is_ok() { chunk_end } else { end };
                Some((data, (stream, next, end, guard)))
            },
        )
    }

    fn write_pcm(&self, bytes: &[u8]) -> SinkResult<()> {
        let mut st = self.state.lock().unwrap();
        while st.awaiting_position && !st.done {
            st = self.cond.wait(st).unwrap();
        }
        let len = bytes
            .len()
            .min(self.total.saturating_sub(st.generated) as usize);
        while !st.done
            && st
                .generated
                .saturating_add(len as u64)
                .saturating_sub(st.consumed)
                > READ_WINDOW
        {
            st = self.cond.wait(st).unwrap();
        }
        if st.done || len == 0 {
            return Ok(());
        }
        if let Some(file) = st.writer.as_mut() {
            if let Err(e) = file.write_all(&bytes[..len]) {
                st.failed = true;
                st.done = true;
                st.writer.take();
                self.cond.notify_all();
                return Err(SinkError::OnWrite(e.to_string()));
            }
            st.generated += len as u64;
            self.cond.notify_all();
        }
        Ok(())
    }
}

struct ReaderGuard(Arc<TrackStream>);
impl Drop for ReaderGuard {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

impl Drop for TrackStream {
    fn drop(&mut self) {
        // path is the unique file owned by this stream, never a shared track path.
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Bound permanently to one decoder generation, never consults a mutable slot.
pub struct StreamSink {
    stream: Arc<TrackStream>,
}
impl StreamSink {
    pub fn new(stream: Arc<TrackStream>) -> Self {
        Self { stream }
    }
}
impl Sink for StreamSink {
    fn write(&mut self, packet: AudioPacket, converter: &mut Converter) -> SinkResult<()> {
        let samples = packet
            .samples()
            .map_err(|_| SinkError::OnWrite("raw packet in sink".into()))?;
        let s16 = converter.f64_to_s16(samples);
        let mut bytes = Vec::with_capacity(s16.len() * 2);
        for sample in s16 {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        self.stream.write_pcm(&bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;

    #[test]
    fn wav_header_and_frame_math() {
        let h = wav_header(176_400);
        assert_eq!(&h[..4], b"RIFF");
        assert_eq!(&h[8..12], b"WAVE");
        assert_eq!(u32::from_le_bytes(h[28..32].try_into().unwrap()), 176_400);
        assert_eq!(u16::from_le_bytes(h[32..34].try_into().unwrap()), 4);
        assert_eq!(u16::from_le_bytes(h[34..36].try_into().unwrap()), 16);
        assert_eq!(stream_total_bytes(1000), 176_444);
        assert_eq!(stream_total_bytes(180_000), 31_752_044);
        assert_eq!(stream_total_bytes(-1), 44);
        let max_total = stream_total_bytes(i32::MAX);
        let h = wav_header((max_total - HEADER_LEN) as u32);
        assert_eq!(
            u32::from_le_bytes(h[4..8].try_into().unwrap()) as u64 + 8,
            max_total
        );
        assert!(u32::from_le_bytes(h[4..8].try_into().unwrap()) as u64 <= u32::MAX as u64);
        for ms in [1, 3, 999, 1000, 179_999] {
            assert_eq!(pcm_bytes_at_ms(ms) % 4, 0);
        }
        assert_eq!(position_ms_for_byte(44 + 176_400), 1000);
    }

    async fn collect(stream: Arc<TrackStream>, start: u64, end: u64) -> Vec<u8> {
        let body = stream.read_sequential(start, end);
        futures_util::pin_mut!(body);
        let mut result = Vec::new();
        while let Some(chunk) = body.next().await {
            result.extend_from_slice(&chunk.unwrap());
        }
        result
    }

    #[tokio::test]
    async fn full_body_includes_header_and_clips_decoder_tail() {
        let dir = tempfile::tempdir().unwrap();
        let s = TrackStream::create_at(dir.path(), "track", 1, 0).unwrap();
        s.write_pcm(&vec![7; 512]).unwrap();
        s.finish();
        let body = collect(s.clone(), 0, s.total()).await;
        assert_eq!(body.len() as u64, stream_total_bytes(1));
        assert_eq!(&body[..44], &wav_header(176));
        assert!(body[44..].iter().all(|b| *b == 7));
    }

    #[tokio::test]
    async fn seek_bytes_have_virtual_offsets_and_unique_files() {
        let dir = tempfile::tempdir().unwrap();
        let old = TrackStream::create_at(dir.path(), "track", 2000, 0).unwrap();
        let s = TrackStream::create_at(dir.path(), "track", 2000, 1000).unwrap();
        assert_ne!(s.path, old.path);
        s.write_pcm(&[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
        s.finish();
        assert_eq!(
            collect(s, 44 + 176_400 + 1, 44 + 176_400 + 7).await,
            [2, 3, 4, 5, 6, 7]
        );
        assert_eq!(collect(old, 0, 44).await, wav_header(352_800));
    }

    #[tokio::test]
    async fn dropping_reader_cancels_stream_and_unblocks_writer() {
        let dir = tempfile::tempdir().unwrap();
        let s = TrackStream::create_at(dir.path(), "track", 120_000, 0).unwrap();
        s.write_pcm(&vec![1; READ_WINDOW as usize]).unwrap();

        let body = s.clone().read_sequential(0, 44 + READ_WINDOW);
        let writer = s.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let task = std::thread::spawn(move || {
            writer.write_pcm(&[1; 4]).unwrap();
            tx.send(()).unwrap();
        });
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(30))
                .is_err()
        );
        drop(body);
        rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        task.join().unwrap();
        assert_eq!(std::fs::metadata(&s.path).unwrap().len(), 44 + READ_WINDOW);
    }

    #[test]
    fn seek_position_resets_virtual_offset_before_writes() {
        let dir = tempfile::tempdir().unwrap();
        let s = TrackStream::create_at(dir.path(), "track", 2000, 1000).unwrap();
        s.await_decoder_position();
        let writer = s.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let task = std::thread::spawn(move || {
            writer.write_pcm(&[9, 8, 7, 6]).unwrap();
            tx.send(()).unwrap();
        });
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(30))
                .is_err()
        );
        s.set_decoder_position(900);
        rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        task.join().unwrap();
        let start = HEADER_LEN + pcm_bytes_at_ms(900);
        let file = std::fs::read(&s.path).unwrap();
        assert_eq!(&file[start as usize..start as usize + 4], &[9, 8, 7, 6]);
        assert_eq!(s.state.lock().unwrap().generated, start + 4);
    }

    #[test]
    fn cancellation_unblocks_backpressure_without_padding() {
        let dir = tempfile::tempdir().unwrap();
        let s = TrackStream::create_at(dir.path(), "track", 120_000, 0).unwrap();
        s.write_pcm(&vec![1; READ_WINDOW as usize]).unwrap();
        let writer = s.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let task = std::thread::spawn(move || {
            writer.write_pcm(&[1; 4]).unwrap();
            tx.send(()).unwrap();
        });
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(30))
                .is_err()
        );
        s.cancel();
        rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        task.join().unwrap();
        assert_eq!(std::fs::metadata(&s.path).unwrap().len(), 44 + READ_WINDOW);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn waiting_reader_does_not_block_executor() {
        let dir = tempfile::tempdir().unwrap();
        let s = TrackStream::create_at(dir.path(), "track", 1000, 0).unwrap();
        let reader = s.clone();
        let task = tokio::spawn(async move { collect(reader, 0, 48).await });
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        s.write_pcm(&[1, 2, 3, 4]).unwrap();
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(2), task)
                .await
                .unwrap()
                .unwrap()
                .len(),
            48
        );
    }
}
