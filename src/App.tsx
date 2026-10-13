import { Component, FormEvent, useEffect, useMemo, useState } from "react";
import { CallOverlay, ShareSheet } from "./components/Call";
import { TitleBar } from "./components/TitleBar";
import { Workspace } from "./components/Workspace";
import { listAudioDevices, logout, startLogin, startSession, updateSettings, useBackend } from "./lib/backend";
import { useApp } from "./state/store";
import { AudioDevice, isTextChannel, Settings } from "./types";
import type { ErrorInfo, ReactNode } from "react";

export function App() {
  useBackend();
  const booted = useApp((state) => state.booted);
  const signedIn = useApp((state) => state.signedIn);
  const phase = useApp((state) => state.phase);
  const user = useApp((state) => state.user);
  const error = useApp((state) => state.error);
  const voice = useApp((state) => state.voice);
  const capture = useApp((state) => state.capture);
  const callActive = (Boolean(voice?.channelId) && voice?.phase !== "idle") || Boolean(capture?.running);

  useEffect(() => {
    startSession();
  }, []);

  useEffect(() => {
    const blockBrowserMenu = (event: MouseEvent) => event.preventDefault();
    window.addEventListener("contextmenu", blockBrowserMenu);
    return () => window.removeEventListener("contextmenu", blockBrowserMenu);
  }, []);

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "k") {
        event.preventDefault();
        useApp.getState().toggleSwitcher();
      }
      if (event.key === "Escape") {
        useApp.getState().setSwitcher(false);
        useApp.getState().setSettingsOpen(false);
        useApp.getState().setShareOpen(false);
        useApp.setState({ lightboxUrl: null });
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  let body: ReactNode;
  if (!booted) {
    body = <StatusPane title="Starting" detail="Loading the local session." />;
  } else if (!signedIn) {
    body = <Login />;
  } else if (!user || phase === "connecting" || phase === "reconnecting") {
    body = (
      <StatusPane
        title={phase === "reconnecting" ? "Reconnecting" : "Connecting"}
        detail={error || "Validating your token and opening the gateway."}
        busy={!error}
        action={
          error ? (
            <button
              className="mt-4 rounded bg-white px-4 py-2 text-sm font-medium text-black"
              onClick={() => {
                useApp.getState().clearError();
                void logout();
              }}
            >
              Back to sign in
            </button>
          ) : null
        }
      />
    );
  } else {
    body = (
      <ErrorBoundary
        fallback={
          <StatusPane
            title="Something went wrong"
            detail="The workspace failed to render. Try signing out and back in."
            busy={false}
            action={
              <button
                className="mt-4 rounded bg-white px-4 py-2 text-sm font-medium text-black"
                onClick={() => void logout()}
              >
                Sign out
              </button>
            }
          />
        }
      >
        <Workspace />
      </ErrorBoundary>
    );
  }

  return (
    <div className="flex h-full flex-col bg-black text-cream" onContextMenu={(event) => event.preventDefault()}>
      <TitleBar />
      <div className={`relative z-0 min-h-0 flex-1 bg-ink ${callActive ? "pt-11" : ""}`}>
        {body}
        <CallOverlay />
      </div>
      <QuickSwitcher />
      <SettingsSheet />
      <ShareSheet />
    </div>
  );
}

function StatusPane({
  title,
  detail,
  action,
  busy = true,
}: {
  title: string;
  detail: string;
  action?: ReactNode;
  busy?: boolean;
}) {
  return (
    <div className="flex h-full flex-col items-center justify-center gap-3 px-6 text-center">
      {busy ? (
        <div
          className="h-5 w-5 animate-spin rounded-full border-2 border-white/20 border-t-white"
          aria-hidden
        />
      ) : null}
      <p className="text-sm font-semibold">{title}</p>
      <p className="max-w-sm text-sm text-mute">{detail}</p>
      {action}
    </div>
  );
}

class ErrorBoundary extends Component<{ children: ReactNode; fallback: ReactNode }, { failed: boolean }> {
  state = { failed: false };

  static getDerivedStateFromError(): { failed: boolean } {
    return { failed: true };
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    console.error("workspace render failed", error, info);
  }

  render() {
    if (this.state.failed) return this.props.fallback;
    return this.props.children;
  }
}

