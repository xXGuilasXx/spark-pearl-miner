//! spm-ipc — frames between the daemon and the GPU worker over `worker.sock`.
//!
//! Frame layout (little-endian): `len: u32 | version: u16 | payload: bincode`, where `len` counts
//! the payload only. Payloads use bincode with fixed-width integers, a size limit equal to
//! [`MAX_FRAME_BYTES`] and trailing bytes rejected. A frame with another version is refused
//! before its payload is parsed, so a daemon and a worker from different builds fail cleanly.
#![forbid(unsafe_code)]

use std::io::{Read, Write};

use bincode::Options;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub use spm_work::WorkUnit;

/// Bumped on any change to the message types below.
pub const IPC_VERSION: u16 = 1;
/// Largest payload accepted (a proof is 100–370 KB of bincode).
pub const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;
pub const HEADER_BYTES: usize = 6;

/// Daemon → worker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ToWorker {
    Hello { version: u16 },
    /// Boxed only to keep the enum small; the wire encoding is the same as an inline WorkUnit.
    SetJob { wu: Box<WorkUnit> },
    Pause,
    Resume,
    /// Duty cycle for the power governor, 1–100 %.
    SetDuty { pct: u8 },
    /// Release the GPU: the worker exits and frees its CUDA context.
    Release,
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FaultKind {
    /// The known-answer test failed at start or on re-run.
    KatFailed,
    /// A proof failed local verification (compute fault; nothing was submitted).
    VerifyFailed,
    /// The per-attempt CPU canary tile disagreed with the GPU.
    CanaryMismatch,
    Cuda,
    OutOfMemory,
    Protocol,
    Other,
}

/// Worker → daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ToDaemon {
    Ready {
        kat_ok: bool,
        device: String,
    },
    /// Unix time in milliseconds.
    Heartbeat {
        ts: u64,
    },
    /// Counters since the previous `Stats` frame.
    Stats {
        credited_macs: u64,
        tiles: u64,
        attempts: u64,
        sm_clock_mhz: u32,
        power_w: f32,
    },
    /// A locally verified hit for work unit `wu_id`.
    Proof {
        wu_id: u64,
        session_id: u64,
        job_id: String,
        is_block: bool,
        /// Jackpot hash (compared little-endian against the bounds).
        digest: [u8; 32],
        t_rows: u32,
        t_cols: u32,
        /// bincode(PlainProof), ready for the proof encoders.
        proof_bincode: Vec<u8>,
    },
    Fault {
        kind: FaultKind,
        msg: String,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error("frame of {0} bytes exceeds the {MAX_FRAME_BYTES}-byte limit")]
    TooLarge(usize),
    #[error("peer speaks IPC version {got}, this build speaks {IPC_VERSION}")]
    Version { got: u16 },
    #[error("bincode: {0}")]
    Bincode(#[from] bincode::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("connection closed")]
    Closed,
}

fn opts() -> impl Options {
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
        .with_limit(MAX_FRAME_BYTES as u64)
        .reject_trailing_bytes()
}

/// Serialize one message into a complete frame.
pub fn encode_frame<T: Serialize>(msg: &T) -> Result<Vec<u8>, IpcError> {
    let payload = opts().serialize(msg)?;
    if payload.len() > MAX_FRAME_BYTES {
        return Err(IpcError::TooLarge(payload.len()));
    }
    let len = u32::try_from(payload.len()).map_err(|_| IpcError::TooLarge(payload.len()))?;
    let mut out = Vec::with_capacity(HEADER_BYTES + payload.len());
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&IPC_VERSION.to_le_bytes());
    out.extend_from_slice(&payload);
    Ok(out)
}

/// Parse a frame header: payload length, after checking the size limit and version.
pub fn parse_header(h: &[u8; HEADER_BYTES]) -> Result<usize, IpcError> {
    let len = u32::from_le_bytes([h[0], h[1], h[2], h[3]]) as usize;
    let version = u16::from_le_bytes([h[4], h[5]]);
    if len > MAX_FRAME_BYTES {
        return Err(IpcError::TooLarge(len));
    }
    if version != IPC_VERSION {
        return Err(IpcError::Version { got: version });
    }
    Ok(len)
}

pub fn decode_payload<T: DeserializeOwned>(payload: &[u8]) -> Result<T, IpcError> {
    Ok(opts().deserialize(payload)?)
}

/// Incremental decoder for a byte stream (non-blocking sockets, tests).
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
}

