//! What a background helper (the OCR service) needs from the OS, the same on
//! every target: to start detached from whoever starts it, a per-user
//! endpoint that one process at a time serves (a Unix socket, a named pipe on
//! Windows), and files downloaded into the cache.

use crate::Res;
#[cfg(windows)]
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Start `screenrec <args>` in the background: it keeps running after we
/// exit, and a Ctrl+C, a `timeout` or a closed console meant for us doesn't
/// reach it.
pub fn spawn_detached(args: &[&str]) -> Res<()> {
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.args(args).current_dir(std::env::temp_dir()).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS};
        std::os::windows::process::CommandExt::creation_flags(&mut cmd, DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP); // a console program: not ours
    }
    let mut child = cmd.spawn()?;
    std::thread::spawn(move || child.wait()); // reaped when it exits, should we still be running
    Ok(())
}

/// A connection to an endpoint: bytes both ways, and `try_clone` for a second
/// thread. One thread may wait in a read while another writes.
#[cfg(unix)]
pub type Conn = std::os::unix::net::UnixStream;

/// On Windows, a named pipe opened for overlapped I/O, each read and write
/// waited for on its own. A synchronous pipe handle (a plain File, clones
/// included) queues every operation behind the one under way, so a write
/// would wait for a read that waits for the other end.
#[cfg(windows)]
pub struct Conn(std::os::windows::io::OwnedHandle);

#[cfg(windows)]
impl Conn {
    /// Another handle to the same connection, for another thread.
    pub fn try_clone(&self) -> io::Result<Conn> {
        Ok(Conn(self.0.try_clone()?))
    }

    fn io(&self, read: bool, buf: *mut u8, len: usize) -> io::Result<usize> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
        let (h, len) = (self.0.as_raw_handle(), len.min(u32::MAX as usize) as u32);
        let n = overlapped(h, |ov| unsafe { if read { ReadFile(h, buf, len, std::ptr::null_mut(), ov) } else { WriteFile(h, buf, len, std::ptr::null_mut(), ov) } });
        match n {
            Err(e) if read && e.raw_os_error() == Some(windows_sys::Win32::Foundation::ERROR_BROKEN_PIPE as i32) => Ok(0), // the other end hung up
            n => n.map(|n| n as usize),
        }
    }
}

#[cfg(windows)]
impl io::Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.io(true, buf.as_mut_ptr(), buf.len())
    }
}

#[cfg(windows)]
impl io::Write for Conn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.io(false, buf.as_ptr().cast_mut(), buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Start one overlapped operation on `h` (`start` gets its OVERLAPPED) and wait for it: the
/// bytes it moved. Each has its own event, so operations on other threads don't wake it.
#[cfg(windows)]
fn overlapped(h: windows_sys::Win32::Foundation::HANDLE, start: impl FnOnce(*mut windows_sys::Win32::System::IO::OVERLAPPED) -> i32) -> io::Result<u32> {
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_IO_PENDING, GetLastError};
    use windows_sys::Win32::System::IO::{GetOverlappedResult, OVERLAPPED};
    use windows_sys::Win32::System::Threading::CreateEventW;
    let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
    ov.hEvent = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
    if ov.hEvent.is_null() {
        return Err(io::Error::last_os_error());
    }
    let mut n = 0;
    let done = (start(&mut ov) != 0 || unsafe { GetLastError() } == ERROR_IO_PENDING) && unsafe { GetOverlappedResult(h, &ov, &mut n, 1) } != 0;
    let res = if done { Ok(n) } else { Err(io::Error::last_os_error()) };
    unsafe { CloseHandle(ov.hEvent) };
    res
}

/// The endpoint `name` (say, "ocr"), served by this process.
#[cfg(unix)]
pub struct Listener {
    sock: std::os::unix::net::UnixListener,
    _lock: std::fs::File, // held while we serve
}