function Login() {
  const [busy, setBusy] = useState(false);
  const [localError, setLocalError] = useState("");
  const signedIn = useApp((state) => state.signedIn);
  const storeError = useApp((state) => state.error);

  useEffect(() => {
    if (signedIn) {
      setBusy(false);
      setLocalError("");
      useApp.getState().clearError();
    }
  }, [signedIn]);

  useEffect(() => {
    let stop: (() => void) | undefined;
    let alive = true;
    void (async () => {
      try {
        const { listen } = await import("@tauri-apps/api/event");
        if (!alive) return;
        stop = await listen("suncord://login-closed", () => setBusy(false));
      } catch {
        /* browser preview has no Tauri events */
      }
    })();
    return () => {
      alive = false;
      stop?.();
    };
  }, []);

  return (
    <div className="flex h-full flex-col items-center justify-center gap-3 bg-black px-6">
      <button
        className="rounded bg-white px-4 py-2 text-sm font-medium text-black disabled:opacity-60"
        disabled={busy}
        onClick={() => {
          setBusy(true);
          setLocalError("");
          const timeout = window.setTimeout(() => {
            setBusy(false);
            setLocalError("Sign-in timed out. Try again.");
          }, 20_000);
          void startLogin()
            .then(() => {
              window.clearTimeout(timeout);
              setBusy(false);
            })
            .catch((error: unknown) => {
              window.clearTimeout(timeout);
              const message =
                typeof error === "string"
                  ? error
                  : error instanceof Error
                    ? error.message
                    : "Could not open sign-in.";
              setLocalError(message);
              setBusy(false);
            });
        }}
      >
        {busy ? "Opening..." : "Sign in"}
      </button>
      {localError || storeError ? (
        <p className="max-w-sm text-center text-sm text-ember">{localError || storeError}</p>
      ) : null}
    </div>
  );
}

function QuickSwitcher() {
  const open = useApp((state) => state.switcherOpen);
  const guilds = useApp((state) => state.guilds) ?? [];
  const channels = useApp((state) => state.channels) ?? [];
  const dms = useApp((state) => state.dms) ?? [];
  const [query, setQuery] = useState("");
  const [cursor, setCursor] = useState(0);
  const items = useMemo(() => {
    const needle = query.trim().toLowerCase();
    const rows = [
      ...dms.map((channel) => ({
        id: channel.id,
        guildId: null as string | null,
        label: channel.name || "Direct message",
        hint: "Direct message",
      })),
      ...channels
        .filter((channel) => isTextChannel(channel.kind) || channel.kind === 2)
        .map((channel) => ({
          id: channel.id,
          guildId: channel.guildId ?? null,
          label: channel.name || "channel",
          hint: guilds.find((guild) => guild.id === channel.guildId)?.name ?? "Server",
        })),
    ];
    return needle
      ? rows.filter((row) => `${row.label} ${row.hint}`.toLowerCase().includes(needle))
      : rows.slice(0, 40);
  }, [channels, dms, guilds, query]);

  useEffect(() => {
    setCursor(0);
  }, [query, open]);

  if (!open) return null;
  return (
    <div
      className="absolute inset-0 z-40 flex items-start justify-center bg-black/50 pt-24"
      onMouseDown={() => useApp.getState().setSwitcher(false)}
    >
      <div
        className="w-[520px] overflow-hidden rounded-lg border border-line bg-panel shadow-none"
        onMouseDown={(event) => event.stopPropagation()}
      >
        <input
          autoFocus
          value={query}
          placeholder="Jump to a channel"
          className="w-full bg-transparent px-4 py-3 outline-none"
          onChange={(event) => setQuery(event.target.value)}
          onKeyDown={(event) => {
            if (event.key === "ArrowDown") {
              event.preventDefault();
              setCursor((value) => Math.min(value + 1, Math.max(items.length - 1, 0)));
            }
            if (event.key === "ArrowUp") {
              event.preventDefault();
              setCursor((value) => Math.max(value - 1, 0));
            }
            if (event.key === "Enter" && items[cursor]) {
              const item = items[cursor];
              useApp.getState().setPlace(item.guildId, item.id);
              useApp.getState().setSwitcher(false);
              void import("./lib/backend").then((backend) => backend.selectChannel(item.id, item.guildId));
            }
          }}
        />
        <div className="max-h-80 overflow-auto border-t border-line">
          {items.length === 0 ? (
            <p className="px-4 py-3 text-sm text-mute">No channels yet.</p>
          ) : (
            items.map((item, index) => (
              <button
                key={`${item.guildId ?? "dm"}-${item.id}`}
                className={`flex w-full items-center justify-between px-4 py-2 text-left text-sm ${index === cursor ? "bg-white/5" : ""}`}
                onMouseEnter={() => setCursor(index)}
                onClick={() => {
                  useApp.getState().setPlace(item.guildId, item.id);
                  useApp.getState().setSwitcher(false);
                  void import("./lib/backend").then((backend) =>
                    backend
                      .selectChannel(item.id, item.guildId)
                      .then((rows) => useApp.getState().replaceMessages(item.id, rows))
                      .catch(() => undefined),
                  );
                }}
              >
                <span>{item.label}</span>
                <span className="text-mute">{item.hint}</span>
              </button>
            ))
          )}
        </div>
      </div>
    </div>
  );
}