impl FrameDecoder {
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Next complete message, if one is buffered. Errors are fatal for the connection.
    pub fn next_frame<T: DeserializeOwned>(&mut self) -> Result<Option<T>, IpcError> {
        let Some(h) = self.buf.first_chunk::<HEADER_BYTES>() else {
            return Ok(None);
        };
        let len = parse_header(h)?;
        if self.buf.len() < HEADER_BYTES + len {
            return Ok(None);
        }
        let msg = decode_payload(&self.buf[HEADER_BYTES..HEADER_BYTES + len])?;
        self.buf.drain(..HEADER_BYTES + len);
        Ok(Some(msg))
    }
}

/// Blocking write of one frame (worker side).
pub fn write_frame<W: Write, T: Serialize>(w: &mut W, msg: &T) -> Result<(), IpcError> {
    w.write_all(&encode_frame(msg)?)?;
    w.flush()?;
    Ok(())
}

/// Blocking read of one frame; `Closed` on a clean EOF at a frame boundary.
pub fn read_frame<R: Read, T: DeserializeOwned>(r: &mut R) -> Result<T, IpcError> {
    let mut h = [0u8; HEADER_BYTES];
    match r.read_exact(&mut h) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Err(IpcError::Closed),
        Err(e) => return Err(e.into()),
    }
    let len = parse_header(&h)?;
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    decode_payload(&payload)
}

/// Async write of one frame (daemon side).
pub async fn write_frame_async<W: AsyncWrite + Unpin, T: Serialize>(w: &mut W, msg: &T) -> Result<(), IpcError> {
    w.write_all(&encode_frame(msg)?).await?;
    w.flush().await?;
    Ok(())
}

