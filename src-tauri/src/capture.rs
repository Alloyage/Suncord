use crate::models::{CaptureFrame, CaptureStats, SourceDto};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use image::{codecs::png::PngEncoder, ColorType, ImageEncoder};
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
    let mut fps = 60u32;
    let mut preference = "gpu".to_string();
    let mut preview_enabled = false;
    let mut dropped = 0u64;
    let mut frames = 0u32;
    let mut window_started = Instant::now();
    loop {
        match commands.recv_timeout(Duration::from_millis(if active.is_some() { 1 } else { 200 })) {
            Ok(CapCmd::Start {
                id,
                fps: next_fps,
                preference: next,
            }) => {
                if let Some(stream) = display_stream.take() {
                    stream.stop();
                }
                display_stream = NativeDisplayStream::start(&id).ok();
                active = Some(id);
                preview_enabled = false;
                fps = next_fps.clamp(15, 240);
                preference = next;
                dropped = 0;
                frames = 0;
                window_started = Instant::now();
            }
            Ok(CapCmd::Stop) => {
                active = None;
                preview_enabled = false;
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
        let interval = Duration::from_secs_f64(1.0 / fps.max(1) as f64);
        let started = Instant::now();
        let wait = Duration::from_millis((1000 / fps.max(1)).max(8) as u64);
        let frame = if let Some(stream) = display_stream.as_ref() {
            stream.next_frame(wait)
        } else {
            grab(&id)
        };
        match frame {
            Ok(frame) => {
                let (width, height, path) = encode(&preference, frame.width, frame.height, &frame.rgba);
                // A compressed copy is emitted only for the local GPU preview;
                // capture retains its native RGBA buffer.
                if preview_enabled {
                    emit_preview(&app, &frame);
                }
                frames += 1;
                if window_started.elapsed() >= Duration::from_secs(1) {
                    let fps = frames as f32 / window_started.elapsed().as_secs_f32();
                    frames = 0;
                    window_started = Instant::now();
                    emit(
                        &app,
                        CaptureStats {
                            running: true,
                            source: id,
                            width,
                            height,
                            fps,
                            capture_ms: started.elapsed().as_secs_f32() * 1000.0,
                            dropped,
                            path: path.into(),
                        },
                    );
                }
            }
            Err(_) => {
                dropped += 1;
            }
        }
        let elapsed = started.elapsed();
        if elapsed < interval {
            thread::sleep(interval - elapsed);
        } else {
            // Behind the target. Keep the requested fps and drop the wait.
            // Quality is not reduced when the machine falls behind.
            dropped += 1;
        }
    }
}

struct Frame {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

/// Display capture is driven by the D3D desktop-duplication stream instead of
/// polling GDI. It delivers fresh frames as Windows presents them and leaves
/// window capture on the compatible xcap fallback.
struct NativeDisplayStream {
    recorder: xcap::VideoRecorder,
    frames: std::sync::mpsc::Receiver<xcap::Frame>,
}

impl NativeDisplayStream {
    fn start(id: &str) -> Result<Self, String> {
        let name = id.strip_prefix("display:").ok_or("Not a display source.")?;
        let monitor = xcap::Monitor::all()
            .map_err(|err| err.to_string())?
            .into_iter()
            .find(|monitor| monitor.name().ok().as_deref() == Some(name))
            .ok_or("That display is no longer available.")?;
        let (recorder, frames) = monitor.video_recorder().map_err(|err| err.to_string())?;
        recorder.start().map_err(|err| err.to_string())?;
        Ok(Self { recorder, frames })
    }

    fn next_frame(&self, timeout: Duration) -> Result<Frame, String> {
        let frame = self.frames.recv_timeout(timeout).map_err(|err| err.to_string())?;
        Ok(Frame { width: frame.width, height: frame.height, rgba: frame.raw })
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
        let image = monitor.capture_image().map_err(|err| err.to_string())?;
        return Ok(Frame {
            width: image.width(),
            height: image.height(),
            rgba: image.into_raw(),
        });
    }
    if let Some(rest) = id.strip_prefix("window:") {
        let title = rest.split_once(':').map(|(_, title)| title).unwrap_or(rest);
        let windows = xcap::Window::all().map_err(|err| err.to_string())?;
        let window = windows
            .into_iter()
            .find(|window| window.title().ok().as_deref() == Some(title))
            .ok_or("That window is no longer available.")?;
        let image = window.capture_image().map_err(|err| err.to_string())?;
        return Ok(Frame {
            width: image.width(),
            height: image.height(),
            rgba: image.into_raw(),
        });
    }
    Err("Unknown capture source.".into())
}

/// GPU path keeps the native frame captured through Windows Graphics Capture.
/// CPU path scales on the CPU so the preference changes the work that is done.
fn encode(preference: &str, width: u32, height: u32, rgba: &[u8]) -> (u32, u32, &'static str) {
    if preference == "cpu" && width > 1280 {
        let target_w = 1280u32;
        let target_h = ((height as u64 * target_w as u64) / width.max(1) as u64).max(2) as u32;
        let _scaled = scale_rgba(rgba, width, height, target_w, target_h);
        (
            target_w,
            target_h,
            "Windows Graphics Capture · software scale",
        )
    } else {
        let _ = rgba.first();
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
    // Bound the copied preview so IPC cannot steal time from native capture.
    let (width, height, rgba) = if frame.width > 960 {
        let width = 960;
        let height = ((frame.height as u64 * width as u64) / frame.width.max(1) as u64).max(2) as u32;
        let rgba = scale_rgba(&frame.rgba, frame.width, frame.height, width, height);
        (width, height, rgba)
    } else {
        (frame.width, frame.height, frame.rgba.clone())
    };
    let mut png = Vec::new();
    if PngEncoder::new(&mut png)
        .write_image(&rgba, width, height, ColorType::Rgba8.into())
        .is_ok()
    {
        let _ = app.emit(
            "suncord://capture-frame",
            CaptureFrame {
                width,
                height,
                png_base64: BASE64.encode(png),
            },
        );
    }
}
