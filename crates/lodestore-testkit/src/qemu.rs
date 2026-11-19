//! Wrappers around `qemu-img`, `qemu-io` and `qemu-nbd` for tests that need a
//! real NBD server with persistent dirty bitmaps, plus a socket proxy that
//! records every read a client issues.

use std::io::{self, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

pub const MIN_QEMU: (u32, u32) = (10, 1);

const TOOLS: [&str; 3] = ["qemu-img", "qemu-nbd", "qemu-io"];

fn tool_version(tool: &str) -> Option<(u32, u32)> {
    let out = Command::new(tool).arg("--version").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let version = text.lines().next()?.split_whitespace().find_map(|w| {
        let (major, rest) = w.split_once('.')?;
        let minor = rest.split(|c: char| !c.is_ascii_digit()).next()?;
        Some((major.parse().ok()?, minor.parse().ok()?))
    })?;
    Some(version)
}

/// True when all three tools are installed and `qemu-nbd` is at least
/// [`MIN_QEMU`]. In CI a missing or old tool panics instead of skipping.
pub fn available() -> bool {
    let nbd = tool_version("qemu-nbd");
    let all = TOOLS.iter().all(|t| tool_version(t).is_some());
    let ok = all && nbd.is_some_and(|v| v >= MIN_QEMU);
    if !ok && std::env::var_os("CI").is_some() {
        panic!(
            "qemu-img, qemu-nbd and qemu-io {}.{} or newer are required in CI (found qemu-nbd {nbd:?})",
            MIN_QEMU.0, MIN_QEMU.1
        );
    }
    ok
}

fn run(cmd: &mut Command) -> String {
    let out = cmd.output().unwrap_or_else(|e| panic!("{cmd:?}: {e}"));
    assert!(
        out.status.success(),
        "{cmd:?} failed ({}): {}{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf-8 output")
}

/// A short socket folder: unix socket paths are limited to 104 bytes on macOS.
pub fn socket_dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("lq")
        .tempdir_in("/tmp")
        .expect("socket folder")
}

pub struct Qcow2 {
    path: PathBuf,
}

