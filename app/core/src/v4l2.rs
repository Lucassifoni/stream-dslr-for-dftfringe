//! V4L2 device discovery and controls through raw `ioctl(2)` (Linux).
//!
//! The kernel structs from `linux/videodev2.h` (stable UAPI) are handled as
//! byte buffers at fixed offsets, so everything above the single ioctl call
//! is pure and tested against a fake [`Ioctl`] implementation. No crate is
//! needed: `ioctl` is declared here and resolved from the libc std links.
//!
//! Controls are read and written on a separate open of the device while
//! ffmpeg streams from it; V4L2 allows that, only streaming is exclusive.

use std::io;

use crate::camera::{self, Backend, CameraDevice, CameraMode, FrameRate};

/// One ioctl on an open V4L2 device; `arg` is the in/out struct buffer.
pub trait Ioctl {
    fn ioctl(&self, request: u64, arg: &mut [u8]) -> io::Result<()>;
}

const fn ioc(dir: u64, nr: u64, size: usize) -> u64 {
    (dir << 30) | ((size as u64) << 16) | ((b'V' as u64) << 8) | nr
}
const fn ior(nr: u64, size: usize) -> u64 {
    ioc(2, nr, size)
}
const fn iowr(nr: u64, size: usize) -> u64 {
    ioc(3, nr, size)
}

pub const CAPABILITY_SIZE: usize = 104;
pub const FMTDESC_SIZE: usize = 64;
pub const FRMSIZEENUM_SIZE: usize = 44;
pub const FRMIVALENUM_SIZE: usize = 52;
pub const QUERYCTRL_SIZE: usize = 68;
pub const QUERYMENU_SIZE: usize = 44;
pub const CONTROL_SIZE: usize = 8;

pub const VIDIOC_QUERYCAP: u64 = ior(0, CAPABILITY_SIZE);
pub const VIDIOC_ENUM_FMT: u64 = iowr(2, FMTDESC_SIZE);
pub const VIDIOC_G_CTRL: u64 = iowr(27, CONTROL_SIZE);
pub const VIDIOC_S_CTRL: u64 = iowr(28, CONTROL_SIZE);
pub const VIDIOC_QUERYCTRL: u64 = iowr(36, QUERYCTRL_SIZE);
pub const VIDIOC_QUERYMENU: u64 = iowr(37, QUERYMENU_SIZE);
pub const VIDIOC_ENUM_FRAMESIZES: u64 = iowr(74, FRMSIZEENUM_SIZE);
pub const VIDIOC_ENUM_FRAMEINTERVALS: u64 = iowr(75, FRMIVALENUM_SIZE);

pub const CAP_VIDEO_CAPTURE: u32 = 0x0000_0001;
pub const CAP_DEVICE_CAPS: u32 = 0x8000_0000;
pub const BUF_TYPE_VIDEO_CAPTURE: u32 = 1;
pub const FMT_FLAG_COMPRESSED: u32 = 0x0001;
pub const FRMSIZE_TYPE_DISCRETE: u32 = 1;
pub const FRMIVAL_TYPE_DISCRETE: u32 = 1;

pub const CTRL_FLAG_DISABLED: u32 = 0x0001;
pub const CTRL_FLAG_GRABBED: u32 = 0x0002;
pub const CTRL_FLAG_READ_ONLY: u32 = 0x0004;
pub const CTRL_FLAG_INACTIVE: u32 = 0x0010;
pub const CTRL_FLAG_WRITE_ONLY: u32 = 0x0040;
pub const CTRL_FLAG_NEXT_CTRL: u32 = 0x8000_0000;

pub const CTRL_TYPE_INTEGER: u32 = 1;
pub const CTRL_TYPE_BOOLEAN: u32 = 2;
pub const CTRL_TYPE_MENU: u32 = 3;
pub const CTRL_TYPE_BUTTON: u32 = 4;
pub const CTRL_TYPE_INTEGER_MENU: u32 = 9;

const EINVAL: i32 = 22;
/// Upper bound on any enumeration, against drivers that never return EINVAL.
const MAX_ENTRIES: u32 = 1024;

fn u32_at(buf: &[u8], offset: usize) -> u32 {
    u32::from_ne_bytes(buf[offset..offset + 4].try_into().expect("4 bytes"))
}

fn i32_at(buf: &[u8], offset: usize) -> i32 {
    i32::from_ne_bytes(buf[offset..offset + 4].try_into().expect("4 bytes"))
}

