use crate::models::{CaptureFrame, CaptureStats, SourceDto};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use image::{codecs::jpeg::JpegEncoder, ColorType, ImageEncoder};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};

enum CapCmd {
    Start {
        id: String,
        fps: u32,
        preference: String,
    },
    Stop,
    Apply {
        fps: u32,
        preference: String,
    },
    Preview(bool),
}

#[derive(Clone)]
pub struct CaptureHandle {
    cmd: std::sync::mpsc::Sender<CapCmd>,
    _thread: std::sync::Arc<Slot>,
}

struct Slot {
    _thread: Option<JoinHandle<()>>,
}

impl CaptureHandle {
    pub fn start(app: AppHandle) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let thread = thread::Builder::new()
            .name("suncord-capture".into())
            .spawn(move || loop_capture(app, rx))
            .ok();
        Self {
            cmd: tx,
            _thread: std::sync::Arc::new(Slot { _thread: thread }),
        }
    }

    pub fn start_source(&self, id: String, fps: u32, preference: String) {
        let _ = self.cmd.send(CapCmd::Start {
            id,
            fps,
            preference,
        });
    }

    pub fn stop(&self) {
        let _ = self.cmd.send(CapCmd::Stop);
    }

    pub fn apply(&self, fps: u32, preference: String) {
        let _ = self.cmd.send(CapCmd::Apply { fps, preference });
    }

    pub fn set_preview(&self, enabled: bool) {
        let _ = self.cmd.send(CapCmd::Preview(enabled));
    }
}

pub fn list_sources() -> Vec<SourceDto> {
    let mut sources = Vec::new();
    let monitors = xcap::Monitor::all().unwrap_or_default();
    for monitor in monitors {
        let name = monitor.name().unwrap_or_else(|_| "Display".into());
        sources.push(SourceDto {
            id: format!("display:{name}"),
            kind: "display".into(),
            width: monitor.width().unwrap_or(0),
            height: monitor.height().unwrap_or(0),
            name,
        });
    }
    let windows = xcap::Window::all().unwrap_or_default();
    for (index, window) in windows.into_iter().enumerate() {
        if window.is_minimized().unwrap_or(true) {
            continue;
        }
        let title = window.title().unwrap_or_default();
        if title.trim().is_empty() || title == "Suncord" || title.starts_with("Sign in") {
            continue;
        }
        let width = window.width().unwrap_or(0);
        let height = window.height().unwrap_or(0);
        if width < 32 || height < 32 {
            continue;
        }
        sources.push(SourceDto {
            id: format!("window:{index}:{title}"),
            kind: "window".into(),
            name: title,
            width,
            height,
        });
        if sources.len() > 48 {
            break;
        }
    }
    sources
}

