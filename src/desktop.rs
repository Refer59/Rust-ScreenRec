//! Desktop integration on Linux: notifications, the user's folders, local
//! time, process priority, the clipboard. windows/desktop.rs and
//! macos/desktop.rs have the same functions.

use crate::Res;
use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use x11rb::connection::{Connection, RequestConnection as _};
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, CreateWindowAux, EventMask, PropMode, Property, SELECTION_NOTIFY_EVENT, SelectionNotifyEvent, SelectionRequestEvent,
    WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{COPY_DEPTH_FROM_PARENT, COPY_FROM_PARENT, NONE};

/// How to install ffmpeg here, for error messages.
pub const GET_FFMPEG: &str = "sudo apt install ffmpeg";

pub fn notify(title: &str, body: &str, icon: Option<&Path>) {
    let icon = icon.map_or("media-record".into(), |p| p.display().to_string());
    let _ = helper("notify-send").args(["-a", "screenrec", "-i", &icon, title, body]).spawn();
}

/// The desktop session our helpers (notify-send, wl-copy, the clipboard's owner) start in:
/// ours, but for what `act_in` set.
static ACT_IN: Mutex<Vec<(String, Option<String>)>> = Mutex::new(Vec::new());

/// What places us in our desktop session, for a service that acts for us (the OCR service, for
/// its clipboard copy and notification): the display and its authorization, D-Bus, the runtime
/// folder, and the PATH the helpers are found in; None for what isn't set. A value that isn't
/// UTF-8 stays out (the service keeps its own).
pub fn session_env() -> Vec<(String, Option<String>)> {
    let vars = ["DISPLAY", "XAUTHORITY", "WAYLAND_DISPLAY", "XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS", "PATH"];
    vars.into_iter()
        .filter_map(|k| match std::env::var(k) {
            Ok(v) => Some((k.to_owned(), Some(v))),
            Err(std::env::VarError::NotPresent) => Some((k.to_owned(), None)),
            Err(std::env::VarError::NotUnicode(_)) => None,
        })
        .collect()
}

/// From now on, start our helpers in the desktop session `env` (a client's `session_env`)
/// instead of ours. The OCR service does it for each job: it serves every display of the user,
/// and outlives the session it was started from (an X server restarted on the same display
/// has a new XAUTHORITY, and the old one no longer opens it).
pub fn act_in(env: Vec<(String, Option<String>)>) {
    *ACT_IN.lock().unwrap() = env;
}

/// The program `name`, to start in the session we act in.
fn helper(name: impl AsRef<OsStr>) -> Command {
    let mut cmd = Command::new(name);
    for (k, v) in ACT_IN.lock().unwrap().iter() {
        match v {
            Some(v) => cmd.env(k, v),
            None => cmd.env_remove(k),
        };
    }
    cmd
}

/// Whether the session we act in is a Wayland one.
fn wayland() -> bool {
    match ACT_IN.lock().unwrap().iter().find(|(k, _)| k == "WAYLAND_DISPLAY") {
        Some((_, v)) => v.is_some(),
        None => std::env::var_os("WAYLAND_DISPLAY").is_some(),
    }
}

/// The user's folder for `xdg_dir` (PICTURES, VIDEOS), else their home.
pub fn user_dir(xdg_dir: &str) -> PathBuf {
    let dir = std::process::Command::new("xdg-user-dir")
        .arg(xdg_dir)
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| std::env::var("HOME").unwrap_or_else(|_| ".".into()));
    PathBuf::from(dir)
}

/// Where we keep files we can always download again (the OCR's runtime and
/// models): $XDG_CACHE_HOME/screenrec, else ~/.cache/screenrec.
#[allow(dead_code)] // until the OCR downloads into it
pub fn cache_dir() -> PathBuf {
    let xdg = std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from).filter(|d| d.is_absolute()); // relative ones are invalid
    xdg.unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| ".".into())).join(".cache")).join("screenrec")
}

