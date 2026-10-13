use crate::audio::PcmFrame;
use crate::models::AsrStatus;
use std::path::PathBuf;
use std::sync::mpsc::{self, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};

/// Local speech engine. A Whisper (or other) backend implements this and is
/// installed on the worker; the default engine only keeps the audio tap alive.
pub trait SpeechEngine: Send {
    fn push(&mut self, pcm_48k_stereo: &[f32]) -> Option<String>;
}

struct IdleEngine;

impl SpeechEngine for IdleEngine {
    fn push(&mut self, _pcm: &[f32]) -> Option<String> {
        None
    }
}

enum AsrCmd {
    Configure { enabled: bool, model_path: String },
}

#[derive(Clone)]
pub struct AsrHandle {
    cmd: mpsc::Sender<AsrCmd>,
    audio: SyncSender<PcmFrame>,
    _thread: std::sync::Arc<JoinSlot>,
}

struct JoinSlot {
    _thread: Option<JoinHandle<()>>,
}

impl AsrHandle {
    pub fn start(app: AppHandle) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (audio_tx, audio_rx) = mpsc::sync_channel(8);
        let thread = thread::Builder::new()
            .name("suncord-asr".into())
            .spawn(move || worker(app, cmd_rx, audio_rx))
            .ok();
        Self {
            cmd: cmd_tx,
            audio: audio_tx,
            _thread: std::sync::Arc::new(JoinSlot { _thread: thread }),
        }
    }

    pub fn audio_sender(&self) -> SyncSender<PcmFrame> {
        self.audio.clone()
    }

    pub fn configure(&self, enabled: bool, model_path: String) {
        let _ = self.cmd.send(AsrCmd::Configure {
            enabled,
            model_path,
        });
    }
}

fn worker(
    app: AppHandle,
    commands: mpsc::Receiver<AsrCmd>,
    audio: mpsc::Receiver<PcmFrame>,
) {
    let mut enabled = false;
    let mut model_path = String::new();
    let mut engine: Box<dyn SpeechEngine> = Box::new(IdleEngine);
    let mut caption = String::new();
    let mut level = 0.0f32;
    let mut last_emit = Instant::now() - Duration::from_secs(1);
    loop {
        while let Ok(command) = commands.try_recv() {
            match command {
                AsrCmd::Configure {
                    enabled: next,
                    model_path: path,
                } => {
                    enabled = next;
                    model_path = path;
                    engine = Box::new(IdleEngine);
                    caption.clear();
                }
            }
        }
        match audio.recv_timeout(Duration::from_millis(200)) {
            Ok(frame) => {
                level = frame.rms;
                if enabled {
                    if let Some(text) = engine.push(&frame.samples) {
                        caption = text;
                        emit(&app, enabled, &model_path, &caption, level);
                        last_emit = Instant::now();
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
        if last_emit.elapsed() >= Duration::from_millis(250) {
            emit(&app, enabled, &model_path, &caption, level);
            last_emit = Instant::now();
        }
    }
}

fn emit(app: &AppHandle, enabled: bool, model_path: &str, caption: &str, level: f32) {
    let detail = if !enabled {
        "Captions are off.".into()
    } else if model_path.is_empty() {
        "Listening. Choose a local model to transcribe.".into()
    } else if PathBuf::from(model_path).exists() {
        "Listening. The model is armed; the SpeechEngine slot is idle.".into()
    } else {
        "Listening. That model path does not exist yet.".into()
    };
    let _ = app.emit(
        "suncord://asr",
        AsrStatus {
            enabled,
            detail,
            caption: caption.to_string(),
            level,
        },
    );
}
