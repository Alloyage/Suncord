pub use gst::ClockTime;
use gstreamer::{self as gst, prelude::*};
use gstreamer_app::AppSink;
use std::sync::{
    atomic::{AtomicU16, AtomicU64, Ordering},
    Arc,
};
use tokio::sync::mpsc;

#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "windows")]
const AUDIO_PROVIDER: &str = "wasapi2deviceprovider";
#[cfg(target_os = "linux")]
const AUDIO_PROVIDER: &str = "pulsedeviceprovider";
#[cfg(target_os = "windows")]
const AUDIO_SOURCE: &str = "wasapi2src";
#[cfg(target_os = "linux")]
const AUDIO_SOURCE: &str = "pulsesrc";

const AUDIO_BITRATE: u32 = 128_000;
/// Discord negotiates constrained-baseline H.264 for browser viewers.
const H264_PROFILE: &str = "constrained-baseline";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    Opus,
    /// Annex B byte-stream, one access unit per frame.
    H264,
    /// Annex B byte-stream, one access unit per frame.
    H265,
    /// Low-overhead OBU stream, one temporal unit per frame.
    Av1,
}

impl Codec {
    /// RTP clock rate in ticks per second.
    pub(crate) fn clock(self) -> u32 {
        match self {
            Self::Opus => 48_000,
            _ => 90_000,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoEncoder {
    X264,
    Nvenc,
}

impl VideoEncoder {
    pub fn available(self, codec: Codec) -> Result<bool, String> {
        gst::init().map_err(|e| e.to_string())?;
        Ok(self
            .element(codec)
            .is_some_and(|name| gst::ElementFactory::find(name).is_some()))
    }

    /// Video codecs this machine encodes, most efficient first.
    /// NVENC elements register only for codecs the GPU supports.
    pub fn codecs(self) -> Result<Vec<Codec>, String> {
        let mut codecs = Vec::new();
        for codec in [Codec::Av1, Codec::H265, Codec::H264] {
            if self.available(codec)? {
                codecs.push(codec);
            }
        }
        Ok(codecs)
    }

    /// The encoder's bitrate property units. GStreamer's NVENC kbit is 1024 bits.
    fn kbits(self, bitrate: u32) -> u32 {
        match self {
            Self::X264 => bitrate / 1000,
            Self::Nvenc => bitrate / 1024,
        }
    }

    pub fn element(self, codec: Codec) -> Option<&'static str> {
        match (self, codec) {
            (Self::X264, Codec::H264) => Some("x264enc"),
            (Self::Nvenc, Codec::H264) => Some(if cfg!(target_os = "windows") {
                "nvd3d11h264enc"
            } else {
                "nvh264enc"
            }),
            (Self::Nvenc, Codec::H265) => Some(if cfg!(target_os = "windows") {
                "nvd3d11h265enc"
            } else {
                "nvh265enc"
            }),
            (Self::Nvenc, Codec::Av1) => Some(if cfg!(target_os = "windows") {
                "nvd3d11av1enc"
            } else {
                "nvav1enc"
            }),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct VideoSettings {
    pub encoder: VideoEncoder,
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    /// Most frames per second. Static screens send fewer.
    pub fps: u32,
    /// Bits per second, shared by the encoder and stream announcement.
    pub bitrate: u32,
}

impl VideoSettings {
    pub fn validate(self) -> Result<Self, String> {
        if !(16..=3840).contains(&self.width)
            || !self.width.is_multiple_of(2)
            || !(16..=2160).contains(&self.height)
            || !self.height.is_multiple_of(2)
            || !(1..=240).contains(&self.fps)
            || !(100_000..=50_000_000).contains(&self.bitrate)
            || !self.bitrate.is_multiple_of(1000)
        {
            return Err("Video requires even dimensions from 16×16 to 3840×2160, 1–240 fps, and 100000–50000000 bit/s in steps of 1000".into());
        }
        if self.encoder.element(self.codec).is_none() {
            return Err(format!("{:?} cannot encode {:?}", self.encoder, self.codec));
        }
        if self.encoder == VideoEncoder::Nvenc && (self.width < 192 || self.height < 128) {
            return Err("NVENC requires dimensions of at least 192×128".into());
        }
        Ok(self)
    }
}

pub struct Frame {
    pub codec: Codec,
    pub pts: gst::ClockTime,
    pub data: Vec<u8>,
    pub discontinuity: bool,
    pub keyframe: bool,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioSource {
    pub id: String,
    pub name: String,
    pub monitor: bool,
    pub is_default: bool,
}

/// Owns the pipeline and its callbacks. Dropping it stops capture synchronously.
pub struct Capture {
    pipeline: gst::Pipeline,
    sink: AppSink,
    sender: mpsc::Sender<Frame>,
    pub frames: Arc<AtomicU64>,
    /// Loudest audio sample since the caller last reset it.
    pub peak: Arc<AtomicU16>,
    /// Source and settings that rebuild video in another codec.
    video: Option<(String, VideoSettings)>,
    portal: Option<Portal>,
}

#[cfg(target_os = "linux")]
struct Portal {
    session: Option<ashpd::desktop::Session<ashpd::desktop::screencast::Screencast>>,
    _remote: Option<std::os::fd::OwnedFd>,
}

#[cfg(target_os = "linux")]
impl Drop for Portal {
    fn drop(&mut self) {
        if let Some(session) = self.session.take() {
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    let _ = session.close().await;
                });
            }
        }
    }
}

#[cfg(target_os = "windows")]
struct Portal;

#[derive(serde::Serialize)]
pub struct ScreenSource {
    pub id: String,
    pub name: String,
    pub width: i32,
    pub height: i32,
    pub rate: i32,
}

impl Capture {
    pub fn screen_sources() -> Vec<ScreenSource> {
        #[cfg(target_os = "windows")]
        {
            windows::screens()
        }
        #[cfg(target_os = "linux")]
        {
            Vec::new()
        }
    }

    /// Enumerate capture devices and output monitors without starting capture.
    pub fn audio_sources() -> Result<Vec<AudioSource>, String> {
        gst::init().map_err(|e| e.to_string())?;
        let provider = gst::DeviceProviderFactory::by_name(AUDIO_PROVIDER)
            .ok_or("Audio device provider unavailable")?;
        provider.start().map_err(|e| e.to_string())?;
        let sources = provider
            .devices()
            .iter()
            .filter(|device| device.device_class() == "Audio/Source")
            .map(|device| {
                let properties = device.properties();
                #[cfg(target_os = "windows")]
                let monitor = properties
                    .as_ref()
                    .and_then(|p| p.get::<bool>("wasapi2.device.loopback").ok())
                    .unwrap_or(false);
                #[cfg(target_os = "windows")]
                let id = format!(
                    "{}{}",
                    if monitor { "loopback:" } else { "" },
                    device.property::<String>("device")
                );
                #[cfg(target_os = "linux")]
                let id = device.property::<String>("internal-name");
                AudioSource {
                    id,
                    name: device.display_name().to_string(),
                    #[cfg(target_os = "windows")]
                    monitor,
                    #[cfg(target_os = "linux")]
                    monitor: properties
                        .as_ref()
                        .and_then(|p| p.get::<String>("device.class").ok())
                        .is_some_and(|class| class == "monitor"),
                    is_default: properties
                        .as_ref()
                        .and_then(|p| {
                            p.get::<bool>(if cfg!(target_os = "windows") {
                                "device.default"
                            } else {
                                "is-default"
                            })
                            .ok()
                        })
                        .unwrap_or(false),
                }
            })
            .collect();
        provider.stop();
        Ok(sources)
    }

    /// Encodes stereo without adding speech processing or changing system devices.
    /// `None` is silence, never an implicit default microphone.
    /// `metered` tracks the loudest sample in `peak`, for speaking indicators.
    pub fn audio(
        device: Option<&str>,
        metered: bool,
    ) -> Result<(Self, mpsc::Receiver<Frame>), String> {
        if device.is_some_and(str::is_empty) {
            return Err("Select an audio source explicitly".into());
        }
        gst::init().map_err(|e| e.to_string())?;
        let source = if device.is_some() {
            AUDIO_SOURCE
        } else {
            "audiotestsrc is-live=true wave=silence"
        };
        let pipeline = gst::parse::launch(&format!(
            "{source} name=input ! audioconvert ! audioresample ! audio/x-raw,format=S16LE,rate=48000,channels=2 ! opusenc name=encoder bitrate={AUDIO_BITRATE} audio-type=generic frame-size=20 ! appsink name=encoded sync=false max-buffers=2 drop=true"
        )).map_err(|e| e.to_string())?.downcast::<gst::Pipeline>().map_err(|_| "Invalid audio pipeline")?;
        if let Some(device) = device {
            let input = pipeline.by_name("input").ok_or("Missing audio source")?;
            #[cfg(target_os = "windows")]
            let device = if let Some(id) = device.strip_prefix("loopback:") {
                input.set_property("loopback", true);
                id
            } else {
                device
            };
            input.set_property("device", device);
        }
        let peak = Arc::new(AtomicU16::new(0));
        let level = peak.clone();
        if metered {
            pipeline
                .by_name("encoder")
                .and_then(|encoder| encoder.static_pad("sink"))
                .ok_or("Missing audio encoder")?
                .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                    if let Some(map) = info.buffer().and_then(|b| b.map_readable().ok()) {
                        let loudest = map
                            .as_chunks::<2>()
                            .0
                            .iter()
                            .map(|s| i16::from_le_bytes(*s).unsigned_abs())
                            .max()
                            .unwrap_or(0);
                        level.fetch_max(loudest, Ordering::Relaxed);
                    }
                    gst::PadProbeReturn::Ok
                });
        }
        let (mut capture, frames) = Self::start(pipeline, Codec::Opus)?;
        capture.peak = peak;
        Ok((capture, frames))
    }

    fn video(source: &str, settings: VideoSettings) -> Result<gst::Pipeline, String> {
        let VideoSettings {
            encoder,
            codec,
            width,
            height,
            fps,
            bitrate,
        } = settings.validate()?;
        gst::init().map_err(|e| e.to_string())?;
        let bitrate = encoder.kbits(bitrate);
        let vbv = bitrate / 4;
        // P4 gains about 0.4 dB over P1, but H.265 P4 tops out near 4K60 on one shared NVENC engine.
        let preset = if u64::from(width) * u64::from(height) * u64::from(fps) > 2560 * 1440 * 120 {
            "p1"
        } else {
            "p4"
        };
        let element = encoder.element(codec).unwrap();
        // Keyframes come only on request, from viewer PLIs, new viewers and dropped frames.
        // Periodic ones are bursts the bitrate cannot absorb, and stutter the stream each time.
        // A quarter-second rate buffer keeps keyframes small enough for the pacer to send in time.
        // Frames stay on the GPU from the DMA-BUF to NVENC. Rendering NV12 hands NVENC a PBO it can map.
        let (scale, encoder) = match encoder {
            VideoEncoder::X264 => (if cfg!(target_os = "windows") { "d3d11download ! videoconvert ! videoscale ! video/x-raw" } else { "videoconvert ! videoscale ! video/x-raw" }, format!("x264enc name=video_encoder tune=zerolatency speed-preset=ultrafast bitrate={bitrate} key-int-max=1073741824 vbv-buf-capacity=250 byte-stream=true")),
            #[cfg(target_os = "linux")]
            VideoEncoder::Nvenc => ("glupload ! glcolorconvert ! glcolorscale ! video/x-raw(memory:GLMemory)", format!("glcolorconvert ! video/x-raw(memory:GLMemory),format=NV12 ! {element} name=video_encoder preset={preset} tune=ultra-low-latency rc-mode=cbr bitrate={bitrate} vbv-buffer-size={vbv} gop-size=-1 bframes=0 rc-lookahead=0 zerolatency=true{}", if codec == Codec::Av1 { "" } else { " repeat-sequence-header=true" })),
            #[cfg(target_os = "windows")]
            VideoEncoder::Nvenc => ("d3d11convert ! video/x-raw(memory:D3D11Memory)", format!("{element} name=video_encoder preset={preset} tune=ultra-low-latency rc-mode=cbr bitrate={bitrate} vbv-buffer-size={vbv} gop-size=-1 bframes=0 rc-lookahead=0 zerolatency=true{}", if codec == Codec::Av1 { "" } else { " repeat-sequence-header=true" })),
        };
        let output = match codec {
            Codec::H264 => format!(
                "video/x-h264,stream-format=byte-stream,alignment=au,profile={H264_PROFILE}"
            ),
            Codec::H265 => {
                "video/x-h265,stream-format=byte-stream,alignment=au,profile=main".into()
            }
            Codec::Av1 => "video/x-av1,stream-format=obu-stream,alignment=tu".into(),
            Codec::Opus => unreachable!("validated video codec"),
        };
        gst::parse::launch(&format!(
            "{source} ! {scale},width={width},height={height},pixel-aspect-ratio=1/1 ! {encoder} ! {output} ! appsink name=encoded sync=false max-buffers=2 drop=true"
        )).map_err(|e| e.to_string())?.downcast::<gst::Pipeline>().map_err(|_| "Invalid video pipeline".into())
    }

    /// Select one source through the portal. Nothing is saved for later capture.
    #[cfg(target_os = "linux")]
    pub async fn screen(
        encoder: VideoEncoder,
        codec: Codec,
        selected: Option<&str>,
    ) -> Result<(Self, mpsc::Receiver<Frame>, VideoSettings), String> {
        if selected.is_some() {
            return Err("Choose the screen through the capture portal".into());
        }
        use ashpd::desktop::screencast::{
            CursorMode, Screencast, SelectSourcesOptions, SourceType,
        };
        use std::os::fd::AsRawFd;
        gst::init().map_err(|e| e.to_string())?;
        let element = encoder
            .element(codec)
            .ok_or_else(|| format!("{encoder:?} cannot encode {codec:?}"))?;
        let scale: &[_] = match encoder {
            VideoEncoder::X264 => &["videoconvert", "videoscale"],
            VideoEncoder::Nvenc => &["glupload", "glcolorconvert", "glcolorscale"],
        };
        for &name in [&["pipewiresrc", element][..], scale].concat().iter() {
            if gst::ElementFactory::find(name).is_none() {
                return Err(format!("Missing native media plugin: {name}"));
            }
        }
        let portal = Screencast::new().await.map_err(|e| e.to_string())?;
        // Finish creation so a cancelled request still closes its returned session.
        let (portal, mut owner) = tokio::spawn(async move {
            let session = portal
                .create_session(Default::default())
                .await
                .map_err(|e| e.to_string())?;
            Ok::<_, String>((
                portal,
                Portal {
                    session: Some(session),
                    _remote: None,
                },
            ))
        })
        .await
        .map_err(|_| "Screen session creation stopped")??;
        let session = owner.session.as_ref().unwrap();
        portal
            .select_sources(
                session,
                SelectSourcesOptions::default()
                    .set_cursor_mode(CursorMode::Embedded)
                    .set_sources(SourceType::Monitor | SourceType::Window)
                    .set_multiple(false)
                    .set_persist_mode(ashpd::desktop::PersistMode::DoNot),
            )
            .await
            .map_err(|e| e.to_string())?
            .response()
            .map_err(|e| e.to_string())?;
        let response = portal
            .start(session, None, Default::default())
            .await
            .map_err(|e| e.to_string())?
            .response()
            .map_err(|e| e.to_string())?;
        let node = response
            .streams()
            .first()
            .ok_or("No screen selected")?
            .pipe_wire_node_id();
        let remote = portal
            .open_pipe_wire_remote(session, Default::default())
            .await
            .map_err(|e| e.to_string())?;
        // Keep the pipeline clock independent of the PipeWire stream rebuilt on codec changes.
        // FD and node are integers obtained from this portal, never pipeline text from the UI.
        let source = format!(
            "pipewiresrc fd={} path={node} provide-clock=false do-timestamp=true",
            remote.as_raw_fd()
        );
        let probe_source = source.clone();
        let (width, height, rate, remote) = tokio::task::spawn_blocking(move || {
            let pipeline = gst::parse::launch(&format!(
                "{probe_source} ! video/x-raw ! appsink name=size sync=false max-buffers=1 drop=true"
            ))
            .map_err(|e| e.to_string())?
            .downcast::<gst::Pipeline>()
            .map_err(|_| "Invalid capture probe")?;
            let result = (|| {
                let sink = pipeline
                    .by_name("size")
                    .ok_or("Missing capture probe")?
                    .downcast::<AppSink>()
                    .map_err(|_| "Invalid capture probe")?;
                pipeline
                    .set_state(gst::State::Playing)
                    .map_err(|_| "Could not inspect screen size")?;
                let sample = sink
                    .try_pull_sample(gst::ClockTime::from_seconds(5))
                    .ok_or("Screen size was not available")?;
                let caps = sample
                    .caps()
                    .and_then(|caps| caps.structure(0))
                    .ok_or("Missing screen dimensions")?;
                // Damage-driven sources announce a variable rate with the display's maximum.
                let rate = ["max-framerate", "framerate"]
                    .iter()
                    .filter_map(|field| caps.get::<gst::Fraction>(*field).ok())
                    .find(|rate| rate.numer() > 0)
                    .map_or(60, |rate| rate.numer() / rate.denom());
                Ok::<_, String>((
                    caps.get::<i32>("width")
                        .map_err(|_| "Missing screen width")?,
                    caps.get::<i32>("height")
                        .map_err(|_| "Missing screen height")?,
                    rate,
                ))
            })();
            let _ = pipeline.set_state(gst::State::Null);
            // Cancellation cannot close the FD while this blocking probe still uses it.
            result.map(|(width, height, rate)| (width, height, rate, remote))
        })
        .await
        .map_err(|_| "Screen size inspection stopped")??;
        owner._remote = Some(remote);
        Self::screen_capture(source, encoder, codec, width, height, rate, Some(owner))
    }

    #[cfg(target_os = "windows")]
    pub async fn screen(
        encoder: VideoEncoder,
        codec: Codec,
        selected: Option<&str>,
    ) -> Result<(Self, mpsc::Receiver<Frame>, VideoSettings), String> {
        let selected = selected.ok_or("Select a screen or window")?;
        let source = windows::screens()
            .into_iter()
            .find(|source| source.id == selected)
            .ok_or("The selected screen or window is no longer available")?;
        let (kind, handle) = source.id.split_once(':').ok_or("Invalid capture source")?;
        let handle = handle
            .parse::<u64>()
            .map_err(|_| "Invalid capture source")?;
        let source_pipeline = format!(
            "d3d11screencapturesrc capture-api=wgc {kind}-handle={handle} show-cursor=true"
        );
        Self::screen_capture(
            source_pipeline,
            encoder,
            codec,
            source.width,
            source.height,
            source.rate,
            None,
        )
    }

    fn screen_capture(
        source: String,
        encoder: VideoEncoder,
        codec: Codec,
        width: i32,
        height: i32,
        rate: i32,
        portal: Option<Portal>,
    ) -> Result<(Self, mpsc::Receiver<Frame>, VideoSettings), String> {
        if width < 16 || height < 16 {
            return Err("The selected surface is too small to share".into());
        }
        let scale = (3840.0 / f64::from(width))
            .min(2160.0 / f64::from(height))
            .min(1.0);
        let width = ((f64::from(width) * scale) as u32 / 2 * 2).max(16);
        let height = ((f64::from(height) * scale) as u32 / 2 * 2).max(16);
        let fps = rate.clamp(1, 240) as u32;
        // H.265 and AV1 reach H.264's quality with about a third less bitrate.
        let bits_per_pixel = if codec == Codec::H264 { 0.15 } else { 0.1 };
        let settings = VideoSettings {
            encoder,
            codec,
            width,
            height,
            fps,
            bitrate: ((f64::from(width * height * fps) * bits_per_pixel) as u32 / 1000 * 1000)
                .clamp(4_000_000, 50_000_000),
        }
        .validate()?;
        // The source paces frames at their damage times, up to the chosen rate, without duplicates.
        // The last frame repeats after a quiet quarter second so keyframe requests on a static screen still complete.
        #[cfg(target_os = "linux")]
        let source =
            format!("{source} keepalive-time=250 ! video/x-raw(ANY),max-framerate={fps}/1");
        #[cfg(target_os = "windows")]
        let source = format!("{source} ! video/x-raw(memory:D3D11Memory),framerate={fps}/1");
        let (mut capture, frames) = Self::start(Self::video(&source, settings)?, codec)?;
        capture.video = Some((source, settings));
        capture.portal = portal;
        Ok((capture, frames, settings))
    }

    fn start(
        pipeline: gst::Pipeline,
        codec: Codec,
    ) -> Result<(Self, mpsc::Receiver<Frame>), String> {
        let (sender, receiver) = mpsc::channel(4);
        let mut capture = Self {
            sink: Self::sink(&pipeline)?,
            pipeline,
            sender,
            frames: Arc::default(),
            peak: Arc::default(),
            video: None,
            portal: None,
        };
        capture.listen(codec)?;
        Ok((capture, receiver))
    }

    fn sink(pipeline: &gst::Pipeline) -> Result<AppSink, String> {
        pipeline
            .by_name("encoded")
            .ok_or("Missing encoded sink")?
            .downcast::<AppSink>()
            .map_err(|_| "Invalid encoded sink".into())
    }

    fn listen(&mut self, codec: Codec) -> Result<(), String> {
        let sender = self.sender.clone();
        let produced = self.frames.clone();
        let mut discontinuity = false;
        self.sink.set_callbacks(
            gstreamer_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                    produced.fetch_add(1, Ordering::Relaxed);
                    // Reserve before copying so a stalled consumer cannot cause repeated allocations.
                    let permit = match sender.try_reserve() {
                        Ok(permit) => permit,
                        Err(mpsc::error::TrySendError::Full(_)) => {
                            discontinuity = true;
                            return Ok(gst::FlowSuccess::Ok);
                        }
                        Err(mpsc::error::TrySendError::Closed(_)) => {
                            return Err(gst::FlowError::Eos)
                        }
                    };
                    let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                    // Encoders offset PTS (often by 1000 hours). The segment maps it
                    // back to the shared running time, including after a codec switch.
                    let pts = sample
                        .segment()
                        .and_then(|segment| segment.downcast_ref::<gst::ClockTime>())
                        .and_then(|segment| segment.to_running_time(buffer.pts()?))
                        .ok_or(gst::FlowError::Error)?;
                    let frame = Frame {
                        codec,
                        pts,
                        data: map.as_slice().to_vec(),
                        discontinuity: std::mem::take(&mut discontinuity),
                        keyframe: !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT),
                    };
                    permit.send(frame);
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
        self.pipeline
            .set_state(gst::State::Playing)
            .map_err(|_| "Could not start native capture")?;
        Ok(())
    }

    /// Rebuilds video in another codec from the same portal stream, without a new prompt.
    /// A new encoder starts with a keyframe at the current bitrate.
    pub fn set_codec(&mut self, codec: Codec) -> Result<(), String> {
        let (source, settings) = self.video.clone().ok_or("Capture has no video")?;
        if settings.codec == codec {
            return Ok(());
        }
        let settings = VideoSettings { codec, ..settings }.validate()?;
        let pipeline = Self::video(&source, settings)?;
        let sink = Self::sink(&pipeline)?;
        // RTP keeps its timeline across codec changes. Do not restart source timestamps.
        pipeline.use_clock(self.pipeline.clock().as_ref());
        pipeline.set_start_time(gst::ClockTime::NONE);
        pipeline.set_base_time(self.pipeline.base_time().ok_or("Capture clock missing")?);
        // Stop the old source before reconnecting capture.
        let _ = self.pipeline.set_state(gst::State::Null);
        self.sink
            .set_callbacks(gstreamer_app::AppSinkCallbacks::builder().build());
        self.pipeline = pipeline;
        self.sink = sink;
        self.video = Some((source, settings));
        self.listen(codec)
    }

    pub fn error(&self) -> Option<String> {
        let bus = self.pipeline.bus()?;
        let message = bus.pop_filtered(&[gst::MessageType::Error, gst::MessageType::Eos])?;
        Some(match message.view() {
            gst::MessageView::Error(error) => error.error().to_string(),
            _ => "Capture ended".into(),
        })
    }

    /// Changes only the target rate. A new NVENC rate buffer size would rebuild the session
    /// with a keyframe, so it stays at a quarter second of the starting bitrate.
    pub fn set_video_bitrate(&mut self, bitrate: u32) -> Result<(), String> {
        if !(100_000..=50_000_000).contains(&bitrate) || !bitrate.is_multiple_of(1000) {
            return Err("Invalid video bitrate".into());
        }
        let (_, settings) = self.video.as_mut().ok_or("Capture has no video")?;
        self.pipeline
            .by_name("video_encoder")
            .ok_or("Video encoder missing")?
            .set_property("bitrate", settings.encoder.kbits(bitrate));
        settings.bitrate = bitrate;
        Ok(())
    }

    pub fn request_keyframe(&self) -> Result<(), String> {
        let event = gstreamer_video::UpstreamForceKeyUnitEvent::builder()
            .all_headers(true)
            .build();
        self.sink
            .send_event(event)
            .then_some(())
            .ok_or_else(|| "Encoder rejected the keyframe request".into())
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        // NULL waits for streaming tasks before releasing the callback's channel sender.
        let _ = self.pipeline.set_state(gst::State::Null);
        self.sink
            .set_callbacks(gstreamer_app::AppSinkCallbacks::builder().build());
    }
}
