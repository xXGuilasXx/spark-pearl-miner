//! A fake daemon for the worker tests: binds `worker.sock` in a private directory, says Hello,
//! and collects every frame the worker sends.
#![allow(dead_code)]

use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use spm_coexist::handshake::{read_ack_file, Ack, ACK_FILE};
use spm_cpuref::U256;
use spm_ipc::{read_frame, write_frame, ToDaemon, ToWorker, WorkUnit, IPC_VERSION};
use spm_proto::Job;
use spm_work::Shape;
use spm_worker::{Engine, EngineError, Options, Outcome};

static NEXT: AtomicU64 = AtomicU64::new(0);

pub struct FakeDaemon {
    pub dir: PathBuf,
    pub sock: PathBuf,
    listener: UnixListener,
}

impl FakeDaemon {
    pub fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "spmw-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("worker.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        Self {
            dir,
            sock,
            listener,
        }
    }

    /// Worker options for in-process tests: no signal handlers, no memory guard, no NVML, fast
    /// heartbeat and stats, deterministic nonces.
    pub fn options(&self) -> Options {
        let mut o = Options::new(self.sock.clone());
        o.connect_timeout = Duration::from_secs(5);
        o.heartbeat = Duration::from_millis(100);
        o.stats_every = Duration::from_millis(200);
        o.signals = false;
        o.memory_guard = false;
        o.telemetry = false;
        o.nonce_start = Some(1);
        o
    }

    /// Starts the worker in a thread with `engine` and accepts its connection.
    pub fn spawn<E, F>(&self, opts: Options, make: F) -> (Conn, WorkerThread)
    where
        E: Engine + 'static,
        F: FnOnce() -> Result<E, EngineError> + Send + 'static,
    {
        let (tx, rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            let out = spm_worker::run_with(opts, make);
            let _ = tx.send(());
            out
        });
        let conn = self.accept();
        (
            conn,
            WorkerThread {
                handle: Some(handle),
                done: rx,
            },
        )
    }
}

impl FakeDaemon {
    /// Accepts one worker connection and says Hello.
    pub fn accept(&self) -> Conn {
        let (stream, _) = self.listener.accept().unwrap();
        Conn::new(stream, self.dir.join(ACK_FILE))
    }
}

impl Drop for FakeDaemon {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

pub struct WorkerThread {
    handle: Option<JoinHandle<anyhow::Result<Outcome>>>,
    done: mpsc::Receiver<()>,
}

impl WorkerThread {
    /// Waits for the worker to return (panics after `timeout`).
    pub fn join(mut self, timeout: Duration) -> Outcome {
        self.done
            .recv_timeout(timeout)
            .expect("the worker did not exit in time");
        self.handle.take().unwrap().join().unwrap().unwrap()
    }
}

pub struct Conn {
    w: UnixStream,
    rx: mpsc::Receiver<ToDaemon>,
    pub seen: Vec<ToDaemon>,
    ack_path: PathBuf,
}

impl Conn {
    fn new(stream: UnixStream, ack_path: PathBuf) -> Self {
        let mut r = stream.try_clone().unwrap();
        let mut w = stream;
        write_frame(
            &mut w,
            &ToWorker::Hello {
                version: IPC_VERSION,
            },
        )
        .unwrap();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            while let Ok(m) = read_frame::<_, ToDaemon>(&mut r) {
                if tx.send(m).is_err() {
                    return;
                }
            }
        });
        Self {
            w,
            rx,
            seen: Vec::new(),
            ack_path,
        }
    }

    pub fn send(&mut self, m: ToWorker) {
        write_frame(&mut self.w, &m).unwrap();
    }

    /// Next frame matching `pred` within `timeout` (every frame is kept in `seen`).
    pub fn wait_for(
        &mut self,
        timeout: Duration,
        pred: impl Fn(&ToDaemon) -> bool,
    ) -> Option<ToDaemon> {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left) {
                Ok(m) => {
                    self.seen.push(m.clone());
                    if pred(&m) {
                        return Some(m);
                    }
                }
                Err(_) => return None,
            }
        }
    }

    /// Frames received during `d`.
    pub fn collect(&mut self, d: Duration) -> Vec<ToDaemon> {
        let deadline = Instant::now() + d;
        let mut out = Vec::new();
        while let Ok(m) = self
            .rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            self.seen.push(m.clone());
            out.push(m);
        }
        out
    }

    /// Every frame received so far, including the ones still queued. Meant for after the worker
    /// exited: it reads until the connection is closed (at most 2 s, or 200 ms of silence).
    pub fn all(&mut self) -> &[ToDaemon] {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            match self.rx.recv_timeout(Duration::from_millis(200)) {
                Ok(m) => self.seen.push(m),
                Err(_) => break,
            }
        }
        &self.seen
    }

    /// Closes the daemon side.
    pub fn close(self) {
        let _ = self.w.shutdown(std::net::Shutdown::Both);
    }

    pub fn ack(&self) -> Option<Ack> {
        read_ack_file(&self.ack_path).unwrap()
    }

    /// Polls the ACK file until `pred` holds; returns the ACK and how long it took.
    pub fn wait_ack(
        &self,
        timeout: Duration,
        pred: impl Fn(&Ack) -> bool,
    ) -> Option<(Ack, Duration)> {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if let Some(a) = self.ack() {
                if pred(&a) {
                    return Some((a, start.elapsed()));
                }
            }
            thread::sleep(Duration::from_micros(500));
        }
        None
    }
}

/// A V3 work unit with the share bound `2^(target_bits + 18)` (k = 2048) or `+ 19` (k = 4096)
/// and a header nbits of 0x1a07fff8 (block bound ≈ 2^221 at k = 2048): blocks are practically
/// out of reach.
pub fn work_unit(shape: Shape, target_bits: usize, seed: u8, wu_id: u64) -> WorkUnit {
    let mut header = [0u8; 76];
    header[0..4].copy_from_slice(&0x2000_0000u32.to_le_bytes());
    header[4..36].fill(seed);
    header[36..68].fill(seed ^ 0x5a);
    header[68..72].copy_from_slice(&0x6666_6666u32.to_le_bytes());
    header[72..76].copy_from_slice(&0x1a07_fff8u32.to_le_bytes());
    let job = Job {
        job_id: format!("job-{seed}-{wu_id}"),
        header,
        target: U256::one() << target_bits,
        height: Some(1),
        diff: None,
        cert_version: Some(3),
    };
    WorkUnit::build(&job, shape, 3, wu_id).unwrap()
}