/// Async read of one frame; `Closed` on a clean EOF at a frame boundary.
pub async fn read_frame_async<R: AsyncRead + Unpin, T: DeserializeOwned>(r: &mut R) -> Result<T, IpcError> {
    let mut h = [0u8; HEADER_BYTES];
    match r.read_exact(&mut h).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Err(IpcError::Closed),
        Err(e) => return Err(e.into()),
    }
    let len = parse_header(&h)?;
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload).await?;
    decode_payload(&payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use primitive_types::U256;
    use spm_proto::Job;
    use spm_work::Shape;

    fn wu() -> WorkUnit {
        let mut header = [0u8; 76];
        header[0..4].copy_from_slice(&0x2000_0000u32.to_le_bytes());
        header[72..76].copy_from_slice(&0x1d7f_ffffu32.to_le_bytes());
        let job = Job {
            job_id: "0000002a_1".into(),
            header,
            target: U256::from(0x7f_ffffu64) << 208,
            height: Some(119_365),
            diff: Some(1),
            cert_version: Some(3),
        };
        WorkUnit::build(&job, Shape { m: 256, n: 256, k: 2048, r: 128 }, 2, 42).unwrap()
    }

    fn all_to_worker() -> Vec<ToWorker> {
        vec![
            ToWorker::Hello { version: IPC_VERSION },
            ToWorker::SetJob { wu: Box::new(wu()) },
            ToWorker::Pause,
            ToWorker::Resume,
            ToWorker::SetDuty { pct: 75 },
            ToWorker::Release,
            ToWorker::Shutdown,
        ]
    }

    fn all_to_daemon() -> Vec<ToDaemon> {
        vec![
            ToDaemon::Ready { kat_ok: true, device: "NVIDIA GB10 (sm_121)".into() },
            ToDaemon::Heartbeat { ts: 1_790_445_863_721 },
            ToDaemon::Stats { credited_macs: 70_368_744_177_664, tiles: 1 << 30, attempts: 3, sm_clock_mhz: 2200, power_w: 74.5 },
            ToDaemon::Proof {
                wu_id: 42,
                session_id: 2,
                job_id: "0000002a_1".into(),
                is_block: false,
                digest: [0xab; 32],
                t_rows: 64 * 3 + 5,
                t_cols: 64 * 7 + 6,
                proof_bincode: (0..300_000u32).map(|i| (i % 251) as u8).collect(),
            },
            ToDaemon::Fault { kind: FaultKind::CanaryMismatch, msg: "tile (3,7) differs".into() },
        ]
    }

    #[test]
    fn every_message_round_trips_through_frames() {
        for m in all_to_worker() {
            let f = encode_frame(&m).unwrap();
            assert_eq!(u16::from_le_bytes([f[4], f[5]]), IPC_VERSION);
            assert_eq!(u32::from_le_bytes([f[0], f[1], f[2], f[3]]) as usize, f.len() - HEADER_BYTES);
            assert_eq!(read_frame::<_, ToWorker>(&mut &f[..]).unwrap(), m);
        }
        for m in all_to_daemon() {
            let f = encode_frame(&m).unwrap();
            assert_eq!(read_frame::<_, ToDaemon>(&mut &f[..]).unwrap(), m);
        }
    }

    #[test]
    fn stream_decoder_handles_split_and_coalesced_frames() {
        let msgs = all_to_daemon();
        let mut stream = Vec::new();
        for m in &msgs {
            write_frame(&mut stream, m).unwrap();
        }
        // Feed in odd-sized pieces.
        let mut d = FrameDecoder::default();
        let mut got = Vec::new();
        for piece in stream.chunks(997) {
            d.push(piece);
            while let Some(m) = d.next_frame::<ToDaemon>().unwrap() {
                got.push(m);
            }
        }
        assert_eq!(got, msgs);
        // Blocking reader over the same stream, then a clean EOF.
        let mut r = &stream[..];
        for m in &msgs {
            assert_eq!(&read_frame::<_, ToDaemon>(&mut r).unwrap(), m);
        }
        assert!(matches!(read_frame::<_, ToDaemon>(&mut r), Err(IpcError::Closed)));
    }

    #[test]
    fn oversized_frames_are_refused_both_ways() {
        let big = ToDaemon::Proof {
            wu_id: 1,
            session_id: 1,
            job_id: "j".into(),
            is_block: false,
            digest: [0; 32],
            t_rows: 0,
            t_cols: 0,
            proof_bincode: vec![0; MAX_FRAME_BYTES],
        };
        assert!(encode_frame(&big).is_err());
        let mut h = [0u8; HEADER_BYTES];
        h[0..4].copy_from_slice(&((MAX_FRAME_BYTES as u32) + 1).to_le_bytes());
        h[4..6].copy_from_slice(&IPC_VERSION.to_le_bytes());
        assert!(matches!(parse_header(&h), Err(IpcError::TooLarge(_))));
        let mut d = FrameDecoder::default();
        d.push(&h);
        assert!(matches!(d.next_frame::<ToDaemon>(), Err(IpcError::TooLarge(_))));
    }

    #[test]
    fn version_mismatch_and_garbage_are_errors() {
        let mut f = encode_frame(&ToWorker::Pause).unwrap();
        f[4] = f[4].wrapping_add(1);
        assert!(matches!(read_frame::<_, ToWorker>(&mut &f[..]), Err(IpcError::Version { .. })));
        // Unknown enum tag.
        let mut f = encode_frame(&ToWorker::Pause).unwrap();
        f[HEADER_BYTES] = 200;
        assert!(matches!(read_frame::<_, ToWorker>(&mut &f[..]), Err(IpcError::Bincode(_))));
        // Trailing bytes inside a payload.
        let mut f = encode_frame(&ToWorker::Pause).unwrap();
        f.push(0);
        let len = (f.len() - HEADER_BYTES) as u32;
        f[0..4].copy_from_slice(&len.to_le_bytes());
        assert!(matches!(read_frame::<_, ToWorker>(&mut &f[..]), Err(IpcError::Bincode(_))));
        // A length prefix that promises more than bincode may allocate for a Vec.
        let mut f = encode_frame(&ToDaemon::Fault { kind: FaultKind::Other, msg: "x".into() }).unwrap();
        let tag_and_kind = 4 + 4;
        f[HEADER_BYTES + tag_and_kind..HEADER_BYTES + tag_and_kind + 8].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(read_frame::<_, ToDaemon>(&mut &f[..]).is_err());
    }

    #[tokio::test]
    async fn async_frames_over_a_duplex_pipe() {
        let (mut a, mut b) = tokio::io::duplex(64 * 1024);
        let send = tokio::spawn(async move {
            for m in all_to_daemon() {
                write_frame_async(&mut a, &m).await.unwrap();
            }
        });
        for m in all_to_daemon() {
            assert_eq!(read_frame_async::<_, ToDaemon>(&mut b).await.unwrap(), m);
        }
        send.await.unwrap();
        assert!(matches!(read_frame_async::<_, ToDaemon>(&mut b).await, Err(IpcError::Closed)));
    }
}