/// The socket for `name`, in a folder only this user can enter: the runtime
/// folder on Linux, $TMPDIR on macOS, else (or if that path is too long for
/// a socket) a folder of ours in /tmp.
#[cfg(unix)]
fn endpoint(name: &str) -> Res<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    let file = format!("screenrec-{name}.sock");
    let dir = std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()).map(PathBuf::from).or_else(|| cfg!(target_os = "macos").then(std::env::temp_dir));
    if let Some(dir) = dir.filter(|d| d.join(&file).as_os_str().len() < 100) {
        return Ok(dir.join(file)); // sun_path holds 104 bytes on macOS, 108 on Linux
    }
    let uid = unsafe { libc::getuid() };
    let dir = PathBuf::from(format!("/tmp/screenrec-{uid}"));
    let _ = std::fs::DirBuilder::new().mode(0o700).create(&dir);
    let m = std::fs::symlink_metadata(&dir)?;
    if !m.is_dir() || m.uid() != uid || m.mode() & 0o077 != 0 {
        return Err(format!("{} is not a private folder of this user", dir.display()).into());
    }
    Ok(dir.join(file))
}

/// Serve the endpoint `name`; None if another process already does.
#[cfg(unix)]
pub fn listen(name: &str) -> Res<Option<Listener>> {
    let path = endpoint(name)?;
    let lock = std::fs::File::create(path.with_extension("lock"))?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Ok(None),
        Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
    }
    let _ = std::fs::remove_file(&path); // left by one that died
    Ok(Some(Listener { sock: std::os::unix::net::UnixListener::bind(&path)?, _lock: lock }))
}

#[cfg(unix)]
impl Listener {
    /// The next client to connect.
    pub fn accept(&mut self) -> Res<Conn> {
        Ok(self.sock.accept()?.0)
    }
}

/// Connect to the process that serves `name`; an error if none does.
#[cfg(unix)]
pub fn connect(name: &str) -> Res<Conn> {
    Ok(Conn::connect(endpoint(name)?)?)
}

#[cfg(windows)]
pub struct Listener {
    name: Vec<u16>,
    next: std::os::windows::io::OwnedHandle, // the pipe instance the next client gets
}

/// The pipe for `name`, NUL-terminated UTF-16. Pipe names are machine-wide: one per user.
/// ponytail: another local user could create it first and get our clients;
/// a random per-user name kept in data_dir() fixes that if it matters.
#[cfg(windows)]
fn pipe(name: &str) -> Vec<u16> {
    let user = std::env::var("USERNAME").unwrap_or_default();
    format!(r"\\.\pipe\screenrec-{name}-{user}").encode_utf16().chain([0]).collect()
}

/// A new instance of the pipe `name`; `first` fails if the pipe already exists.
#[cfg(windows)]
fn instance(name: &[u16], first: bool) -> io::Result<std::os::windows::io::OwnedHandle> {
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, PIPE_ACCESS_DUPLEX};
    use windows_sys::Win32::System::Pipes::{CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT};
    let open = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | if first { FILE_FLAG_FIRST_PIPE_INSTANCE } else { 0 };
    let mode = PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS;
    let h = unsafe { CreateNamedPipeW(name.as_ptr(), open, mode, PIPE_UNLIMITED_INSTANCES, 1 << 16, 1 << 16, 0, std::ptr::null()) };
    if h == INVALID_HANDLE_VALUE { Err(io::Error::last_os_error()) } else { Ok(unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(h) }) }
}

