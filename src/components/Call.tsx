import { FormEvent, useEffect, useRef, useState, type ReactNode } from "react";
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
import { CaptureFrame, CaptureSource } from "../types";

export function CallOverlay() {
  const voice = useApp((state) => state.voice);
  const capture = useApp((state) => state.capture);
  const inVoice = Boolean(voice.channelId) && voice.phase !== "idle";
  const sharing = Boolean(capture?.running);
  const [expanded, setExpanded] = useState(true);

  useEffect(() => {
    const enabled = sharing && (expanded || inVoice);
    void setCapturePreview(enabled).catch(() => undefined);
    return () => {
      if (!useApp.getState().shareOpen) void setCapturePreview(false).catch(() => undefined);
    };
  }, [sharing, expanded, inVoice]);

  if (!inVoice && !sharing) return null;

  return (
    <>
      <div className="pointer-events-auto absolute left-0 right-0 top-0 z-20 flex h-11 items-center gap-2 bg-[#0c0c0c] px-3 shadow-[0_1px_0_0_rgba(255,255,255,0.06)]">
        <span className="min-w-0 flex-1 truncate text-sm">
          {inVoice ? voice.channelName || "Voice call" : "Screen share"}
          <span className="ml-2 text-xs text-mute">
            {inVoice ? voice.detail : sharing ? `${capture.width}×${capture.height} · ${capture.fps.toFixed(0)} fps` : ""}
          </span>
        </span>
        {inVoice ? (
          <>
            <CallBarButton label={voice.muted ? "Unmute" : "Mute"} active={Boolean(voice.muted)} onClick={() => void setMuted(!voice.muted)} />
            <CallBarButton label={voice.deafened ? "Undeafen" : "Deafen"} active={Boolean(voice.deafened)} onClick={() => void setDeafened(!voice.deafened)} />
            <CallBarButton label="Share" active={sharing} onClick={() => useApp.getState().setShareOpen(true)} />
            <CallBarButton label="Leave" danger onClick={() => void leaveVoice()} />
          </>
        ) : null}
        {sharing ? (
          <>
            <CallBarButton label={expanded ? "Collapse" : "Expand"} onClick={() => setExpanded((value) => !value)} />
            <CallBarButton label="Stop share" danger onClick={() => void stopCapture()} />
          </>
        ) : null}
      </div>
      {sharing && expanded ? (
        <div className="pointer-events-auto absolute inset-x-0 bottom-0 top-11 z-20 flex items-center justify-center bg-black/90 p-4">
          <WebGLCapturePreview className="h-full max-h-full w-full max-w-6xl object-contain" />
        </div>
      ) : null}
    </>
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
      onClick={onClick}
      className={`shrink-0 rounded px-2.5 py-1 text-xs ${
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
        <p className="text-sm text-mute">
          Streaming {capture.width}x{capture.height} at {capture.fps.toFixed(0)} fps — preview is in the call overlay.
        </p>
      ) : (
        <p className="text-sm text-mute">Pick a display or window to capture at 60 FPS.</p>
      )}
      <div className="mt-3 max-h-72 overflow-auto">
        {(sources ?? []).map((source) => (
          <button
            key={source.id}
            className="flex w-full items-center justify-between rounded px-2 py-2 text-left text-sm hover:bg-white/5"
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

function WebGLCapturePreview({ className }: { className?: string }) {
  const canvas = useRef<HTMLCanvasElement>(null);
  const latest = useRef<CaptureFrame | null>(null);
  const bitmap = useRef<ImageBitmap | null>(null);

  useEffect(() => {
    const node = canvas.current;
    const gl = node?.getContext("webgl2") ?? node?.getContext("webgl");
    if (!node || !gl) return;
    const program = makePreviewProgram(gl);
    const texture = gl.createTexture();
    const buffer = gl.createBuffer();
    if (!program || !texture || !buffer) return;
    gl.bindBuffer(gl.ARRAY_BUFFER, buffer);
    gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1, -1, 1, -1, -1, 1, 1, 1]), gl.STATIC_DRAW);
    const position = gl.getAttribLocation(program, "position");
    let unlisten: (() => void) | undefined;
    let cancelled = false;
    let decoding = false;
    let animation = 0;
    void import("@tauri-apps/api/event").then(({ listen }) =>
      listen<CaptureFrame>("suncord://capture-frame", (event) => {
        latest.current = event.payload;
      }).then((stop) => {
        if (cancelled) stop();
        else unlisten = stop;
      }),
    );
    const draw = () => {
      const frame = latest.current;
      if (frame && !decoding) {
        latest.current = null;
        decoding = true;
        decodePng(frame.pngBase64)
          .then((next) => {
            decoding = false;
            if (cancelled) return next.close();
            bitmap.current?.close();
            bitmap.current = next;
            if (node.width !== next.width || node.height !== next.height) {
              node.width = next.width;
              node.height = next.height;
            }
          })
          .catch(() => {
            decoding = false;
          });
      }
      const image = bitmap.current;
      if (image) {
        gl.viewport(0, 0, node.width, node.height);
        gl.useProgram(program);
        gl.bindBuffer(gl.ARRAY_BUFFER, buffer);
        gl.enableVertexAttribArray(position);
        gl.vertexAttribPointer(position, 2, gl.FLOAT, false, 0, 0);
        gl.bindTexture(gl.TEXTURE_2D, texture);
        gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR);
        gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.LINEAR);
        gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, gl.RGBA, gl.UNSIGNED_BYTE, image);
        gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
      }
      animation = requestAnimationFrame(draw);
    };
    animation = requestAnimationFrame(draw);
    return () => {
      cancelled = true;
      cancelAnimationFrame(animation);
      unlisten?.();
      bitmap.current?.close();
      bitmap.current = null;
      gl.deleteTexture(texture);
      gl.deleteBuffer(buffer);
      gl.deleteProgram(program);
    };
  }, []);
  return <canvas ref={canvas} className={className ?? "h-full w-full object-contain"} aria-label="Screen share preview" />;
}

function decodePng(data: string): Promise<ImageBitmap> {
  const bytes = Uint8Array.from(atob(data), (char) => char.charCodeAt(0));
  return createImageBitmap(new Blob([bytes], { type: "image/png" }));
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
    <div className="absolute inset-0 z-40 flex justify-end bg-black/40" onMouseDown={onClose}>
      <form
        className="h-full w-[360px] overflow-auto border-l border-line bg-side p-4"
        onMouseDown={(event) => event.stopPropagation()}
        onSubmit={(event: FormEvent) => event.preventDefault()}
      >
        <div className="mb-4 flex items-center justify-between">
          <h2 className="text-sm font-semibold">{title}</h2>
          <button type="button" onClick={onClose} className="text-mute">
            Close
          </button>
        </div>
        {children}
      </form>
    </div>
  );
}