impl Qcow2 {
    pub fn create(path: &Path, size: u64) -> Self {
        run(Command::new("qemu-img")
            .args(["create", "-f", "qcow2"])
            .arg(path)
            .arg(size.to_string()));
        Self {
            path: path.to_owned(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn add_bitmap(&self, name: &str, granularity: u64) {
        run(Command::new("qemu-img")
            .args(["bitmap", "--add", "--granularity"])
            .arg(granularity.to_string())
            .arg(&self.path)
            .arg(name));
    }

    pub fn remove_bitmap(&self, name: &str) {
        run(Command::new("qemu-img")
            .args(["bitmap", "--remove"])
            .arg(&self.path)
            .arg(name));
    }

    pub fn remove_all_bitmaps(&self) {
        let info = self.info();
        let bitmaps = info["format-specific"]["data"]["bitmaps"].as_array();
        for bitmap in bitmaps.into_iter().flatten() {
            self.remove_bitmap(bitmap["name"].as_str().expect("bitmap name"));
        }
    }

    fn io(&self, command: String) {
        run(Command::new("qemu-io")
            .args(["-f", "qcow2", "-c"])
            .arg(command)
            .arg(&self.path));
    }

    pub fn write(&self, offset: u64, len: u64, pattern: u8) {
        self.io(format!("write -P {pattern:#04x} {offset} {len}"));
    }

    pub fn write_zeroes(&self, offset: u64, len: u64) {
        self.io(format!("write -z {offset} {len}"));
    }

    /// Drops every bitmap first and does not recreate them: a resized qcow2
    /// must not carry bitmaps of the old size.
    pub fn resize(&self, size: u64) {
        self.remove_all_bitmaps();
        run(Command::new("qemu-img")
            .args(["resize", "--shrink", "-f", "qcow2"])
            .arg(&self.path)
            .arg(size.to_string()));
    }

    pub fn to_raw(&self, out: &Path) {
        run(Command::new("qemu-img")
            .args(["convert", "-f", "qcow2", "-O", "raw"])
            .arg(&self.path)
            .arg(out));
    }

    fn info(&self) -> serde_json::Value {
        let out = run(Command::new("qemu-img")
            .args(["info", "--output=json", "-f", "qcow2"])
            .arg(&self.path));
        serde_json::from_str(&out).expect("qemu-img info json")
    }

    pub fn virtual_size(&self) -> u64 {
        self.info()["virtual-size"].as_u64().expect("virtual-size")
    }
}

/// `qemu-nbd -r` on one unix socket exporting the named bitmaps.
pub struct NbdExport {
    child: Child,
    socket: PathBuf,
}

impl NbdExport {
    pub fn socket_in(socket_dir: &Path) -> PathBuf {
        socket_dir.join("q")
    }

    pub fn start(qcow2: &Qcow2, socket_dir: &Path, bitmaps: &[&str]) -> Self {
        let socket = Self::socket_in(socket_dir);
        let _ = std::fs::remove_file(&socket);
        let mut cmd = Command::new("qemu-nbd");
        cmd.args(["-r", "-f", "qcow2"]);
        for bitmap in bitmaps {
            cmd.arg("-B").arg(bitmap);
        }
        cmd.arg("-k")
            .arg(&socket)
            .arg("-t")
            .arg(qcow2.path())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().unwrap_or_else(|e| panic!("{cmd:?}: {e}"));
        let deadline = Instant::now() + Duration::from_secs(10);
        while !socket.exists() {
            if let Some(status) = child.try_wait().expect("polling qemu-nbd") {
                let mut stderr = String::new();
                if let Some(mut pipe) = child.stderr.take() {
                    let _ = pipe.read_to_string(&mut stderr);
                }
                panic!("qemu-nbd exited early ({status}): {stderr}");
            }
            assert!(
                Instant::now() < deadline,
                "qemu-nbd did not open its socket"
            );
            thread::sleep(Duration::from_millis(20));
        }
        Self { child, socket }
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    pub fn stop(self) {}
}

impl Drop for NbdExport {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.socket);
    }
}

const OPT_MAGIC: u64 = 0x4948_4156_454F_5054;
const REQUEST_MAGIC: u32 = 0x2560_9513;
const OPT_EXPORT_NAME: u32 = 1;
const OPT_ABORT: u32 = 2;
const OPT_GO: u32 = 7;
const CMD_READ: u16 = 0;
const CMD_WRITE: u16 = 1;
const CMD_DISC: u16 = 2;

/// Reads `n` bytes from the client, forwards them upstream and returns them.
fn pass(client: &mut UnixStream, upstream: &mut UnixStream, n: usize) -> io::Result<Vec<u8>> {
    let mut buf = vec![0u8; n];
    client.read_exact(&mut buf)?;
    upstream.write_all(&buf)?;
    Ok(buf)
}

fn parse_client(
    client: &mut UnixStream,
    upstream: &mut UnixStream,
    reads: &Mutex<Vec<(u64, u32)>>,
) -> io::Result<()> {
    pass(client, upstream, 4)?;
    loop {
        let head = pass(client, upstream, 16)?;
        let magic = u64::from_be_bytes(head[..8].try_into().unwrap());
        if magic != OPT_MAGIC {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let option = u32::from_be_bytes(head[8..12].try_into().unwrap());
        let len = u32::from_be_bytes(head[12..16].try_into().unwrap()) as usize;
        pass(client, upstream, len)?;
        match option {
            OPT_GO | OPT_EXPORT_NAME => break,
            OPT_ABORT => return Ok(()),
            _ => {}
        }
    }
    loop {
        let req = pass(client, upstream, 28)?;
        if u32::from_be_bytes(req[..4].try_into().unwrap()) != REQUEST_MAGIC {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let kind = u16::from_be_bytes(req[6..8].try_into().unwrap());
        let offset = u64::from_be_bytes(req[16..24].try_into().unwrap());
        let length = u32::from_be_bytes(req[24..28].try_into().unwrap());
        match kind {
            CMD_READ => reads.lock().unwrap().push((offset, length)),
            CMD_WRITE => {
                pass(client, upstream, length as usize)?;
            }
            CMD_DISC => return Ok(()),
            _ => {}
        }
    }
}

/// A unix socket proxy in front of an NBD server that records the offset and
/// length of every `CMD_READ` the client sends.
pub struct ReadLog {
    socket: PathBuf,
    reads: Arc<Mutex<Vec<(u64, u32)>>>,
    broken: Arc<AtomicBool>,
}

impl ReadLog {
    pub fn start(upstream: &Path, socket_dir: &Path) -> Self {
        let socket = socket_dir.join("p");
        let listener = UnixListener::bind(&socket).expect("binding the read log socket");
        let reads: Arc<Mutex<Vec<(u64, u32)>>> = Arc::default();
        let broken = Arc::new(AtomicBool::new(false));
        let (log, flag, upstream) = (reads.clone(), broken.clone(), upstream.to_owned());
        thread::spawn(move || {
            for client in listener.incoming().flatten() {
                let (log, flag, upstream) = (log.clone(), flag.clone(), upstream.clone());
                thread::spawn(move || {
                    let _ = serve(client, &upstream, &log, &flag);
                });
            }
        });
        Self {
            socket,
            reads,
            broken,
        }
    }

    pub fn uri(&self) -> String {
        format!("nbd+unix:///?socket={}", self.socket.display())
    }

    pub fn reads(&self) -> Vec<(u64, u32)> {
        assert!(
            !self.broken.load(Ordering::SeqCst),
            "the read log could not parse the client stream"
        );
        self.reads.lock().unwrap().clone()
    }

    pub fn clear(&self) {
        self.reads.lock().unwrap().clear();
    }
}

fn serve(
    mut client: UnixStream,
    upstream: &Path,
    reads: &Mutex<Vec<(u64, u32)>>,
    broken: &AtomicBool,
) -> io::Result<()> {
    let mut up = UnixStream::connect(upstream)?;
    let mut up_read = up.try_clone()?;
    let mut client_write = client.try_clone()?;
    let down = thread::spawn(move || {
        let _ = io::copy(&mut up_read, &mut client_write);
        let _ = client_write.shutdown(std::net::Shutdown::Both);
    });
    let result = parse_client(&mut client, &mut up, reads);
    if matches!(&result, Err(e) if e.kind() == io::ErrorKind::InvalidData) {
        broken.store(true, Ordering::SeqCst);
    }
    let _ = up.shutdown(std::net::Shutdown::Write);
    let _ = down.join();
    result
}

impl Drop for ReadLog {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket);
    }
}