/// Serve the endpoint `name`; None if another process already does.
#[cfg(windows)]
pub fn listen(name: &str) -> Res<Option<Listener>> {
    let name = pipe(name);
    match instance(&name, true) {
        Ok(next) => Ok(Some(Listener { name, next })),
        Err(e) if e.raw_os_error() == Some(windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED as i32) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

#[cfg(windows)]
impl Listener {
    /// The next client to connect.
    pub fn accept(&mut self) -> Res<Conn> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::ERROR_PIPE_CONNECTED;
        use windows_sys::Win32::System::Pipes::ConnectNamedPipe;
        loop {
            // A client may have connected already, since the instance was made: that's fine too.
            let h = self.next.as_raw_handle();
            let ok = match overlapped(h, |ov| unsafe { ConnectNamedPipe(h, ov) }) {
                Ok(_) => true,
                Err(e) => e.raw_os_error() == Some(ERROR_PIPE_CONNECTED as i32),
            };
            let this = std::mem::replace(&mut self.next, instance(&self.name, false)?);
            if ok {
                return Ok(Conn(this));
            } // else it's dropped: a client that left before we got to it
        }
    }
}

/// Connect to the process that serves `name`; an error if none does.
#[cfg(windows)]
pub fn connect(name: &str) -> Res<Conn> {
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_FLAG_OVERLAPPED, OPEN_EXISTING};
    let h = unsafe { CreateFileW(pipe(name).as_ptr(), GENERIC_READ | GENERIC_WRITE, 0, std::ptr::null(), OPEN_EXISTING, FILE_FLAG_OVERLAPPED, std::ptr::null_mut()) };
    if h == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error().into());
    }
    Ok(Conn(unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(h) }))
}

/// Download `url` to `dest` with the system's curl (Windows 10+, macOS, most
/// Linux) or wget (Linux: Ubuntu ships it, not always curl). `dest` appears
/// only once the download is complete.
pub fn download(url: &str, dest: &Path) -> Res<()> {
    let mut part = dest.as_os_str().to_owned();
    part.push(".part");
    let part = PathBuf::from(part);
    let ok = match Command::new("curl").args(["-fsSL", "--retry", "3", "--connect-timeout", "20", "-o"]).arg(&part).arg(url).stdin(Stdio::null()).status() {
        Ok(s) => s.success(),
        Err(_) => Command::new("wget")
            .args(["-q", "-O"])
            .arg(&part)
            .arg(url)
            .stdin(Stdio::null())
            .status()
            .map_err(|_| tr!("downloading needs curl or wget", "para descargar hace falta curl o wget", "ダウンロードには curl か wget が必要です"))?
            .success(),
    };
    if !ok {
        let _ = std::fs::remove_file(&part);
        return Err(tr!("couldn't download {}", "no se pudo descargar {}", "{} をダウンロードできませんでした", url).into());
    }
    Ok(std::fs::rename(&part, dest)?)
}

/// Unpack `members` (their names exactly as the archive lists them; all of it
/// if none) of the .tgz or .zip `archive` into `dir`, with the system's tar
/// (GNU tar on Linux, bsdtar on macOS and Windows 10+, which reads .zip too).
/// Name only what is needed: ONNX Runtime's Windows zip holds a 420 MB .pdb
/// next to its 16 MB DLL, its macOS tgz a 73 MB dSYM.
pub fn unpack(archive: &Path, dir: &Path, members: &[&str]) -> Res<()> {
    std::fs::create_dir_all(dir)?;
    // Not the archive's owners (uid 1001 for ONNX Runtime's): as root, tar would chown to them,
    // which fails in a user namespace or rootless container, and is wrong anywhere.
    let st = tar().arg("-xf").arg(archive).arg("--no-same-owner").arg("-C").arg(dir).args(members).stdin(Stdio::null()).status();
    if st.map_err(|_| tr!("unpacking needs tar", "para descomprimir hace falta tar", "展開には tar が必要です"))?.success() {
        Ok(())
    } else {
        Err(tr!("couldn't unpack {}", "no se pudo descomprimir {}", "{} を展開できませんでした", archive.display()).into())
    }
}

