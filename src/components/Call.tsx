import { FormEvent, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import {
  leaveVoice,
  listCaptureSources,
  setCapturePreview,
  setDeafened,
  setMuted,
  startCapture,
  stopCapture,
} from "../lib/backend";
import { useApp } from "../state/store";
import { CaptureFrame, CaptureSource, VoiceMember } from "../types";

export function CallOverlay() {
  const voice = useApp((state) => state.voice);
  const capture = useApp((state) => state.capture);
  const user = useApp((state) => state.user);
  // Keep the overlay mounted for any non-idle voice session — including transient
  // reconnect/"error" flaps — so clicks never flash the text-chat surface underneath.
  const inVoice = Boolean(voice.channelId) && voice.phase !== "idle";
  const sharing = Boolean(capture?.running);
  const [expanded, setExpanded] = useState(true);

  useEffect(() => {
    if (inVoice || sharing) setExpanded(true);
  }, [inVoice, voice.channelId, sharing]);

  useEffect(() => {
    const enabled = sharing && (expanded || inVoice);
    void setCapturePreview(enabled).catch(() => undefined);
    return () => {
      if (!useApp.getState().shareOpen) void setCapturePreview(false).catch(() => undefined);
    };
  }, [sharing, expanded, inVoice]);

  const users = useApp((state) => state.users);
  const dms = useApp((state) => state.dms);
  const channels = useApp((state) => state.channels);
  const participants = useMemo(() => {
    const channel =
      [...(dms ?? []), ...(channels ?? [])].find((item) => item.id === voice.channelId) ?? null;
    return mergeParticipants(
      voice.members ?? [],
      user,
      voice.muted,
      voice.deafened,
      channel?.recipientIds ?? [],
      users ?? {},
    );
  }, [voice.members, voice.muted, voice.deafened, voice.channelId, user, users, dms, channels]);
  const count = Math.max(participants.length, inVoice ? 1 : 0);

  if (!inVoice && !sharing) return null;

  const title = inVoice ? voice.channelName || "Voice call" : "Screen share";
  const detail = inVoice ? voice.detail : "";

  if (!expanded) {
    return (
      <div className="pointer-events-auto absolute left-0 right-0 top-0 z-20 flex h-11 items-center gap-2 bg-[#0c0c0c] px-3 shadow-[0_1px_0_0_rgba(255,255,255,0.06)]">
        <span className="min-w-0 flex-1 truncate text-sm">
          {title}
          {inVoice ? <ParticipantBadge count={count} /> : null}
          {detail ? <span className="ml-2 text-xs text-mute">{detail}</span> : null}
        </span>
        <CallControls
          inVoice={inVoice}
          sharing={sharing}
          muted={Boolean(voice.muted)}
          deafened={Boolean(voice.deafened)}
          expanded={false}
          onExpand={() => setExpanded(true)}
        />
      </div>
    );
  }

  return (
    <div className="pointer-events-auto absolute inset-0 z-20 flex flex-col bg-[#0a0a0a]">
      <header className="flex h-12 shrink-0 items-center gap-3 border-b border-white/5 px-4">
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-2">
            <h2 className="truncate text-sm font-semibold tracking-wide">{title}</h2>
            {inVoice ? <ParticipantBadge count={count} /> : null}
          </div>
          {detail ? <p className="truncate text-xs text-mute">{detail}</p> : null}
        </div>
        <span className="shrink-0 text-[10px] uppercase tracking-[0.14em] text-mute">
          {voice.phase === "live" ? voice.transport || "live" : voice.phase}
        </span>
      </header>

      <div className="relative min-h-0 flex-1 p-4">
        <CallGrid sharing={sharing} participants={participants} speakingLevel={voice.level} />
      </div>

      <footer className="flex shrink-0 items-center justify-center gap-2 border-t border-white/5 bg-[#0c0c0c] px-4 py-3">
        <CallControls
          inVoice={inVoice}
          sharing={sharing}
          muted={Boolean(voice.muted)}
          deafened={Boolean(voice.deafened)}
          expanded
          onExpand={() => setExpanded(false)}
        />
      </footer>
    </div>
  );
}

function ParticipantBadge({ count }: { count: number }) {
  return (
    <span className="inline-flex shrink-0 items-center rounded bg-white/10 px-1.5 py-0.5 text-[11px] tabular-nums text-cream">
      {count} in call
    </span>
  );
}

function CallControls({
  inVoice,
  sharing,
  muted,
  deafened,
  expanded,
  onExpand,
}: {
  inVoice: boolean;
  sharing: boolean;
  muted: boolean;
  deafened: boolean;
  expanded: boolean;
  onExpand: () => void;
}) {
  return (
    <>
      {inVoice ? (
        <>
          <CallBarButton label={muted ? "Unmute" : "Mute"} active={muted} onClick={() => void setMuted(!muted)} />
          <CallBarButton
            label={deafened ? "Undeafen" : "Deafen"}
            active={deafened}
            onClick={() => void setDeafened(!deafened)}
          />
          <CallBarButton
            label={sharing ? "Sharing" : "Share Screen"}
            active={sharing}
            onClick={() => useApp.getState().setShareOpen(true)}
          />
          <CallBarButton label="Leave" danger onClick={() => void leaveVoice()} />
        </>
      ) : null}
      {sharing && !inVoice ? (
        <CallBarButton label="Stop share" danger onClick={() => void stopCapture()} />
      ) : null}
      {sharing && inVoice ? (
        <CallBarButton label="Stop share" danger onClick={() => void stopCapture()} />
      ) : null}
      <CallBarButton label={expanded ? "Collapse" : "Expand"} onClick={onExpand} />
    </>
  );
}

function CallGrid({
  sharing,
  participants,
  speakingLevel,
}: {
  sharing: boolean;
  participants: GridParticipant[];
  speakingLevel: number;
}) {
  const capture = useApp((state) => state.capture);
  const shareRef = useRef<HTMLDivElement>(null);
  const [fullscreen, setFullscreen] = useState(false);
  const [displayFps, setDisplayFps] = useState(0);
  const reportDisplayFps = useMemo(() => {
    let last = 0;
    return (fps: number) => {
      const rounded = Math.round(fps);
      if (rounded !== last) {
        last = rounded;
        setDisplayFps(rounded);
      }
    };
  }, []);

  useEffect(() => {
    const onChange = () => setFullscreen(Boolean(document.fullscreenElement));
    document.addEventListener("fullscreenchange", onChange);
    return () => document.removeEventListener("fullscreenchange", onChange);
  }, []);

  const toggleFullscreen = (event?: { stopPropagation?: () => void }) => {
    event?.stopPropagation?.();
    const node = shareRef.current;
    if (!node) return;
    if (document.fullscreenElement) {
      void document.exitFullscreen().catch(() => setFullscreen(false));
      return;
    }
    const request =
      node.requestFullscreen?.bind(node) ??
      (node as HTMLDivElement & { webkitRequestFullscreen?: () => Promise<void> }).webkitRequestFullscreen?.bind(node);
    if (request) {
      void Promise.resolve(request()).catch(() => setFullscreen(true));
      return;
    }
    setFullscreen((value) => !value);
  };

  const people = participants.map((person) => {
    const speaking = person.self ? speakingLevel > 0.04 : false;
    return <ParticipantTile key={person.id} person={person} speaking={speaking} />;
  });

  const personCols =
    people.length <= 1 ? "grid-cols-1" : people.length === 2 ? "grid-cols-2" : "grid-cols-2 lg:grid-cols-3";

  if (!sharing && !people.length) {
    return (
      <div className="flex h-full min-h-[180px] items-center justify-center text-sm text-mute">
        Waiting for participants…
      </div>
    );
  }

  return (
    <div className="flex h-full min-h-0 flex-col gap-3">
      {sharing ? (
        <div
          ref={shareRef}
          className={`relative flex min-h-0 flex-[1.4] items-center justify-center overflow-hidden rounded-lg bg-black ring-1 ring-white/10 ${
            fullscreen && !document.fullscreenElement ? "fixed inset-0 z-[80] rounded-none ring-0" : ""
          }`}
          onDoubleClick={() => toggleFullscreen()}
        >
          <WebGLCapturePreview
            className="h-full max-h-full w-full object-contain"
            onDisplayFps={reportDisplayFps}
          />
          <CaptureTelemetryBadge stats={capture} displayFps={displayFps} />
          <button
            type="button"
            title={fullscreen ? "Exit full screen" : "Full screen"}
            aria-label={fullscreen ? "Exit full screen" : "Full screen"}
            onClick={(event) => toggleFullscreen(event)}
            className="absolute bottom-3 right-3 z-10 rounded bg-black/70 px-2.5 py-1.5 text-[11px] text-cream ring-1 ring-white/15 hover:bg-black/85"
          >
            {fullscreen ? "Exit full screen" : "Full screen"}
          </button>
        </div>
      ) : null}
      {people.length ? (
        <div className={`grid min-h-0 flex-1 auto-rows-fr gap-3 ${personCols}`}>{people}</div>
      ) : null}
    </div>
  );
}

function CaptureTelemetryBadge({
  stats,
  displayFps,
}: {
  stats: { width?: number; height?: number; fps?: number; bitrateKbps?: number } | null | undefined;
  displayFps: number;
}) {
  if (!stats?.width || !stats?.height) return null;
  // Prefer measured canvas draw rate over capture-side emit counts (those inflate easily).
  const fps = Math.round(displayFps > 0 ? displayFps : stats.fps ?? 0);
  const bitrate = Math.round(stats.bitrateKbps ?? 0);
  return (
    <div className="pointer-events-none absolute left-3 top-3 rounded bg-black/70 px-2 py-1 font-mono text-[11px] tabular-nums text-cream ring-1 ring-white/10">
      {stats.width}×{stats.height}
      {fps > 0 ? ` · ${fps} fps` : ""}
      {bitrate > 0 ? ` · ${bitrate} kbps` : ""}
    </div>
  );
}

type GridParticipant = VoiceMember & { self?: boolean };

function ParticipantTile({ person, speaking }: { person: GridParticipant; speaking: boolean }) {
  const [broken, setBroken] = useState(false);
  const initial = (person.name || "?").slice(0, 1).toUpperCase();
  const showImage = Boolean(person.avatar) && !broken;
  return (
    <div
      className={`relative flex min-h-[140px] flex-col items-center justify-center gap-3 overflow-hidden rounded-lg bg-[#141414] transition-[box-shadow] ${
        speaking ? "shadow-[0_0_0_2px_rgba(87,242,135,0.85)]" : "ring-1 ring-white/8"
      }`}
    >
      <div className="flex h-20 w-20 items-center justify-center overflow-hidden rounded-full bg-[#1c1c1c] text-2xl font-medium text-cream">
        {showImage ? (
          <img
            src={person.avatar!}
            alt=""
            className="h-full w-full object-cover"
            onError={() => setBroken(true)}
          />
        ) : (
          initial
        )}
      </div>
      <div className="px-3 text-center">
        <p className="truncate text-sm font-medium">
          {person.name}
          {person.self ? <span className="text-mute"> (you)</span> : null}
        </p>
        <p className="text-[11px] text-mute">
          {person.deafened ? "Deafened" : person.muted ? "Muted" : speaking ? "Speaking" : "In call"}
        </p>
      </div>
    </div>
  );
}

function mergeParticipants(
  members: VoiceMember[],
  user: { id: string; name: string; avatar?: string | null } | null,
  muted: boolean,
  deafened: boolean,
  recipientIds: string[],
  users: Record<string, { id: string; name: string; avatar?: string | null }>,
): GridParticipant[] {
  const byId = new Map<string, GridParticipant>();
  for (const member of members) {
    byId.set(member.id, { ...member, self: user?.id === member.id });
  }
  for (const recipientId of recipientIds) {
    if (!recipientId || byId.has(recipientId)) continue;
    const person = users[recipientId];
    byId.set(recipientId, {
      id: recipientId,
      name: person?.name ?? `user ${recipientId.slice(-4)}`,
      avatar: person?.avatar ?? null,
      muted: false,
      deafened: false,
      self: false,
    });
  }
  if (user?.id && !byId.has(user.id)) {
    byId.set(user.id, {
      id: user.id,
      name: user.name,
      avatar: user.avatar,
      muted,
      deafened,
      self: true,
    });
  } else if (user?.id) {
    const self = byId.get(user.id)!;
    byId.set(user.id, { ...self, muted, deafened, self: true });
  }
  return Array.from(byId.values()).sort(
    (a, b) => Number(Boolean(b.self)) - Number(Boolean(a.self)) || a.name.localeCompare(b.name),
  );
}

function CallBarButton({
  label,
  onClick,
  active = false,
  danger = false,
}: {
  label: string;
  onClick: () => void;
  active?: boolean;
  danger?: boolean;
}) {
  return (
    <button
      type="button"
      title={label}
      aria-label={label}
      onClick={onClick}
      className={`shrink-0 rounded px-2.5 py-1.5 text-xs transition-all duration-150 ease-in-out ${
        danger
          ? "bg-[#3a1515] text-[#ffb4b4] hover:bg-[#4a1c1c]"
          : active
            ? "bg-white text-black"
            : "bg-white/10 text-cream hover:bg-white/15"
      }`}
    >
      {label}
    </button>
  );
}

export function ShareSheet() {
  const open = useApp((state) => state.shareOpen);
  const capture = useApp((state) => state.capture);
  const [sources, setSources] = useState<CaptureSource[]>([]);
  useEffect(() => {
    if (!open) return;
    void listCaptureSources().then(setSources).catch(() => setSources([]));
  }, [open]);
  useEffect(() => {
    if (!open) return;
    void setCapturePreview(Boolean(capture?.running)).catch(() => undefined);
  }, [open, capture?.running]);
  if (!open) return null;
  return (
    <Sheet title="Share screen" onClose={() => useApp.getState().setShareOpen(false)}>
      {capture?.running ? (
        <p className="text-sm text-mute">Screen share is live in the call view.</p>
      ) : (
        <p className="text-sm text-mute">Pick a display or window to share.</p>
      )}
      <div className="mt-3 max-h-[min(50vh,320px)] overflow-auto">
        {(sources ?? []).map((source) => (
          <button
            key={source.id}
            className="flex w-full items-center justify-between rounded px-2 py-2 text-left text-sm transition-all duration-150 ease-in-out hover:bg-white/5"
            onClick={() => {
              void startCapture(source.id);
              useApp.getState().setShareOpen(false);
            }}
          >
            <span className="truncate pr-3">{source.name}</span>
            <span className="shrink-0 text-mute">
              {source.kind} {source.width}x{source.height}
            </span>
          </button>
        ))}
      </div>
      {capture?.running ? (
        <button className="mt-3 text-sm text-ember" onClick={() => void stopCapture()}>
          Stop capture
        </button>
      ) : null}
    </Sheet>
  );
}

function WebGLCapturePreview({
  className,
  onDisplayFps,
}: {
  className?: string;
  onDisplayFps?: (fps: number) => void;
}) {
  const canvas = useRef<HTMLCanvasElement>(null);
  const latest = useRef<CaptureFrame | null>(null);
  const rgbPixels = useRef<Uint8Array | null>(null);
  const rgbSize = useRef({ w: 0, h: 0 });
  const dirty = useRef(false);
  const texSize = useRef({ w: 0, h: 0 });

  useEffect(() => {
    const node = canvas.current;
    const gl = node?.getContext("webgl2", {
      antialias: false,
      powerPreference: "high-performance",
      preserveDrawingBuffer: false,
      desynchronized: true,
    })
      ?? node?.getContext("webgl", {
        antialias: false,
        powerPreference: "high-performance",
        preserveDrawingBuffer: false,
        desynchronized: true,
      });
    if (!node || !gl) return;
    const program = makePreviewProgram(gl);
    const texture = gl.createTexture();
    const buffer = gl.createBuffer();
    if (!program || !texture || !buffer) return;
    gl.bindBuffer(gl.ARRAY_BUFFER, buffer);
    gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1, -1, 1, -1, -1, 1, 1, 1]), gl.STATIC_DRAW);
    const position = gl.getAttribLocation(program, "position");
    gl.bindTexture(gl.TEXTURE_2D, texture);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.LINEAR);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
    gl.pixelStorei(gl.UNPACK_ALIGNMENT, 1);
    let unlisten: (() => void) | undefined;
    let cancelled = false;
    let decoding = false;
    let decodeId = 0;
    let animation = 0;
    let drawn = 0;
    let fpsWindow = performance.now();
    const worker = createFrameDecodeWorker();
    const pumpDecode = () => {
      if (cancelled || decoding) return;
      const frame = latest.current;
      if (!frame) return;
      latest.current = null;
      decoding = true;
      const id = (decodeId += 1);
      const format = frame.format === "jpeg" ? "jpeg" : "rgb";
      const applyRgb = (pixels: Uint8Array, width: number, height: number) => {
        decoding = false;
        if (cancelled || id !== decodeId) return;
        rgbPixels.current = pixels;
        rgbSize.current = { w: width, h: height };
        dirty.current = true;
        if (node.width !== width || node.height !== height) {
          node.width = width;
          node.height = height;
        }
        if (latest.current) pumpDecode();
      };
      if (worker) {
        const onMessage = (
          event: MessageEvent<{ id: number; rgb?: Uint8Array; width?: number; height?: number; error?: string }>,
        ) => {
          if (event.data.id !== id) return;
          worker.removeEventListener("message", onMessage);
          if (event.data.rgb && event.data.width && event.data.height) {
            applyRgb(event.data.rgb, event.data.width, event.data.height);
          } else {
            decoding = false;
            if (latest.current) pumpDecode();
          }
        };
        worker.addEventListener("message", onMessage);
        worker.postMessage({ id, data: frame.pngBase64, format, width: frame.width, height: frame.height });
        return;
      }
      try {
        const binary = atob(frame.pngBase64);
        const bytes = new Uint8Array(binary.length);
        for (let i = 0; i < binary.length; i += 1) bytes[i] = binary.charCodeAt(i);
        applyRgb(bytes, frame.width, frame.height);
      } catch {
        decoding = false;
        if (latest.current) pumpDecode();
      }
    };
    void import("@tauri-apps/api/event").then(({ listen }) =>
      listen<CaptureFrame>("suncord://capture-frame", (event) => {
        latest.current = event.payload;
        pumpDecode();
      }).then((stop) => {
        if (cancelled) stop();
        else unlisten = stop;
      }),
    );
    const draw = () => {
      const pixels = rgbPixels.current;
      const size = rgbSize.current;
      if (pixels && dirty.current && size.w > 0 && size.h > 0) {
        dirty.current = false;
        gl.viewport(0, 0, node.width, node.height);
        gl.useProgram(program);
        gl.bindBuffer(gl.ARRAY_BUFFER, buffer);
        gl.enableVertexAttribArray(position);
        gl.vertexAttribPointer(position, 2, gl.FLOAT, false, 0, 0);
        gl.bindTexture(gl.TEXTURE_2D, texture);
        // Direct RGB upload — no JPEG decode / ImageBitmap realloc stutter.
        if (texSize.current.w === size.w && texSize.current.h === size.h) {
          gl.texSubImage2D(gl.TEXTURE_2D, 0, 0, 0, size.w, size.h, gl.RGB, gl.UNSIGNED_BYTE, pixels);
        } else {
          gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGB, size.w, size.h, 0, gl.RGB, gl.UNSIGNED_BYTE, pixels);
          texSize.current = { w: size.w, h: size.h };
        }
        gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
        drawn += 1;
        const now = performance.now();
        if (now - fpsWindow >= 1000) {
          onDisplayFps?.(drawn / ((now - fpsWindow) / 1000));
          drawn = 0;
          fpsWindow = now;
        }
      }
      animation = requestAnimationFrame(draw);
    };
    animation = requestAnimationFrame(draw);
    return () => {
      cancelled = true;
      cancelAnimationFrame(animation);
      unlisten?.();
      worker?.terminate();
      rgbPixels.current = null;
      gl.deleteTexture(texture);
      gl.deleteBuffer(buffer);
      gl.deleteProgram(program);
    };
  }, [onDisplayFps]);
  return <canvas ref={canvas} className={className ?? "h-full w-full object-contain"} aria-label="Screen share preview" />;
}

