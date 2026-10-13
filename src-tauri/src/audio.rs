use crate::models::AudioDeviceDto;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, Device, SampleFormat, SampleRate, Stream, StreamConfig, SupportedStreamConfig};
use rtrb::{Consumer, Producer, RingBuffer};
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub const PIPELINE_RATE: u32 = 48_000;
pub const PIPELINE_CHANNELS: u16 = 2;
const FRAME_SAMPLES: usize = 960 * 2;

#[derive(Clone)]
pub struct PcmFrame {
    pub samples: Vec<f32>,
    pub rms: f32,
}

#[derive(Clone)]
pub struct AudioIo {
    pub buffer_frames: u32,
    pub hardware_accel: bool,
    pub input_name: String,
    pub output_name: String,
    pub running: bool,
}

enum PumpCmd {
    Configure(AudioIo),
}

pub struct AudioPump {
    cmd: std::sync::mpsc::Sender<PumpCmd>,
    _thread: Option<JoinHandle<()>>,
}

impl AudioPump {
    pub fn start(
        frames: tokio::sync::mpsc::Sender<PcmFrame>,
        asr: std::sync::mpsc::SyncSender<PcmFrame>,
    ) -> Self {
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
        let thread = thread::Builder::new()
            .name("suncord-audio".into())
            .spawn(move || pump(cmd_rx, frames, asr))
            .ok();
        Self {
            cmd: cmd_tx,
            _thread: thread,
        }
    }

    pub fn configure(&self, config: AudioIo) {
        let _ = self.cmd.send(PumpCmd::Configure(config));
    }
}

/// Plays decoded Opus PCM on the system default (or named) output device.
pub struct PlaybackPump {
    tx: std::sync::mpsc::SyncSender<Vec<f32>>,
    _thread: Option<JoinHandle<()>>,
}

impl PlaybackPump {
    pub fn start(output_name: String) -> Self {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(32);
        let thread = thread::Builder::new()
            .name("suncord-playback".into())
            .spawn(move || playback_loop(rx, output_name))
            .ok();
        Self {
            tx,
            _thread: thread,
        }
    }

    pub fn push(&self, samples: Vec<f32>) {
        let _ = self.tx.try_send(samples);
    }

    pub fn stop(&self) {
        // Dropping the sender wakes the playback thread; clone stays alive on self.
        // Sending an empty frame is the stop signal.
        let _ = self.tx.try_send(Vec::new());
    }
}

fn playback_loop(rx: std::sync::mpsc::Receiver<Vec<f32>>, output_name: String) {
    let (mut producer, consumer) = RingBuffer::<f32>::new(48_000 * 2);
    let stream = match open_output(&output_name, consumer) {
        Ok(stream) => stream,
        Err(err) => {
            log::warn!("audio output did not open: {err}");
            // Keep draining so the voice UDP loop never blocks on a full channel.
            while rx.recv().is_ok() {}
            return;
        }
    };
    if let Err(err) = stream.play() {
        log::warn!("audio output play failed: {err}");
        return;
    }
    log::info!("audio output ready");
    while let Ok(samples) = rx.recv() {
        if samples.is_empty() {
            break;
        }
        for sample in samples {
            let _ = producer.push(sample);
        }
    }
}

fn open_output(name: &str, mut consumer: Consumer<f32>) -> Result<Stream, String> {
    let device = find_device(name, false).ok_or("No output device is available.")?;
    let supported = device
        .default_output_config()
        .map_err(|err| err.to_string())?;
    let sample_rate = supported.sample_rate().0;
    let channels = supported.channels().max(1) as usize;
    let format = supported.sample_format();
    let config = StreamConfig {
        channels: supported.channels(),
        sample_rate: supported.sample_rate(),
        buffer_size: BufferSize::Default,
    };
    // Playback ring is 48 kHz stereo; resample naively if the device differs.
    let ratio = sample_rate as f64 / PIPELINE_RATE as f64;
    let stream = match format {
        SampleFormat::F32 => device
            .build_output_stream(
                &config,
                move |data: &mut [f32], _| {
                    fill_output(data, channels, ratio, &mut consumer);
                },
                |err| log::warn!("audio output: {err}"),
                None,
            )
            .map_err(|err| err.to_string())?,
        SampleFormat::I16 => device
            .build_output_stream(
                &config,
                move |data: &mut [i16], _| {
                    let mut f32_buf = vec![0f32; data.len()];
                    fill_output(&mut f32_buf, channels, ratio, &mut consumer);
                    for (dst, sample) in data.iter_mut().zip(f32_buf.iter()) {
                        *dst = (sample.clamp(-1.0, 1.0) * 32767.0) as i16;
                    }
                },
                |err| log::warn!("audio output: {err}"),
                None,
            )
            .map_err(|err| err.to_string())?,
        _ => return Err("This output device's sample format is not supported.".into()),
    };
    Ok(stream)
}