/// Where we keep files installed for good (the text recognizer's runtime and
/// models, put there by `screenrec install`): $XDG_DATA_HOME/screenrec, else
/// ~/.local/share/screenrec.
pub fn data_dir() -> PathBuf {
    let xdg = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).filter(|d| d.is_absolute());
    xdg.unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| ".".into())).join(".local/share")).join("screenrec")
}

/// Unix time `secs` as local (year, month, day, hour, minute, second).
pub fn local_time(secs: u64) -> (i32, i32, i32, i32, i32, i32) {
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&(secs as libc::time_t), &mut tm) };
    (tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min, tm.tm_sec)
}

/// Whatever else runs comes first.
pub fn lower_priority() {
    unsafe { libc::nice(10) };
}

/// Put the w×h BGRX image (rows `stride` bytes apart) on the clipboard, as a
/// PNG. It stays there after we exit, until something else is copied.
pub fn copy_image(bgrx: &[u8], w: usize, h: usize, stride: usize) -> Res<()> {
    clip("image/png", &crate::frame::png(bgrx, w, h, stride)?)
}

/// Put `text` on the clipboard. It stays there after we exit, until something
/// else is copied.
pub fn copy_text(text: &str) -> Res<()> {
    clip(TEXT, text.as_bytes())
}

const TEXT: &str = "text/plain;charset=utf-8";

/// The hidden command that owns the clipboard for us (see `clip`).
pub const CLIP_OWNER: &str = "clipboard-owner";

fn copy_failed() -> String {
    tr!("couldn't copy to the clipboard", "no se pudo copiar al portapapeles", "クリップボードにコピーできませんでした")
}

/// An X11 selection lives only as long as its owner, and screenrec exits
/// right after a screenshot: a detached copy of ourselves (`CLIP_OWNER`) owns
/// the clipboard until another client copies something. There is no
/// clipboard manager to hand it to (GNOME 42 on X11 runs none: nobody owns
/// CLIPBOARD_MANAGER). On Wayland, wl-copy does the same job.
fn clip(mime: &str, data: &[u8]) -> Res<()> {
    if wayland() {
        let mut wl = helper("wl-copy").args(["--type", mime]).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().map_err(|_| {
            tr!(
                "copying to the clipboard on Wayland needs wl-copy (sudo apt install wl-clipboard)",
                "copiar al portapapeles en Wayland necesita wl-copy (sudo apt install wl-clipboard)",
                "Wayland でクリップボードにコピーするには wl-copy が必要です (sudo apt install wl-clipboard)"
            )
        })?;
        let _ = wl.stdin.take().unwrap().write_all(data); // dropped: the end of the data; a failed wl-copy says so below
        return if wl.wait()?.success() { Ok(()) } else { Err(copy_failed().into()) };
    }
    // Its own process group: a Ctrl+C or a `timeout` meant for us must leave the clipboard alone.
    let mut owner = helper(std::env::current_exe()?);
    owner.args([CLIP_OWNER, mime]).current_dir("/").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).process_group(0);
    let mut owner = owner.spawn()?;
    let _ = owner.stdin.take().unwrap().write_all(data);
    let mut said = String::new();
    BufReader::new(owner.stdout.take().unwrap()).read_line(&mut said)?;
    std::thread::spawn(move || owner.wait()); // reaped when it exits, should we still be running
    match said.trim_end() {
        "ok" => Ok(()),
        "" => Err(copy_failed().into()),
        why => Err(why.into()),
    }
}

/// `screenrec clipboard-owner <mime>`, started by `clip`: owns the clipboard
/// with the data read from stdin, says "ok" (or what went wrong) on stdout,
/// then serves the data until another client takes the clipboard or the X
/// session ends.
pub fn own_clipboard(mime: Option<&String>) -> Res<()> {
    let mut data = Vec::new();
    std::io::stdin().read_to_end(&mut data)?;
    let owner = Owner::new(mime.map_or(TEXT, |m| m), data);
    println!("{}", owner.as_ref().map_or_else(|e| format!("{}: {e}", copy_failed()).replace('\n', " "), |_| "ok".into()));
    owner?.serve()
}