function createFrameDecodeWorker(): Worker | null {
  if (typeof Worker === "undefined") return null;
  try {
    const source = `
      self.onmessage = async (event) => {
        const { id, data, format, width, height } = event.data;
        try {
          const binary = atob(data);
          const bytes = new Uint8Array(binary.length);
          for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
          if (format === "jpeg") {
            const bitmap = await createImageBitmap(new Blob([bytes], { type: "image/jpeg" }), {
              premultiplyAlpha: "none",
              colorSpaceConversion: "none",
            });
            const canvas = new OffscreenCanvas(bitmap.width, bitmap.height);
            const ctx = canvas.getContext("2d");
            if (!ctx) throw new Error("no 2d");
            ctx.drawImage(bitmap, 0, 0);
            const image = ctx.getImageData(0, 0, bitmap.width, bitmap.height);
            const rgb = new Uint8Array(bitmap.width * bitmap.height * 3);
            for (let i = 0, j = 0; i < image.data.length; i += 4, j += 3) {
              rgb[j] = image.data[i];
              rgb[j + 1] = image.data[i + 1];
              rgb[j + 2] = image.data[i + 2];
            }
            bitmap.close();
            self.postMessage({ id, rgb, width: bitmap.width, height: bitmap.height }, [rgb.buffer]);
            return;
          }
          self.postMessage({ id, rgb: bytes, width, height }, [bytes.buffer]);
        } catch (error) {
          self.postMessage({ id, error: String(error) });
        }
      };
    `;
    return new Worker(URL.createObjectURL(new Blob([source], { type: "application/javascript" })));
  } catch {
    return null;
  }
}