fn loop_capture(app: AppHandle, commands: std::sync::mpsc::Receiver<CapCmd>) {
    let mut active: Option<String> = None;
    let mut display_stream: Option<NativeDisplayStream> = None;
    let mut last_frame: Option<Frame> = None;
    let mut fps = 60u32;
    let mut preference = "gpu".to_string();
    let mut preview_enabled = false;
    let mut dropped = 0u64;
    let mut frames = 0u32;
    let mut bytes = 0u64;
    let mut window_started = Instant::now();
    let mut next_emit = Instant::now();
    // Preview JPEG encode runs off-thread; capture only ships Arc frames (no 4K clone).
    let (preview_tx, preview_rx) = std::sync::mpsc::sync_channel::<std::sync::Arc<Frame>>(2);
    let preview_app = app.clone();
    thread::Builder::new()
        .name("suncord-preview".into())
        .spawn(move || {
            while let Ok(frame) = preview_rx.recv() {
                let mut latest = frame;
                while let Ok(newer) = preview_rx.try_recv() {
                    latest = newer;
                }
                emit_preview(&preview_app, &latest);
            }
        })
        .ok();
    loop {
        let idle = active.is_none();
        match commands.recv_timeout(Duration::from_millis(if idle { 200 } else { 0 })) {
            Ok(CapCmd::Start {
                id,
                fps: next_fps,
                preference: next,
            }) => {
                if let Some(stream) = display_stream.take() {
                    stream.stop();
                }
                display_stream = NativeDisplayStream::start(&id).ok();
                last_frame = None;
                active = Some(id);
                preview_enabled = false;
                fps = next_fps.clamp(15, 240);
                preference = next;
                dropped = 0;
                frames = 0;
                bytes = 0;
                window_started = Instant::now();
                next_emit = Instant::now();
            }
            Ok(CapCmd::Stop) => {
                active = None;
                preview_enabled = false;
                last_frame = None;
                if let Some(stream) = display_stream.take() {
                    stream.stop();
                }
                emit(
                    &app,
                    CaptureStats {
                        running: false,
                        ..CaptureStats::default()
                    },
                );
            }
            Ok(CapCmd::Apply {
                fps: next_fps,
                preference: next,
            }) => {
                fps = next_fps.clamp(15, 240);
                preference = next;
            }
            Ok(CapCmd::Preview(enabled)) => preview_enabled = enabled,
            Err(_) => {}
        }
        let Some(id) = active.clone() else {
            continue;
        };
        let target_fps = fps.max(1);
        let interval = Duration::from_secs_f64(1.0 / target_fps as f64);
        let tick = Instant::now();
        let wait = if next_emit > tick {
            next_emit - tick
        } else {
            Duration::ZERO
        };

        // Block briefly for the next WGC frame so we ride the compositor clock (~60 Hz)
        // instead of spinning on try_recv and only counting sparse desktop updates.
        let mut fresh = false;
        if let Some(stream) = display_stream.as_ref() {
            if let Some(frame) = stream.wait_latest(wait.max(Duration::from_millis(1))) {
                last_frame = Some(frame);
                fresh = true;
            }
        } else if last_frame.is_none() || tick >= next_emit {
            match grab(&id) {
                Ok(frame) => {
                    last_frame = Some(frame);
                    fresh = true;
                }
                Err(_) => dropped = dropped.saturating_add(1),
            }
        }

        // Pace emits to the target FPS. Re-ship the last buffer when WGC is quiet so
        // the software cursor stays live without GDI fallback (which tops out ~5 FPS).
        if !fresh && tick < next_emit {
            continue;
        }
        if !fresh && last_frame.is_none() {
            thread::sleep(Duration::from_millis(2));
            continue;
        }

        next_emit = Instant::now() + interval;
        let Some(frame) = last_frame.clone() else {
            continue;
        };
        let (width, height, path) = encode(&preference, frame.width, frame.height);
        bytes = bytes.saturating_add(frame.rgba.len() as u64);
        if preview_enabled {
            if preview_tx.try_send(std::sync::Arc::new(frame)).is_err() {
                dropped = dropped.saturating_add(1);
            }
        }
        frames += 1;
        if window_started.elapsed() >= Duration::from_secs(1) {
            let elapsed = window_started.elapsed().as_secs_f32().max(0.001);
            let measured_fps = frames as f32 / elapsed;
            let bitrate_kbps = (bytes as f32 * 8.0 / 1000.0) / elapsed;
            frames = 0;
            bytes = 0;
            window_started = Instant::now();
            emit(
                &app,
                CaptureStats {
                    running: true,
                    source: id,
                    width,
                    height,
                    fps: measured_fps,
                    capture_ms: tick.elapsed().as_secs_f32() * 1000.0,
                    dropped,
                    path: path.into(),
                    bitrate_kbps,
                },
            );
        }
    }
}

#[derive(Clone)]
struct Frame {
    width: u32,
    height: u32,
    rgba: std::sync::Arc<Vec<u8>>,
    origin_x: i32,
    origin_y: i32,
}

/// Display capture is driven by the D3D desktop-duplication stream instead of
/// polling GDI. It delivers fresh frames as Windows presents them and leaves
/// window capture on the compatible xcap fallback.
struct NativeDisplayStream {
    recorder: xcap::VideoRecorder,
    frames: std::sync::mpsc::Receiver<xcap::Frame>,
    origin_x: i32,
    origin_y: i32,
}

impl NativeDisplayStream {
    fn start(id: &str) -> Result<Self, String> {
        let name = id.strip_prefix("display:").ok_or("Not a display source.")?;
        let monitor = xcap::Monitor::all()
            .map_err(|err| err.to_string())?
            .into_iter()
            .find(|monitor| monitor.name().ok().as_deref() == Some(name))
            .ok_or("That display is no longer available.")?;
        let origin_x = monitor.x().unwrap_or(0);
        let origin_y = monitor.y().unwrap_or(0);
        let (recorder, frames) = monitor.video_recorder().map_err(|err| err.to_string())?;
        recorder.start().map_err(|err| err.to_string())?;
        Ok(Self {
            recorder,
            frames,
            origin_x,
            origin_y,
        })
    }

    fn wait_latest(&self, timeout: Duration) -> Option<Frame> {
        let mut frame = match self.frames.recv_timeout(timeout) {
            Ok(frame) => frame,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => self.frames.try_recv().ok()?,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return None,
        };
        while let Ok(newer) = self.frames.try_recv() {
            frame = newer;
        }
        Some(Frame {
            width: frame.width,
            height: frame.height,
            rgba: std::sync::Arc::new(frame.raw),
            origin_x: self.origin_x,
            origin_y: self.origin_y,
        })
    }