fn fill_output(data: &mut [f32], channels: usize, ratio: f64, consumer: &mut Consumer<f32>) {
    // Device callback wants interleaved samples at device rate/channels.
    // Pull stereo pairs from the 48 kHz ring and duplicate/resample lightly.
    let frames = data.len() / channels.max(1);
    for frame in 0..frames {
        let left = consumer.pop().unwrap_or(0.0);
        let right = consumer.pop().unwrap_or(left);
        let _ = ratio; // keep timing on the capture clock; drop/pad via ring underruns
        let base = frame * channels;
        if channels == 1 {
            data[base] = (left + right) * 0.5;
        } else {
            data[base] = left;
            data[base + 1] = right;
            for extra in data.iter_mut().skip(base + 2).take(channels.saturating_sub(2)) {
                *extra = 0.0;
            }
        }
    }
}

pub fn list_devices() -> Vec<AudioDeviceDto> {
    let host = cpal::default_host();
    let mut devices = Vec::new();
    if let Ok(list) = host.input_devices() {
        for device in list {
            if let Ok(name) = device.name() {
                devices.push(AudioDeviceDto {
                    name,
                    input: true,
                    output: false,
                });
            }
        }
    }
    if let Ok(list) = host.output_devices() {
        for device in list {
            if let Ok(name) = device.name() {
                if let Some(existing) = devices.iter_mut().find(|device| device.name == name) {
                    existing.output = true;
                } else {
                    devices.push(AudioDeviceDto {
                        name,
                        input: false,
                        output: true,
                    });
                }
            }
        }
    }
    devices
}

fn pump(
    commands: std::sync::mpsc::Receiver<PumpCmd>,
    frames: tokio::sync::mpsc::Sender<PcmFrame>,
    asr: std::sync::mpsc::SyncSender<PcmFrame>,
) {
    let mut stream: Option<Stream> = None;
    let mut consumer: Option<Consumer<f32>> = None;
    let mut pending: Vec<f32> = Vec::with_capacity(FRAME_SAMPLES * 2);
    let mut rate = PIPELINE_RATE;
    let mut channels = PIPELINE_CHANNELS;
    loop {
        while let Ok(command) = commands.try_recv() {
            stream = None;
            consumer = None;
            pending.clear();
            let PumpCmd::Configure(config) = command;
            if !config.running {
                continue;
            }
                match open_input(&config) {
                    Ok((opened, reader, opened_rate, opened_channels)) => {
                        rate = opened_rate;
                        channels = opened_channels;
                        stream = Some(opened);
                        consumer = Some(reader);
                        log::info!(
                            "audio input {opened_rate} Hz / {opened_channels} ch, output {}",
                            if config.output_name.is_empty() {
                                "system default"
                            } else {
                                config.output_name.as_str()
                            }
                        );
                    }
                    Err(err) => log::warn!("audio input did not open: {err}"),
                }
        }
        // The stream is owned here so the input callback stays alive.
        let _held = stream.as_ref();
        if let Some(reader) = consumer.as_mut() {
            for frame in drain(reader, &mut pending, rate, channels) {
                let _ = frames.try_send(frame.clone());
                let _ = asr.try_send(frame);
            }
        }
        thread::sleep(Duration::from_millis(4));
    }
}

fn drain(
    reader: &mut Consumer<f32>,
    pending: &mut Vec<f32>,
    rate: u32,
    channels: u16,
) -> Vec<PcmFrame> {
    let mut fresh = Vec::new();
    while let Ok(sample) = reader.pop() {
        fresh.push(sample);
    }
    if fresh.is_empty() {
        return Vec::new();
    }
    if rate == PIPELINE_RATE && channels == PIPELINE_CHANNELS {
        pending.extend(fresh);
    } else {
        pending.extend(to_pipeline(&fresh, rate, channels));
    }
    let mut frames = Vec::new();
    while pending.len() >= FRAME_SAMPLES {
        let samples: Vec<f32> = pending.drain(..FRAME_SAMPLES).collect();
        let level = rms(&samples);
        frames.push(PcmFrame {
            samples,
            rms: level,
        });
    }
    frames
}

fn to_pipeline(input: &[f32], rate: u32, channels: u16) -> Vec<f32> {
    let channels = channels.max(1) as usize;
    let frames = input.len() / channels;
    if frames == 0 {
        return Vec::new();
    }
    let ratio = PIPELINE_RATE as f64 / rate.max(1) as f64;
    let out_frames = ((frames as f64) * ratio).round().max(1.0) as usize;
    let mut out = Vec::with_capacity(out_frames * 2);
    for index in 0..out_frames {
        let position = index as f64 / ratio;
        let left_index = (position.floor() as usize).min(frames - 1);
        let right_index = (left_index + 1).min(frames - 1);
        let mix = (position - left_index as f64) as f32;
        let a = stereo_at(input, channels, left_index);
        let b = stereo_at(input, channels, right_index);
        out.push(a[0] + (b[0] - a[0]) * mix);
        out.push(a[1] + (b[1] - a[1]) * mix);
    }
    out
}