function makePreviewProgram(gl: WebGLRenderingContext | WebGL2RenderingContext): WebGLProgram | null {
  const compile = (type: number, source: string) => {
    const shader = gl.createShader(type);
    if (!shader) return null;
    gl.shaderSource(shader, source);
    gl.compileShader(shader);
    if (gl.getShaderParameter(shader, gl.COMPILE_STATUS)) return shader;
    gl.deleteShader(shader);
    return null;
  };
  const vertex = compile(gl.VERTEX_SHADER, "attribute vec2 position; varying vec2 uv; void main(){ uv=(position+1.0)*.5; gl_Position=vec4(position,0.0,1.0); }");
  const fragment = compile(gl.FRAGMENT_SHADER, "precision mediump float; varying vec2 uv; uniform sampler2D tex; void main(){ gl_FragColor=texture2D(tex,vec2(uv.x,1.0-uv.y)); }");
  if (!vertex || !fragment) return null;
  const program = gl.createProgram();
  if (!program) return null;
  gl.attachShader(program, vertex);
  gl.attachShader(program, fragment);
  gl.linkProgram(program);
  gl.deleteShader(vertex);
  gl.deleteShader(fragment);
  return gl.getProgramParameter(program, gl.LINK_STATUS) ? program : null;
}

function Sheet({ title, onClose, children }: { title: string; onClose: () => void; children: ReactNode }) {
  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/50 p-4 transition-all duration-150 ease-in-out"
      onMouseDown={onClose}
    >
      <form
        className="flex max-h-[min(80vh,640px)] w-full max-w-md flex-col overflow-hidden rounded-lg border border-line bg-side p-4 shadow-xl transition-all duration-150 ease-in-out"
        onMouseDown={(event) => event.stopPropagation()}
        onSubmit={(event: FormEvent) => event.preventDefault()}
      >
        <div className="mb-4 flex shrink-0 items-center justify-between">
          <h2 className="text-sm font-semibold">{title}</h2>
          <button
            type="button"
            title="Close"
            onClick={onClose}
            className="rounded px-2 py-1 text-mute transition-all duration-150 ease-in-out hover:bg-white/10 hover:text-cream"
          >
            Close
          </button>
        </div>
        <div className="min-h-0 flex-1 overflow-auto">{children}</div>
      </form>
    </div>
  );
}