    fn stop(self) {
        let _ = self.recorder.stop();
    }
}

fn grab(id: &str) -> Result<Frame, String> {
    if let Some(name) = id.strip_prefix("display:") {
        let monitors = xcap::Monitor::all().map_err(|err| err.to_string())?;
        let monitor = monitors
            .into_iter()
            .find(|monitor| monitor.name().ok().as_deref() == Some(name))
            .ok_or("That display is no longer available.")?;
        let origin_x = monitor.x().unwrap_or(0);
        let origin_y = monitor.y().unwrap_or(0);
        let image = monitor.capture_image().map_err(|err| err.to_string())?;
        return Ok(Frame {
            width: image.width(),
            height: image.height(),
            rgba: std::sync::Arc::new(image.into_raw()),
            origin_x,
            origin_y,
        });
    }
    if let Some(rest) = id.strip_prefix("window:") {
        let title = rest.split_once(':').map(|(_, title)| title).unwrap_or(rest);
        let windows = xcap::Window::all().map_err(|err| err.to_string())?;
        let window = windows
            .into_iter()
            .find(|window| window.title().ok().as_deref() == Some(title))
            .ok_or("That window is no longer available.")?;
        let origin_x = window.x().unwrap_or(0);
        let origin_y = window.y().unwrap_or(0);
        let image = window.capture_image().map_err(|err| err.to_string())?;
        return Ok(Frame {
            width: image.width(),
            height: image.height(),
            rgba: std::sync::Arc::new(image.into_raw()),
            origin_x,
            origin_y,
        });
    }
    Err("Unknown capture source.".into())
}

#[cfg(windows)]
#[repr(C)]
struct WinPoint {
    x: i32,
    y: i32,
}

#[cfg(windows)]
#[repr(C)]
struct WinCursorInfo {
    cb_size: u32,
    flags: u32,
    cursor: isize,
    pt_screen_pos: WinPoint,
}

#[cfg(windows)]
#[link(name = "user32")]
extern "system" {
    fn GetCursorPos(point: *mut WinPoint) -> i32;
    fn GetCursorInfo(info: *mut WinCursorInfo) -> i32;
}

#[cfg(windows)]
fn paint_software_cursor(frame: &mut Frame, origin_x: i32, origin_y: i32, scale_x: f32, scale_y: f32) {
    const CURSOR_SHOWING: u32 = 0x0000_0001;
    let mut info = WinCursorInfo {
        cb_size: std::mem::size_of::<WinCursorInfo>() as u32,
        flags: 0,
        cursor: 0,
        pt_screen_pos: WinPoint { x: 0, y: 0 },
    };
    let visible = unsafe { GetCursorInfo(&mut info) } != 0 && (info.flags & CURSOR_SHOWING) != 0;
    if !visible {
        return;
    }
    let mut point = WinPoint { x: 0, y: 0 };
    if unsafe { GetCursorPos(&mut point) } == 0 {
        point = info.pt_screen_pos;
    }
    let local_x = point.x.saturating_sub(origin_x);
    let local_y = point.y.saturating_sub(origin_y);
    if local_x < 0 || local_y < 0 {
        return;
    }
    let x = (local_x as f32 * scale_x).round() as u32;
    let y = (local_y as f32 * scale_y).round() as u32;
    if x >= frame.width || y >= frame.height {
        return;
    }
    // Compact arrow so the pointer stays readable at preview scale.
    const ARROW: &[(i32, i32)] = &[
        (0, 0), (0, 1), (0, 2), (0, 3), (0, 4), (0, 5), (0, 6), (0, 7), (0, 8), (0, 9), (0, 10),
        (0, 11), (1, 1), (1, 2), (1, 3), (1, 4), (1, 5), (1, 6), (1, 7), (1, 8), (1, 9), (1, 10),
        (2, 2), (2, 3), (2, 4), (2, 5), (2, 6), (2, 7), (2, 8), (2, 9), (3, 3), (3, 4), (3, 5),
        (3, 6), (3, 7), (3, 8), (4, 4), (4, 5), (4, 6), (4, 7), (4, 10), (5, 5), (5, 6), (5, 9),
        (5, 11), (6, 6), (6, 8), (6, 12), (7, 7), (7, 13), (8, 8), (8, 12), (9, 9), (9, 11),
        (10, 10),
    ];
    let pixels = std::sync::Arc::make_mut(&mut frame.rgba);
    let mut put = |px: i32, py: i32, r: u8, g: u8, b: u8| {
        if px < 0 || py < 0 {
            return;
        }
        let px = px as u32;
        let py = py as u32;
        if px >= frame.width || py >= frame.height {
            return;
        }
        let idx = ((py as usize * frame.width as usize) + px as usize) * 4;
        if idx + 3 >= pixels.len() {
            return;
        }
        pixels[idx] = r;
        pixels[idx + 1] = g;
        pixels[idx + 2] = b;
        pixels[idx + 3] = 255;
    };
    for &(dx, dy) in ARROW {
        let px = x as i32 + dx;
        let py = y as i32 + dy;
        for (ox, oy) in [(-1, 0), (1, 0), (0, -1), (0, 1), (-1, -1), (1, 1)] {
            put(px + ox, py + oy, 16, 16, 16);
        }
    }
    for &(dx, dy) in ARROW {
        put(x as i32 + dx, y as i32 + dy, 255, 255, 255);
    }
}