fn tar() -> Command {
    #[cfg(windows)] // Windows' own, not a GNU tar from Git, which reads "C:" as a host name
    let tar = PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into())).join(r"System32\tar.exe");
    #[cfg(not(windows))]
    let tar = "tar";
    Command::new(tar)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::time::Duration;

    #[test]
    fn one_process_serves_and_clients_talk_to_it() {
        let name = format!("test-{}", std::process::id());
        let mut server = listen(&name).unwrap().expect("nobody serves it yet");
        assert!(listen(&name).unwrap().is_none(), "a second server must see the first");
        let to = name.clone();
        let client = std::thread::spawn(move || {
            let mut c = connect(&to).unwrap();
            c.write_all(b"ping").unwrap();
            let mut back = [0; 4];
            c.read_exact(&mut back).unwrap();
            back
        });
        let mut conn = server.accept().unwrap();
        let mut got = [0; 4];
        conn.read_exact(&mut got).unwrap();
        conn.write_all(b"pong").unwrap();
        assert_eq!((&got, &client.join().unwrap()), (b"ping", b"pong"));
        drop(server);
        #[cfg(unix)]
        for p in [endpoint(&name).unwrap(), endpoint(&name).unwrap().with_extension("lock")] {
            let _ = std::fs::remove_file(p);
        }
    }

    #[test]
    fn a_waiting_reader_does_not_hold_up_a_write() {
        // A recording waits for the service's answers on one thread while it sends frames on
        // another, and the service reads the next frame while it answers the last: both ends.
        use std::sync::mpsc::channel;
        let name = format!("duplex-{}", std::process::id());
        let mut server = listen(&name).unwrap().expect("nobody serves it yet");
        let (to, (said, heard), t) = (name.clone(), channel(), Duration::from_secs(10));
        std::thread::spawn(move || {
            let mut c = connect(&to).unwrap();
            let mut answers = c.try_clone().unwrap();
            let waiting = std::thread::spawn(move || {
                let mut b = [0; 1];
                answers.read_exact(&mut b).map(|()| b[0])
            });
            std::thread::sleep(Duration::from_millis(200)); // it waits for an answer by now
            c.write_all(b"1").unwrap();
            said.send(waiting.join().unwrap().unwrap()).unwrap();
            c.write_all(b"2").unwrap();
        });
        let mut conn = server.accept().unwrap();
        let mut answer = conn.try_clone().unwrap();
        let (got, frames) = channel();
        std::thread::spawn(move || {
            let mut b = [0; 1];
            while conn.read_exact(&mut b).is_ok() {
                got.send(b[0]).unwrap();
            }
        });
        assert_eq!(frames.recv_timeout(t).expect("the client's write waited for its reader"), b'1');
        std::thread::sleep(Duration::from_millis(200)); // the service waits for the next frame by now
        let (wrote, written) = channel();
        std::thread::spawn(move || wrote.send(answer.write_all(b"a").is_ok()));
        assert!(written.recv_timeout(t).expect("the service's write waited for its reader"));
        assert_eq!(heard.recv_timeout(t).unwrap(), b'a');
        assert_eq!(frames.recv_timeout(t).unwrap(), b'2');
        drop(server);
        #[cfg(unix)]
        for p in [endpoint(&name).unwrap(), endpoint(&name).unwrap().with_extension("lock")] {
            let _ = std::fs::remove_file(p);
        }
    }

    #[test]
    fn downloads_appear_whole_and_unpack() {
        let dir = std::env::temp_dir().join(format!("screenrec-fetch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file_url = |p: &Path| format!("file:///{}", p.display().to_string().replace('\\', "/").trim_start_matches('/'));
        std::fs::write(dir.join("model.bin"), "ñ 日本語").unwrap();
        std::fs::write(dir.join("symbols.pdb"), "big").unwrap();
        assert!(tar().arg("-cf").arg(dir.join("a.tar")).arg("-C").arg(&dir).args(["model.bin", "symbols.pdb"]).status().unwrap().success());
        download(&file_url(&dir.join("a.tar")), &dir.join("got.tar")).unwrap();
        unpack(&dir.join("got.tar"), &dir.join("out"), &["model.bin"]).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("out/model.bin")).unwrap(), "ñ 日本語");
        assert!(!dir.join("out/symbols.pdb").exists(), "only the members asked for");
        assert!(download(&file_url(&dir.join("missing")), &dir.join("no.bin")).is_err());
        assert!(!dir.join("no.bin").exists() && !dir.join("no.bin.part").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