fn i64_at(buf: &[u8], offset: usize) -> i64 {
    i64::from_ne_bytes(buf[offset..offset + 8].try_into().expect("8 bytes"))
}

fn put_u32(buf: &mut [u8], offset: usize, value: u32) {
    buf[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
}

fn put_i32(buf: &mut [u8], offset: usize, value: i32) {
    buf[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
}

/// NUL-terminated fixed-size C string field.
fn c_string(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).trim().to_string()
}

/// `v4l2_fourcc(a, b, c, d)` as text, trailing spaces trimmed (`"Y16 "` → `"Y16"`).
pub fn fourcc_to_string(code: u32) -> String {
    String::from_utf8_lossy(&code.to_le_bytes()).trim_end().to_string()
}

/// Inverse of [`fourcc_to_string`]; pads to four characters with spaces.
pub fn fourcc_code(text: &str) -> Option<u32> {
    let bytes = text.as_bytes();
    match bytes.len() {
        1..=4 => {
            let mut code = [b' '; 4];
            code[..bytes.len()].copy_from_slice(bytes);
            Some(u32::from_le_bytes(code))
        }
        _ => None,
    }
}

fn is_einval(e: &io::Error) -> bool {
    e.raw_os_error() == Some(EINVAL)
}

/// Collect `query(0), query(1), ...` until the driver answers EINVAL.
fn enumerate<T>(mut query: impl FnMut(u32) -> io::Result<T>) -> io::Result<Vec<T>> {
    let mut out = Vec::new();
    for index in 0..MAX_ENTRIES {
        match query(index) {
            Ok(item) => out.push(item),
            Err(e) if is_einval(&e) => return Ok(out),
            Err(e) => return Err(e),
        }
    }
    Ok(out)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Capability {
    pub driver: String,
    pub card: String,
    pub bus_info: String,
    pub capabilities: u32,
    pub device_caps: u32,
}

impl Capability {
    /// Whether this node captures video (UVC also exposes metadata-only nodes).
    pub fn is_video_capture(&self) -> bool {
        let caps = match self.capabilities & CAP_DEVICE_CAPS {
            0 => self.capabilities,
            _ => self.device_caps,
        };
        caps & CAP_VIDEO_CAPTURE != 0
    }
}

pub fn query_capability(dev: &impl Ioctl) -> io::Result<Capability> {
    let mut buf = [0u8; CAPABILITY_SIZE];
    dev.ioctl(VIDIOC_QUERYCAP, &mut buf)?;
    Ok(Capability {
        driver: c_string(&buf[0..16]),
        card: c_string(&buf[16..48]),
        bus_info: c_string(&buf[48..80]),
        capabilities: u32_at(&buf, 84),
        device_caps: u32_at(&buf, 88),
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FormatDesc {
    pub fourcc: String,
    pub code: u32,
    pub description: String,
    pub compressed: bool,
}

pub fn formats(dev: &impl Ioctl) -> io::Result<Vec<FormatDesc>> {
    enumerate(|index| {
        let mut buf = [0u8; FMTDESC_SIZE];
        put_u32(&mut buf, 0, index);
        put_u32(&mut buf, 4, BUF_TYPE_VIDEO_CAPTURE);
        dev.ioctl(VIDIOC_ENUM_FMT, &mut buf)?;
        let code = u32_at(&buf, 44);
        Ok(FormatDesc {
            fourcc: fourcc_to_string(code),
            code,
            description: c_string(&buf[12..44]),
            compressed: u32_at(&buf, 8) & FMT_FLAG_COMPRESSED != 0,
        })
    })
}

/// Sizes for `code`, deduplicated. Stepwise / continuous ranges yield their max and min.
pub fn frame_sizes(dev: &impl Ioctl, code: u32) -> io::Result<Vec<(u32, u32)>> {
    let query = |index: u32| {
        let mut buf = [0u8; FRMSIZEENUM_SIZE];
        put_u32(&mut buf, 0, index);
        put_u32(&mut buf, 4, code);
        dev.ioctl(VIDIOC_ENUM_FRAMESIZES, &mut buf).map(|_| buf)
    };
    let first = match query(0) {
        Ok(buf) => buf,
        Err(e) if is_einval(&e) => return Ok(vec![]),
        Err(e) => return Err(e),
    };
    let sizes = match u32_at(&first, 8) {
        FRMSIZE_TYPE_DISCRETE => enumerate(|i| query(i).map(|b| (u32_at(&b, 12), u32_at(&b, 16))))?,
        _ => vec![(u32_at(&first, 16), u32_at(&first, 28)), (u32_at(&first, 12), u32_at(&first, 24))],
    };
    Ok(camera::dedup_keep_order(&sizes))
}

/// Frame rates for `code` at `width`x`height`, fastest first. Ranges yield their fastest and slowest.
pub fn frame_rates(dev: &impl Ioctl, code: u32, width: u32, height: u32) -> io::Result<Vec<FrameRate>> {
    let query = |index: u32| {
        let mut buf = [0u8; FRMIVALENUM_SIZE];
        put_u32(&mut buf, 0, index);
        put_u32(&mut buf, 4, code);
        put_u32(&mut buf, 8, width);
        put_u32(&mut buf, 12, height);
        dev.ioctl(VIDIOC_ENUM_FRAMEINTERVALS, &mut buf).map(|_| buf)
    };
    let interval = |buf: &[u8], offset: usize| FrameRate::from_interval(u32_at(buf, offset), u32_at(buf, offset + 4));
    let first = match query(0) {
        Ok(buf) => buf,
        Err(e) if is_einval(&e) => return Ok(vec![]),
        Err(e) => return Err(e),
    };
    let rates: Vec<FrameRate> = match u32_at(&first, 16) {
        FRMIVAL_TYPE_DISCRETE => enumerate(|i| query(i).map(|b| interval(&b, 20)))?.into_iter().flatten().collect(),
        _ => [interval(&first, 20), interval(&first, 28)].into_iter().flatten().collect(),
    };
    Ok(camera::sorted_rates(&rates))
}

/// Capture modes of a device: one per (format, size), for formats ffmpeg can request.
pub fn modes(dev: &impl Ioctl) -> io::Result<Vec<CameraMode>> {
    let mut out = Vec::new();
    for format in formats(dev)?.into_iter().filter(|f| camera::ffmpeg_input_format(&f.fourcc).is_some()) {
        for (width, height) in frame_sizes(dev, format.code)? {
            let rates = frame_rates(dev, format.code, width, height)?;
            if rates.is_empty() {
                continue;
            }
            out.push(CameraMode {
                format: Some(format.fourcc.clone()),
                format_label: format.description.clone(),
                width,
                height,
                rates,
                transcoded: !camera::is_passthrough(&format.fourcc),
            });
        }
    }
    Ok(out)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlKind {
    Integer,
    Boolean,
    Menu,
    IntegerMenu,
    Button,
}

impl ControlKind {
    fn from_type(kind: u32) -> Option<ControlKind> {
        match kind {
            CTRL_TYPE_INTEGER => Some(ControlKind::Integer),
            CTRL_TYPE_BOOLEAN => Some(ControlKind::Boolean),
            CTRL_TYPE_MENU => Some(ControlKind::Menu),
            CTRL_TYPE_INTEGER_MENU => Some(ControlKind::IntegerMenu),
            CTRL_TYPE_BUTTON => Some(ControlKind::Button),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuItem {
    pub index: i32,
    pub label: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Control {
    pub id: u32,
    pub name: String,
    pub kind: ControlKind,
    pub minimum: i32,
    pub maximum: i32,
    pub step: i32,
    pub default: i32,
    /// `None` for buttons, write-only controls, or when the driver refused the read.
    pub value: Option<i32>,
    pub flags: u32,
    /// Menu kinds only; drivers may leave gaps (UVC `auto_exposure` has 1 and 3).
    pub menu: Vec<MenuItem>,
}

impl Control {
    /// Disabled because another control (typically its "auto" switch) is in charge.
    pub fn inactive(&self) -> bool {
        self.flags & CTRL_FLAG_INACTIVE != 0
    }

    /// Cannot be changed right now (read-only, or grabbed while streaming).
    pub fn read_only(&self) -> bool {
        self.flags & (CTRL_FLAG_READ_ONLY | CTRL_FLAG_GRABBED) != 0
    }

    /// Check `value` against the control's range and menu before sending it.
    pub fn check(&self, value: i32) -> Result<(), String> {
        let in_menu = || self.menu.iter().any(|m| m.index == value);
        match self.kind {
            _ if self.read_only() => Err(format!("{} is read-only", self.name)),
            ControlKind::Button => Ok(()),
            ControlKind::Menu | ControlKind::IntegerMenu if !in_menu() => {
                Err(format!("{} has no menu entry {value}", self.name))
            }
            _ if !(self.minimum..=self.maximum).contains(&value) => Err(format!(
                "{} must be between {} and {}, got {value}",
                self.name, self.minimum, self.maximum
            )),
            _ => Ok(()),
        }
    }
}

fn query_control(dev: &impl Ioctl, id: u32) -> io::Result<[u8; QUERYCTRL_SIZE]> {
    let mut buf = [0u8; QUERYCTRL_SIZE];
    put_u32(&mut buf, 0, id);
    dev.ioctl(VIDIOC_QUERYCTRL, &mut buf).map(|_| buf)
}

fn menu_items(dev: &impl Ioctl, id: u32, kind: ControlKind, minimum: i32, maximum: i32) -> io::Result<Vec<MenuItem>> {
    let mut items = Vec::new();
    for index in minimum.max(0)..=maximum.min(minimum.max(0) + MAX_ENTRIES as i32) {
        let mut buf = [0u8; QUERYMENU_SIZE];
        put_u32(&mut buf, 0, id);
        put_u32(&mut buf, 4, index as u32);
        match dev.ioctl(VIDIOC_QUERYMENU, &mut buf) {
            Ok(()) => items.push(MenuItem {
                index,
                label: match kind {
                    ControlKind::IntegerMenu => i64_at(&buf, 8).to_string(),
                    _ => c_string(&buf[8..40]),
                },
            }),
            Err(e) if is_einval(&e) => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(items)
}

pub fn get_control(dev: &impl Ioctl, id: u32) -> io::Result<i32> {
    let mut buf = [0u8; CONTROL_SIZE];
    put_u32(&mut buf, 0, id);
    dev.ioctl(VIDIOC_G_CTRL, &mut buf)?;
    Ok(i32_at(&buf, 4))
}

pub fn set_control(dev: &impl Ioctl, id: u32, value: i32) -> io::Result<()> {
    let mut buf = [0u8; CONTROL_SIZE];
    put_u32(&mut buf, 0, id);
    put_i32(&mut buf, 4, value);
    dev.ioctl(VIDIOC_S_CTRL, &mut buf)
}

/// All user-facing controls, in driver order. Class headers, disabled
/// controls and types needing the extended API (64-bit, strings) are skipped.
pub fn controls(dev: &impl Ioctl) -> io::Result<Vec<Control>> {
    let mut out = Vec::new();
    let mut next = CTRL_FLAG_NEXT_CTRL;
    for _ in 0..MAX_ENTRIES {
        let buf = match query_control(dev, next) {
            Ok(buf) => buf,
            Err(e) if is_einval(&e) => break,
            Err(e) => return Err(e),
        };
        let id = u32_at(&buf, 0);
        let flags = u32_at(&buf, 56);
        if let (Some(kind), 0) = (ControlKind::from_type(u32_at(&buf, 4)), flags & CTRL_FLAG_DISABLED) {
            let (minimum, maximum) = (i32_at(&buf, 40), i32_at(&buf, 44));
            let readable = kind != ControlKind::Button && flags & CTRL_FLAG_WRITE_ONLY == 0;
            out.push(Control {
                id,
                name: c_string(&buf[8..40]),
                kind,
                minimum,
                maximum,
                step: i32_at(&buf, 48),
                default: i32_at(&buf, 52),
                value: readable.then(|| get_control(dev, id).ok()).flatten(),
                flags,
                menu: match kind {
                    ControlKind::Menu | ControlKind::IntegerMenu => menu_items(dev, id, kind, minimum, maximum)?,
                    _ => vec![],
                },
            });
        }
        if id | CTRL_FLAG_NEXT_CTRL <= next {
            break;
        }
        next = id | CTRL_FLAG_NEXT_CTRL;
    }
    Ok(out)
}

/// `/dev/videoN` paths among `/dev` entry names, sorted by N.
pub fn video_nodes<S: AsRef<str>>(names: impl IntoIterator<Item = S>) -> Vec<String> {
    let mut numbers: Vec<u32> = names
        .into_iter()
        .filter_map(|n| n.as_ref().strip_prefix("video").and_then(|d| d.parse().ok()))
        .collect();
    numbers.sort_unstable();
    numbers.into_iter().map(|n| format!("/dev/video{n}")).collect()
}

/// Describe the device at `path`; `None` when it is not a video capture node.
pub fn describe(dev: &impl Ioctl, path: &str) -> Option<CameraDevice> {
    let caps = match query_capability(dev) {
        Ok(caps) => caps,
        Err(e) => return Some(unusable(path, path, e)),
    };
    match caps.is_video_capture() {
        false => None,
        true => Some(match modes(dev) {
            Ok(modes) => CameraDevice {
                backend: Backend::V4l2,
                id: path.to_string(),
                name: caps.card,
                modes,
                error: None,
            },
            Err(e) => unusable(path, &caps.card, e),
        }),
    }
}

fn unusable(path: &str, name: &str, e: io::Error) -> CameraDevice {
    CameraDevice {
        backend: Backend::V4l2,
        id: path.to_string(),
        name: name.to_string(),
        modes: vec![],
        error: Some(describe_error(path, &e)),
    }
}

/// Actionable message for a failed open / ioctl on `path`.
pub fn describe_error(path: &str, e: &io::Error) -> String {
    match e.kind() {
        io::ErrorKind::PermissionDenied => {
            format!("{path}: permission denied — add your user to the `video` group (sudo usermod -aG video $USER), then log in again")
        }
        _ => format!("{path}: {e}"),
    }
}

/// Check `value` against control `id`, set it, and return the refreshed list
/// (changing an "auto" control toggles other controls' inactive flags).
pub fn apply_control(dev: &impl Ioctl, id: u32, value: i32) -> Result<Vec<Control>, String> {
    let before = controls(dev).map_err(|e| e.to_string())?;
    let control = before.iter().find(|c| c.id == id).ok_or_else(|| format!("no control {id:#x}"))?;
    control.check(value)?;
    set_control(dev, id, value).map_err(|e| format!("cannot set {}: {e}", control.name))?;
    controls(dev).map_err(|e| e.to_string())
}

#[cfg(target_os = "linux")]
pub use device::{discover, open, Device};

/// Controls of the device at `path`.
#[cfg(target_os = "linux")]
pub fn controls_at(path: &str) -> Result<Vec<Control>, String> {
    let dev = open(path).map_err(|e| describe_error(path, &e))?;
    controls(&dev).map_err(|e| describe_error(path, &e))
}

/// [`apply_control`] on the device at `path`.
#[cfg(target_os = "linux")]
pub fn apply_control_at(path: &str, id: u32, value: i32) -> Result<Vec<Control>, String> {
    let dev = open(path).map_err(|e| describe_error(path, &e))?;
    apply_control(&dev, id, value)
}

#[cfg(not(target_os = "linux"))]
/// V4L2 controls only exist on Linux.
pub fn controls_at(_path: &str) -> Result<Vec<Control>, String> {
    Ok(vec![])
}

#[cfg(not(target_os = "linux"))]
/// V4L2 controls only exist on Linux.
pub fn apply_control_at(_path: &str, _id: u32, _value: i32) -> Result<Vec<Control>, String> {
    Err("camera controls are only available on Linux".into())
}

#[cfg(not(target_os = "linux"))]
/// V4L2 only exists on Linux.
pub fn discover() -> Vec<CameraDevice> {
    vec![]
}

#[cfg(target_os = "linux")]
mod device {
    use std::fs::{self, File, OpenOptions};
    use std::io;
    use std::os::fd::AsRawFd;
    use std::os::raw::{c_int, c_ulong};

    use super::{describe, describe_error, unusable, video_nodes, Ioctl};
    use crate::camera::CameraDevice;

    extern "C" {
        fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    }

    /// An open V4L2 device node.
    pub struct Device(File);

    pub fn open(path: &str) -> io::Result<Device> {
        OpenOptions::new().read(true).write(true).open(path).map(Device)
    }

    impl Ioctl for Device {
        fn ioctl(&self, request: u64, arg: &mut [u8]) -> io::Result<()> {
            assert_eq!(((request >> 16) & 0x3fff) as usize, arg.len(), "ioctl buffer size mismatch");
            loop {
                // SAFETY: `arg` is a live, exclusively borrowed buffer whose length matches
                // the size encoded in `request`, so the kernel reads/writes stay in bounds.
                let rc = unsafe { ioctl(self.0.as_raw_fd(), request as c_ulong, arg.as_mut_ptr()) };
                match rc {
                    -1 => match io::Error::last_os_error() {
                        e if e.kind() == io::ErrorKind::Interrupted => continue,
                        e => return Err(e),
                    },
                    _ => return Ok(()),
                }
            }
        }
    }

    /// Every V4L2 video capture node under `/dev`.
    pub fn discover() -> Vec<CameraDevice> {
        let names = fs::read_dir("/dev")
            .map(|entries| entries.filter_map(Result::ok).map(|e| e.file_name().to_string_lossy().into_owned()).collect())
            .unwrap_or_else(|_| Vec::new());
        video_nodes(names)
            .into_iter()
            .filter_map(|path| match open(&path) {
                Ok(dev) => describe(&dev, &path),
                Err(e) => Some(CameraDevice { error: Some(describe_error(&path, &e)), ..unusable(&path, &path, e) }),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    type FakeSize = (u32, u32, Vec<(u32, u32)>);

    struct FakeFormat {
        fourcc: &'static str,
        description: &'static str,
        compressed: bool,
        sizes: Vec<FakeSize>,
    }

    struct FakeControl {
        id: u32,
        name: &'static str,
        kind: u32,
        range: (i32, i32, i32, i32),
        flags: u32,
        menu: Vec<(u32, &'static str)>,
    }

    /// Answers ioctls the way a UVC driver would, including duplicate entries.
    struct FakeDevice {
        device_caps: u32,
        formats: Vec<FakeFormat>,
        controls: Vec<FakeControl>,
        values: RefCell<HashMap<u32, i32>>,
    }

    fn einval() -> io::Error {
        io::Error::from_raw_os_error(EINVAL)
    }

    fn put_str(buf: &mut [u8], offset: usize, s: &str) {
        buf[offset..offset + s.len()].copy_from_slice(s.as_bytes());
    }

    impl FakeDevice {
        fn format(&self, code: u32) -> io::Result<&FakeFormat> {
            self.formats.iter().find(|f| fourcc_code(f.fourcc) == Some(code)).ok_or_else(einval)
        }
    }

    impl Ioctl for FakeDevice {
        fn ioctl(&self, request: u64, buf: &mut [u8]) -> io::Result<()> {
            assert_eq!(((request >> 16) & 0x3fff) as usize, buf.len());
            match request {
                VIDIOC_QUERYCAP => {
                    put_str(buf, 0, "uvcvideo");
                    put_str(buf, 16, "USB Camera: USB Camera");
                    put_str(buf, 48, "usb-0000:00:14.0-1");
                    put_u32(buf, 84, CAP_DEVICE_CAPS | CAP_VIDEO_CAPTURE | 0x0080_0000);
                    put_u32(buf, 88, self.device_caps);
                }
                VIDIOC_ENUM_FMT => {
                    assert_eq!(u32_at(buf, 4), BUF_TYPE_VIDEO_CAPTURE);
                    let f = self.formats.get(u32_at(buf, 0) as usize).ok_or_else(einval)?;
                    put_u32(buf, 8, if f.compressed { FMT_FLAG_COMPRESSED } else { 0 });
                    put_str(buf, 12, f.description);
                    put_u32(buf, 44, fourcc_code(f.fourcc).unwrap());
                }
                VIDIOC_ENUM_FRAMESIZES => {
                    let f = self.format(u32_at(buf, 4))?;
                    let (w, h, _) = f.sizes.get(u32_at(buf, 0) as usize).ok_or_else(einval)?;
                    put_u32(buf, 8, FRMSIZE_TYPE_DISCRETE);
                    put_u32(buf, 12, *w);
                    put_u32(buf, 16, *h);
                }
                VIDIOC_ENUM_FRAMEINTERVALS => {
                    let f = self.format(u32_at(buf, 4))?;
                    let (w, h) = (u32_at(buf, 8), u32_at(buf, 12));
                    let (_, _, intervals) = f.sizes.iter().find(|s| s.0 == w && s.1 == h).ok_or_else(einval)?;
                    let (num, den) = intervals.get(u32_at(buf, 0) as usize).ok_or_else(einval)?;
                    put_u32(buf, 16, FRMIVAL_TYPE_DISCRETE);
                    put_u32(buf, 20, *num);
                    put_u32(buf, 24, *den);
                }
                VIDIOC_QUERYCTRL => {
                    let wanted = u32_at(buf, 0);
                    let c = match wanted & CTRL_FLAG_NEXT_CTRL {
                        0 => self.controls.iter().find(|c| c.id == wanted),
                        _ => self.controls.iter().find(|c| c.id > wanted & !CTRL_FLAG_NEXT_CTRL),
                    }
                    .ok_or_else(einval)?;
                    put_u32(buf, 0, c.id);
                    put_u32(buf, 4, c.kind);
                    put_str(buf, 8, c.name);
                    put_i32(buf, 40, c.range.0);
                    put_i32(buf, 44, c.range.1);
                    put_i32(buf, 48, c.range.2);
                    put_i32(buf, 52, c.range.3);
                    put_u32(buf, 56, c.flags);
                }
                VIDIOC_QUERYMENU => {
                    let c = self.controls.iter().find(|c| c.id == u32_at(buf, 0)).ok_or_else(einval)?;
                    let (_, label) = c.menu.iter().find(|m| m.0 == u32_at(buf, 4)).ok_or_else(einval)?;
                    put_str(buf, 8, label);
                }
                VIDIOC_G_CTRL => {
                    let c = self.controls.iter().find(|c| c.id == u32_at(buf, 0)).ok_or_else(einval)?;
                    let value = self.values.borrow().get(&c.id).copied().unwrap_or(c.range.3);
                    put_i32(buf, 4, value);
                }
                VIDIOC_S_CTRL => {
                    let id = u32_at(buf, 0);
                    self.controls.iter().find(|c| c.id == id).ok_or_else(einval)?;
                    self.values.borrow_mut().insert(id, i32_at(buf, 4));
                }
                _ => return Err(io::Error::from_raw_os_error(25)),
            }
            Ok(())
        }
    }

    fn uvc_camera() -> FakeDevice {
        let fast = vec![(1, 30), (1, 25), (1, 30)];
        FakeDevice {
            device_caps: CAP_VIDEO_CAPTURE | 0x0420_0000,
            formats: vec![
                FakeFormat {
                    fourcc: "MJPG",
                    description: "Motion-JPEG",
                    compressed: true,
                    sizes: vec![(1280, 720, fast.clone()), (640, 480, fast.clone()), (1280, 720, fast.clone())],
                },
                FakeFormat {
                    fourcc: "YUYV",
                    description: "YUYV 4:2:2",
                    compressed: false,
                    sizes: vec![(1920, 1080, vec![(1, 5)])],
                },
                FakeFormat {
                    fourcc: "ZZZZ",
                    description: "Unknown to ffmpeg",
                    compressed: false,
                    sizes: vec![(640, 480, fast)],
                },
            ],
            controls: vec![
                FakeControl {
                    id: 0x0098_0913,
                    name: "Gain",
                    kind: CTRL_TYPE_INTEGER,
                    range: (0, 100, 1, 0),
                    flags: 0,
                    menu: vec![],
                },
                FakeControl {
                    id: 0x009a_0001,
                    name: "Camera Controls",
                    kind: 6,
                    range: (0, 0, 0, 0),
                    flags: 0x0100,
                    menu: vec![],
                },
                FakeControl {
                    id: 0x009a_0901,
                    name: "Auto Exposure",
                    kind: CTRL_TYPE_MENU,
                    range: (0, 3, 1, 3),
                    flags: 0,
                    menu: vec![(1, "Manual Mode"), (3, "Aperture Priority Mode")],
                },
                FakeControl {
                    id: 0x009a_0902,
                    name: "Exposure Time, Absolute",
                    kind: CTRL_TYPE_INTEGER,
                    range: (1, 5000, 1, 156),
                    flags: CTRL_FLAG_INACTIVE,
                    menu: vec![],
                },
                FakeControl {
                    id: 0x009a_0903,
                    name: "Hidden",
                    kind: CTRL_TYPE_BOOLEAN,
                    range: (0, 1, 1, 0),
                    flags: CTRL_FLAG_DISABLED,
                    menu: vec![],
                },
            ],
            values: RefCell::new(HashMap::new()),
        }
    }

    #[test]
    fn request_numbers_match_videodev2_h() {
        assert_eq!(VIDIOC_QUERYCAP, 0x8068_5600);
        assert_eq!(VIDIOC_ENUM_FMT, 0xc040_5602);
        assert_eq!(VIDIOC_G_CTRL, 0xc008_561b);
        assert_eq!(VIDIOC_S_CTRL, 0xc008_561c);
        assert_eq!(VIDIOC_QUERYCTRL, 0xc044_5624);
        assert_eq!(VIDIOC_QUERYMENU, 0xc02c_5625);
        assert_eq!(VIDIOC_ENUM_FRAMESIZES, 0xc02c_564a);
        assert_eq!(VIDIOC_ENUM_FRAMEINTERVALS, 0xc034_564b);
    }

    #[test]
    fn fourcc_round_trip() {
        assert_eq!(fourcc_code("MJPG"), Some(0x4750_4a4d));
        assert_eq!(fourcc_to_string(0x4750_4a4d), "MJPG");
        assert_eq!(fourcc_to_string(fourcc_code("Y16").unwrap()), "Y16");
        assert_eq!(fourcc_code(""), None);
        assert_eq!(fourcc_code("TOOLONG"), None);
    }

    #[test]
    fn capture_nodes_are_told_from_metadata_nodes() {
        let caps = |capabilities, device_caps| Capability {
            driver: String::new(),
            card: String::new(),
            bus_info: String::new(),
            capabilities,
            device_caps,
        };
        assert!(caps(0x84a0_0001, 0x0420_0001).is_video_capture());
        assert!(!caps(0x84a0_0001, 0x04a0_0000).is_video_capture());
        assert!(caps(0x0000_0001, 0).is_video_capture());
    }

    #[test]
    fn describes_a_uvc_camera() {
        let dev = uvc_camera();
        let described = describe(&dev, "/dev/video0").unwrap();
        assert_eq!(described.name, "USB Camera: USB Camera");
        assert_eq!(described.error, None);
        let summary: Vec<_> = described
            .modes
            .iter()
            .map(|m| (m.format.clone().unwrap(), m.width, m.height, m.rates.len(), m.transcoded))
            .collect();
        assert_eq!(
            summary,
            [
                ("MJPG".to_string(), 1280, 720, 2, false),
                ("MJPG".to_string(), 640, 480, 2, false),
                ("YUYV".to_string(), 1920, 1080, 1, true),
            ],
            "duplicates removed, unknown formats skipped"
        );
        assert_eq!(described.modes[0].rates, [FrameRate::new(30, 1).unwrap(), FrameRate::new(25, 1).unwrap()]);
    }

    #[test]
    fn metadata_nodes_are_skipped() {
        let dev = FakeDevice { device_caps: 0x04a0_0000, ..uvc_camera() };
        assert_eq!(describe(&dev, "/dev/video1"), None);
    }

    #[test]
    fn lists_controls_with_menus_and_flags() {
        let dev = uvc_camera();
        let list = controls(&dev).unwrap();
        let names: Vec<_> = list.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["Gain", "Auto Exposure", "Exposure Time, Absolute"]);
        let auto = &list[1];
        assert_eq!(auto.kind, ControlKind::Menu);
        assert_eq!(auto.value, Some(3));
        assert_eq!(
            auto.menu,
            [
                MenuItem { index: 1, label: "Manual Mode".into() },
                MenuItem { index: 3, label: "Aperture Priority Mode".into() }
            ]
        );
        assert!(list[2].inactive());
        assert!(!list[2].read_only());
    }

    #[test]
    fn sets_and_reads_back_controls() {
        let dev = uvc_camera();
        set_control(&dev, 0x0098_0913, 42).unwrap();
        assert_eq!(get_control(&dev, 0x0098_0913).unwrap(), 42);
        assert_eq!(controls(&dev).unwrap()[0].value, Some(42));
        assert!(set_control(&dev, 0x1234, 1).is_err());
    }

    #[test]
    fn apply_control_validates_then_refreshes() {
        let dev = uvc_camera();
        let after = apply_control(&dev, 0x009a_0901, 1).unwrap();
        assert_eq!(after[1].value, Some(1));
        assert!(apply_control(&dev, 0x009a_0901, 2).is_err(), "not a menu entry");
        assert!(apply_control(&dev, 0xdead, 0).is_err());
    }

    #[test]
    fn checks_values_before_sending() {
        let list = controls(&uvc_camera()).unwrap();
        assert!(list[0].check(100).is_ok());
        assert!(list[0].check(101).is_err());
        assert!(list[1].check(1).is_ok());
        assert!(list[1].check(2).is_err(), "gap in the menu");
        let read_only = Control { flags: CTRL_FLAG_READ_ONLY, ..list[0].clone() };
        assert!(read_only.check(1).is_err());
    }

    #[test]
    fn video_node_paths_sorted_numerically() {
        assert_eq!(
            video_nodes(["video10", "null", "video2", "video0", "videoX", "v4l"]),
            ["/dev/video0", "/dev/video2", "/dev/video10"]
        );
    }

    #[test]
    fn permission_errors_are_actionable() {
        let e = io::Error::from(io::ErrorKind::PermissionDenied);
        assert!(describe_error("/dev/video0", &e).contains("video` group"));
    }
}