#[cfg(not(windows))]
fn paint_software_cursor(_frame: &mut Frame, _origin_x: i32, _origin_y: i32, _scale_x: f32, _scale_y: f32) {}

/// Stats-only path — never touch the pixel buffer on the capture hot loop.
fn encode(preference: &str, width: u32, height: u32) -> (u32, u32, &'static str) {
    if preference == "cpu" && width > 1280 {
        let target_w = 1280u32;
        let target_h = ((height as u64 * target_w as u64) / width.max(1) as u64).max(2) as u32;
        (
            target_w,
            target_h,
            "Windows Graphics Capture · software scale",
        )
    } else {
        (
            width,
            height,
            "Windows Graphics Capture · native",
        )
    }
}

fn scale_rgba(src: &[u8], width: u32, height: u32, target_w: u32, target_h: u32) -> Vec<u8> {
    let mut out = vec![0u8; (target_w as usize) * (target_h as usize) * 4];
    for y in 0..target_h {
        let src_y = (y as u64 * height as u64 / target_h.max(1) as u64).min(height.saturating_sub(1) as u64) as u32;
        for x in 0..target_w {
            let src_x = (x as u64 * width as u64 / target_w.max(1) as u64).min(width.saturating_sub(1) as u64) as u32;
            let from = ((src_y as usize * width as usize) + src_x as usize) * 4;
            let to = ((y as usize * target_w as usize) + x as usize) * 4;
            if from + 3 < src.len() && to + 3 < out.len() {
                out[to..to + 4].copy_from_slice(&src[from..from + 4]);
            }
        }
    }
    out
}

fn emit(app: &AppHandle, stats: CaptureStats) {
    let _ = app.emit("suncord://capture", stats);
}

fn emit_preview(app: &AppHandle, frame: &Frame) {
    // Compact JPEG keeps the off-thread encoder under a frame budget at 60 FPS.
    let max_w = 720u32;
    let (width, height, rgba) = if frame.width > max_w {
        let width = max_w;
        let height = ((frame.height as u64 * width as u64) / frame.width.max(1) as u64).max(2) as u32;
        (
            width,
            height,
            scale_rgba(frame.rgba.as_ref(), frame.width, frame.height, width, height),
        )
    } else {
        (frame.width, frame.height, frame.rgba.as_ref().clone())
    };
    let origin_x = frame.origin_x;
    let origin_y = frame.origin_y;
    let mut painted = Frame {
        width,
        height,
        rgba: std::sync::Arc::new(rgba),
        origin_x,
        origin_y,
    };
    let scale_x = width as f32 / frame.width.max(1) as f32;
    let scale_y = height as f32 / frame.height.max(1) as f32;
    paint_software_cursor(&mut painted, origin_x, origin_y, scale_x, scale_y);
    let rgb = rgba_to_rgb(painted.rgba.as_ref());
    let mut jpeg = Vec::with_capacity((width * height) as usize / 6);
    let encoder = JpegEncoder::new_with_quality(&mut jpeg, 55);
    if encoder
        .write_image(&rgb, width, height, ColorType::Rgb8.into())
        .is_ok()
    {
        let _ = app.emit(
            "suncord://capture-frame",
            CaptureFrame {
                width,
                height,
                png_base64: BASE64.encode(jpeg),
            },
        );
    }
}

fn rgba_to_rgb(rgba: &[u8]) -> Vec<u8> {
    let pixels = rgba.len() / 4;
    let mut rgb = Vec::with_capacity(pixels * 3);
    for chunk in rgba.chunks_exact(4) {
        rgb.push(chunk[0]);
        rgb.push(chunk[1]);
        rgb.push(chunk[2]);
    }
    rgb
}