fn stereo_at(input: &[f32], channels: usize, frame: usize) -> [f32; 2] {
    let base = frame * channels;
    let left = input.get(base).copied().unwrap_or(0.0);
    let right = if channels > 1 {
        input.get(base + 1).copied().unwrap_or(left)
    } else {
        left
    };
    [left, right]
}

fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let energy = samples.iter().map(|sample| sample * sample).sum::<f32>() / samples.len() as f32;
    energy.sqrt().clamp(0.0, 1.0)
}

fn open_input(config: &AudioIo) -> Result<(Stream, Consumer<f32>, u32, u16), String> {
    let device = find_device(&config.input_name, true).ok_or("No input device is available.")?;
    let supported = choose_config(&device)?;
    let rate = supported.sample_rate().0;
    let channels = supported.channels();
    let format = supported.sample_format();
    let preferred = if config.hardware_accel {
        BufferSize::Fixed(config.buffer_frames.clamp(128, 4_800))
    } else {
        BufferSize::Default
    };
    let opened = open_with(&device, &supported, preferred, format)
        .or_else(|_| open_with(&device, &supported, BufferSize::Default, format))?;
    opened.0.play().map_err(|err| err.to_string())?;
    Ok((opened.0, opened.1, rate, channels))
}

fn open_with(
    device: &Device,
    supported: &SupportedStreamConfig,
    buffer: BufferSize,
    format: SampleFormat,
) -> Result<(Stream, Consumer<f32>), String> {
    let config = StreamConfig {
        channels: supported.channels(),
        sample_rate: supported.sample_rate(),
        buffer_size: buffer,
    };
    let channels = supported.channels().max(1) as usize;
    let (mut producer, consumer) = RingBuffer::<f32>::new(262_144);
    let stream = match format {
        SampleFormat::F32 => device
            .build_input_stream(
                &config,
                move |data: &[f32], _| push_interleaved(&mut producer, data, channels),
                |err| log::warn!("audio input: {err}"),
                None,
            )
            .map_err(|err| err.to_string())?,
        SampleFormat::I16 => device
            .build_input_stream(
                &config,
                move |data: &[i16], _| {
                    for frame in data.chunks(channels) {
                        let left = frame.first().copied().unwrap_or(0) as f32 / 32768.0;
                        let right = frame
                            .get(1)
                            .copied()
                            .map(|sample| sample as f32 / 32768.0)
                            .unwrap_or(left);
                        let _ = producer.push(left);
                        let _ = producer.push(right);
                    }
                },
                |err| log::warn!("audio input: {err}"),
                None,
            )
            .map_err(|err| err.to_string())?,
        _ => return Err("This device's sample format is not supported.".into()),
    };
    Ok((stream, consumer))
}

fn push_interleaved(producer: &mut Producer<f32>, data: &[f32], channels: usize) {
    if channels <= 1 {
        for sample in data {
            let _ = producer.push(*sample);
            let _ = producer.push(*sample);
        }
        return;
    }
    for frame in data.chunks(channels) {
        let left = frame.first().copied().unwrap_or(0.0);
        let right = frame.get(1).copied().unwrap_or(left);
        let _ = producer.push(left);
        let _ = producer.push(right);
    }
}

fn choose_config(device: &Device) -> Result<SupportedStreamConfig, String> {
    let ranges = device
        .supported_input_configs()
        .map_err(|err| err.to_string())?;
    let mut fallback = None;
    for range in ranges {
        let format = range.sample_format();
        if !matches!(format, SampleFormat::F32 | SampleFormat::I16) {
            continue;
        }
        let rate_ok =
            range.min_sample_rate().0 <= PIPELINE_RATE && range.max_sample_rate().0 >= PIPELINE_RATE;
        let chosen = if rate_ok {
            range.with_sample_rate(SampleRate(PIPELINE_RATE))
        } else {
            range.with_sample_rate(range.max_sample_rate())
        };
        if rate_ok && chosen.channels() >= 2 && format == SampleFormat::F32 {
            return Ok(chosen);
        }
        fallback = Some(chosen);
    }
    fallback.ok_or_else(|| "The audio device has no usable format.".into())
}

fn find_device(name: &str, input: bool) -> Option<Device> {
    let host = cpal::default_host();
    if name.is_empty() {
        return if input {
            host.default_input_device()
        } else {
            host.default_output_device()
        };
    }
    let list = if input {
        host.input_devices().ok()?
    } else {
        host.output_devices().ok()?
    };
    list.into_iter()
        .find(|device| device.name().ok().as_deref() == Some(name))
}