/// The clipboard's owner: an unmapped window of ours and the data in each
/// form it is offered in, (target, type, bytes).
struct Owner {
    conn: RustConnection,
    time: u32,
    targets: u32,
    timestamp: u32,
    incr: u32,
    offers: Vec<(u32, u32, Vec<u8>)>,
}

/// An INCR transfer in progress: the requestor's window and property, the
/// offer, how much was sent.
struct Incr {
    win: u32,
    prop: u32,
    offer: usize,
    at: usize,
}

impl Owner {
    fn new(mime: &str, data: Vec<u8>) -> Res<Self> {
        let (conn, screen) = x11rb::connect(None)?;
        let root = conn.setup().roots[screen].root;
        let names = ["CLIPBOARD", "TARGETS", "TIMESTAMP", "INCR", "UTF8_STRING", "TEXT", "text/plain", TEXT, mime];
        let cookies = names.iter().map(|n| conn.intern_atom(false, n.as_bytes())).collect::<Result<Vec<_>, _>>()?;
        let mut atoms = [0; 9];
        for (a, c) in atoms.iter_mut().zip(cookies) {
            *a = c.reply()?.atom;
        }
        let [clipboard, targets, timestamp, incr, utf8, text, plain, plain_utf8, own] = atoms;
        let offers = if mime == TEXT {
            let string = AtomEnum::STRING.into(); // Latin-1, what's beyond it a '?'
            let latin1 = String::from_utf8_lossy(&data).chars().map(|c| u8::try_from(c).unwrap_or(b'?')).collect();
            vec![(utf8, utf8, data.clone()), (plain_utf8, plain_utf8, data.clone()), (plain, plain, data.clone()), (text, utf8, data), (string, string, latin1)]
        } else {
            vec![(own, own, data)]
        };
        let win = conn.generate_id()?;
        let aux = CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE);
        conn.create_window(COPY_DEPTH_FROM_PARENT, win, root, 0, 0, 1, 1, 0, WindowClass::INPUT_ONLY, COPY_FROM_PARENT, &aux)?;
        // Ownership needs a real timestamp (ICCCM): naming the window brings one.
        conn.change_property8(PropMode::REPLACE, win, AtomEnum::WM_NAME, AtomEnum::STRING, b"screenrec clipboard")?;
        conn.flush()?;
        let time = loop {
            if let Event::PropertyNotify(e) = conn.wait_for_event()? {
                break e.time;
            }
        };
        conn.set_selection_owner(win, clipboard, time)?;
        if conn.get_selection_owner(clipboard)?.reply()?.owner != win {
            return Err(copy_failed().into());
        }
        Ok(Owner { conn, time, targets, timestamp, incr, offers })
    }

    /// Answer requests until another client owns the clipboard; then finish
    /// the transfers under way (for 5 s at most) and return.
    fn serve(self) -> Res<()> {
        let chunk = self.conn.maximum_request_bytes().saturating_sub(100).min(1 << 18); // GTK's: bigger goes by INCR
        let mut sending: Vec<Incr> = Vec::new();
        let mut lost = None;
        loop {
            let ev = match lost {
                None => self.conn.wait_for_event()?,
                Some(_) if sending.is_empty() => return Ok(()),
                Some(t) if Instant::now() > t + Duration::from_secs(5) => return Ok(()), // a requestor that stopped reading
                Some(_) => match self.conn.poll_for_event()? {
                    Some(ev) => ev,
                    None => {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                },
            };
            match ev {
                Event::SelectionRequest(e) => self.answer(&e, chunk, &mut sending)?,
                Event::SelectionClear(_) => lost = lost.or(Some(Instant::now())),
                // INCR: the requestor took the last chunk, here is the next; an empty one ends it.
                Event::PropertyNotify(e) if e.state == Property::DELETE => {
                    let Some(i) = sending.iter().position(|s| (s.win, s.prop) == (e.window, e.atom)) else { continue };
                    let ((_, ty, bytes), at) = (&self.offers[sending[i].offer], sending[i].at);
                    let n = (bytes.len() - at).min(chunk);
                    self.conn.change_property8(PropMode::REPLACE, e.window, e.atom, *ty, &bytes[at..at + n])?;
                    sending[i].at += n;
                    if n == 0 {
                        sending.remove(i);
                        if !sending.iter().any(|s| s.win == e.window) {
                            self.conn.change_window_attributes(e.window, &ChangeWindowAttributesAux::new().event_mask(EventMask::NO_EVENT))?;
                        }
                    }
                    self.conn.flush()?;
                }
                _ => {} // X errors included: a requestor's window may be gone already
            }
        }
    }

    /// Put what `e` asks for on the requestor's property, or refuse, and tell it.
    fn answer(&self, e: &SelectionRequestEvent, chunk: usize, sending: &mut Vec<Incr>) -> Res<()> {
        let prop = if e.property == NONE { e.target } else { e.property }; // obsolete requestors name none
        let conn = &self.conn;
        let ok = if e.target == self.targets {
            let list: Vec<u32> = [self.targets, self.timestamp].into_iter().chain(self.offers.iter().map(|o| o.0)).collect();
            conn.change_property32(PropMode::REPLACE, e.requestor, prop, AtomEnum::ATOM, &list)?;
            true
        } else if e.target == self.timestamp {
            conn.change_property32(PropMode::REPLACE, e.requestor, prop, AtomEnum::INTEGER, &[self.time])?;
            true
        } else if let Some(i) = self.offers.iter().position(|o| o.0 == e.target) {
            let (_, ty, bytes) = &self.offers[i];
            if bytes.len() <= chunk {
                conn.change_property8(PropMode::REPLACE, e.requestor, prop, *ty, bytes)?;
            } else {
                // Watch the requestor's properties first: its deletions ask for the chunks.
                conn.change_window_attributes(e.requestor, &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE))?;
                conn.change_property32(PropMode::REPLACE, e.requestor, prop, self.incr, &[bytes.len() as u32])?;
                sending.retain(|s| (s.win, s.prop) != (e.requestor, prop));
                sending.push(Incr { win: e.requestor, prop, offer: i, at: 0 });
            }
            true
        } else {
            false
        };
        let property = if ok { prop } else { NONE };
        let note = SelectionNotifyEvent { response_type: SELECTION_NOTIFY_EVENT, sequence: 0, time: e.time, requestor: e.requestor, selection: e.selection, target: e.target, property };
        conn.send_event(false, e.requestor, EventMask::NO_EVENT, note)?;
        conn.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers_start_in_the_session_we_act_in() {
        // The client's variables, those it has and those it hasn't, over ours (PATH is surely set here).
        act_in(vec![("DISPLAY".into(), Some(":77".into())), ("PATH".into(), None), ("WAYLAND_DISPLAY".into(), Some("wayland-9".into()))]);
        let out = helper("/usr/bin/env").output(); // not a shell: it would make up a PATH
        let on_wayland = wayland();
        act_in(Vec::new());
        let out = String::from_utf8(out.unwrap().stdout).unwrap();
        let mut got: Vec<_> = out.lines().filter(|l| ["DISPLAY=", "PATH=", "WAYLAND_DISPLAY="].iter().any(|v| l.starts_with(v))).collect();
        got.sort();
        assert_eq!(got, ["DISPLAY=:77", "WAYLAND_DISPLAY=wayland-9"]);
        assert!(on_wayland);
        assert_eq!(wayland(), std::env::var_os("WAYLAND_DISPLAY").is_some(), "ours again");
    }
}
