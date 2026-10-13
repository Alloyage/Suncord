use crate::models::{Settings, SettingsPatch};
use parking_lot::RwLock;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Clone)]
pub struct SettingsStore {
    inner: Arc<RwLock<Settings>>,
    path: PathBuf,
}

impl SettingsStore {
    pub fn load(path: PathBuf) -> Self {
        let settings = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Settings>(&bytes).ok())
            .unwrap_or_default()
            .clamp();
        Self {
            inner: Arc::new(RwLock::new(settings)),
            path,
        }
    }

    pub fn get(&self) -> Settings {
        self.inner.read().clone()
    }

    pub fn apply_patch(&self, patch: SettingsPatch) -> Settings {
        let mut settings = self.inner.write();
        if let Some(value) = patch.buffer_frames {
            settings.buffer_frames = value;
        }
        if let Some(value) = patch.hardware_accel {
            settings.hardware_accel = value;
        }
        if let Some(value) = patch.adaptive_throttle {
            settings.adaptive_throttle = value;
        }
        if let Some(value) = patch.capture_fps {
            settings.capture_fps = value;
        }
        if let Some(value) = patch.hw_preference {
            settings.hw_preference = value;
        }
        if let Some(value) = patch.asr_enabled {
            settings.asr_enabled = value;
        }
        if let Some(value) = patch.asr_model_path {
            settings.asr_model_path = value;
        }
        if let Some(value) = patch.input_device {
            settings.input_device = value;
        }
        if let Some(value) = patch.output_device {
            settings.output_device = value;
        }
        *settings = settings.clone().clamp();
        let next = settings.clone();
        drop(settings);
        self.persist(&next);
        next
    }

    fn persist(&self, settings: &Settings) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(bytes) = serde_json::to_vec(settings) {
            let _ = std::fs::write(&self.path, bytes);
        }
    }
}