function SettingsSheet() {
  const open = useApp((state) => state.settingsOpen);
  const settings = useApp((state) => state.settings) ?? {
    bufferFrames: 480,
    hardwareAccel: true,
    adaptiveThrottle: false,
    captureFps: 60,
    hwPreference: "gpu",
    asrEnabled: false,
    asrModelPath: "",
    inputDevice: "",
    outputDevice: "",
  };
  const [devices, setDevices] = useState<AudioDevice[]>([]);
  useEffect(() => {
    if (!open) return;
    void listAudioDevices().then(setDevices).catch(() => setDevices([]));
  }, [open]);
  if (!open) return null;
  const save = (patch: Partial<Settings>) => {
    useApp.getState().patchSettings(patch);
    void updateSettings(patch).then((next) => useApp.getState().patchSettings(next));
  };
  return (
    <Sheet title="Audio and capture" onClose={() => useApp.getState().setSettingsOpen(false)}>
      <label className="block text-sm">
        <span className="text-mute">Input</span>
        <select
          className="mt-1 w-full rounded bg-ink px-2 py-2"
          value={settings.inputDevice}
          onChange={(event) => save({ inputDevice: event.target.value })}
        >
          <option value="">System default</option>
          {devices
            .filter((device) => device.input)
            .map((device) => (
              <option key={device.name} value={device.name}>
                {device.name}
              </option>
            ))}
        </select>
      </label>
      <label className="mt-3 block text-sm">
        <span className="text-mute">Buffer</span>
        <input
          type="number"
          min={128}
          max={4800}
          className="mt-1 w-full rounded bg-ink px-2 py-2"
          value={settings.bufferFrames}
          onChange={(event) => save({ bufferFrames: Number(event.target.value) })}
        />
      </label>
      <Check label="Hardware acceleration" checked={settings.hardwareAccel} onChange={(hardwareAccel) => save({ hardwareAccel })} />
      <Check label="Hold target quality" checked={!settings.adaptiveThrottle} onChange={(hold) => save({ adaptiveThrottle: !hold })} />
      <label className="mt-3 block text-sm">
        <span className="text-mute">Capture fps</span>
        <input
          type="number"
          min={15}
          max={240}
          className="mt-1 w-full rounded bg-ink px-2 py-2"
          value={settings.captureFps}
          onChange={(event) => save({ captureFps: Number(event.target.value) })}
        />
      </label>
      <label className="mt-3 block text-sm">
        <span className="text-mute">Capture path</span>
        <select
          className="mt-1 w-full rounded bg-ink px-2 py-2"
          value={settings.hwPreference}
          onChange={(event) => save({ hwPreference: event.target.value })}
        >
          <option value="gpu">Native</option>
          <option value="auto">Automatic</option>
          <option value="cpu">Scaled</option>
        </select>
      </label>
      <label className="mt-3 block text-sm">
        <span className="text-mute">Local ASR model path</span>
        <input
          className="mt-1 w-full rounded bg-ink px-2 py-2"
          value={settings.asrModelPath}
          placeholder="C:\models\ggml-base.bin"
          onChange={(event) => save({ asrModelPath: event.target.value })}
        />
      </label>
      <button className="mt-5 text-sm text-ember" onClick={() => void logout()}>
        Sign out
      </button>
    </Sheet>
  );
}

function Check({ label, checked, onChange }: { label: string; checked: boolean; onChange: (value: boolean) => void }) {
  return (
    <label className="mt-3 flex items-center gap-2 text-sm">
      <input type="checkbox" checked={checked} onChange={(event) => onChange(event.target.checked)} />
      {label}
    </label>
  );
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
