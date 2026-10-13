import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { useEffect } from "react";
import { useApp } from "../state/store";
import {
  AsrStatus,
  AudioDevice,
  AuthEvent,
  Bootstrap,
  CaptureSource,
  CaptureStats,
  ChatMessage,
  HistoryEvent,
  MessageDeleteEvent,
  Note,
  Settings,
  Snapshot,
  StatusEvent,
  VoiceStatus,
} from "../types";

export function inTauri(): boolean {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

export async function loadBootstrap(): Promise<void> {
  if (!inTauri()) {
    useApp.setState({ booted: true, signedIn: false });
    return;
  }
  try {
    const boot = await invoke<Bootstrap>("bootstrap");
    useApp.getState().applyBootstrap(boot);
  } catch (error) {
    useApp.setState({ booted: true, signedIn: false });
    useApp.getState().setError(error instanceof Error ? error.message : "Could not load the saved session.");
  }
}

let sessionStarted = false;

/** Kept for App mount; real bootstrap runs inside useBackend after listeners bind. */
export function startSession(): void {
  sessionStarted = true;
}

export function useBackend(): void {
  useEffect(() => {
    if (!inTauri()) {
      void loadBootstrap();
      return;
    }
    let alive = true;
    const stops: Array<() => void> = [];
    void (async () => {
      const bindings = await Promise.all([
        listen<StatusEvent>("suncord://status", (event) => {
          useApp.getState().setStatus(event.payload.phase, event.payload.detail);
        }),
        listen<AuthEvent>("suncord://auth", (event) => {
          const signedIn = Boolean(event.payload?.signedIn);
          if (signedIn) {
            useApp.getState().setSignedIn(true);
            useApp.getState().setStatus("connecting", "Opening the gateway");
          } else {
            useApp.getState().resetSession();
          }
        }),
        listen<Snapshot>("suncord://snapshot", (event) => {
          useApp.getState().applySnapshot(event.payload);
          void refreshDms().catch(() => undefined);
        }),
        listen<ChatMessage>("suncord://message", (event) => {
          useApp.getState().pushMessage(event.payload);
        }),
        listen<HistoryEvent>("suncord://history", (event) => {
          useApp.getState().replaceMessages(event.payload.channelId, event.payload.messages);
        }),
        listen<MessageDeleteEvent>("suncord://message-delete", (event) => {
          useApp.getState().removeMessages(event.payload.channelId, event.payload.messageIds);
        }),
        listen<VoiceStatus>("suncord://voice", (event) => {
          useApp.getState().setVoice(event.payload);
        }),
        listen<CaptureStats>("suncord://capture", (event) => {
          useApp.getState().setCapture(event.payload);
        }),
        listen<AsrStatus>("suncord://asr", (event) => {
          useApp.getState().setAsr(event.payload);
        }),
        listen<Note>("suncord://error", (event) => {
          useApp.getState().setError(event.payload.message);
        }),
      ]);
      if (!alive) {
        bindings.forEach((stop) => stop());
        return;
      }
      stops.push(...bindings);
      if (!sessionStarted) sessionStarted = true;
      await loadBootstrap();
    })();

    const syncActiveChannel = () => {
      const { signedIn, activeChannelId, activeGuildId, voice } = useApp.getState();
      if (!signedIn || !activeChannelId) return;
      // Avoid yanking message state while a call overlay is live — focus clicks
      // during a call otherwise race REST history against gateway MESSAGE_CREATE.
      if (voice?.channelId && voice.phase !== "idle") return;
      const channelId = activeChannelId;
      void selectChannel(channelId, activeGuildId)
        .then((rows) => {
          if (useApp.getState().activeChannelId !== channelId) return;
          useApp.getState().replaceMessages(channelId, rows);
        })
        .catch(() => undefined);
    };
    const onFocus = () => syncActiveChannel();
    const onVisibility = () => {
      if (document.visibilityState === "visible") syncActiveChannel();
    };
    window.addEventListener("focus", onFocus);
    document.addEventListener("visibilitychange", onVisibility);

    return () => {
      alive = false;
      stops.forEach((stop) => stop());
      window.removeEventListener("focus", onFocus);
      document.removeEventListener("visibilitychange", onVisibility);
    };
  }, []);
}

export function selectChannel(channelId: string, guildId: string | null): Promise<ChatMessage[]> {
  return invoke<ChatMessage[]>("select_channel", { channelId, guildId });
}

export function sendMessage(channelId: string, content: string, nonce: string): Promise<ChatMessage> {
  return invoke<ChatMessage>("send_message", { channelId, content, nonce });
}

export function editMessage(channelId: string, messageId: string, content: string): Promise<ChatMessage> {
  return invoke<ChatMessage>("edit_message", { channelId, messageId, content });
}

export function deleteMessage(channelId: string, messageId: string): Promise<void> {
  return invoke("delete_message", { channelId, messageId });
}

export function addReaction(channelId: string, messageId: string, emoji: string): Promise<void> {
  return invoke("add_reaction", { channelId, messageId, emoji });
}

export async function refreshDms(): Promise<void> {
  const dms = await invoke<import("../types").Channel[]>("refresh_dms");
  useApp.setState({ dms: dms ?? [] });
}

export function channelPins(channelId: string): Promise<ChatMessage[]> {
  return invoke<ChatMessage[]>("channel_pins", { channelId });
}

export function openExternal(url: string): void {
  // Prefer the shell plugin so the browser opens without blocking the UI thread.
  void import("@tauri-apps/plugin-shell")
    .then(({ open }) => open(url))
    .catch(() => invoke("open_external", { url }));
}

export function startLogin(): Promise<void> {
  return invoke("auth_start_login");
}

export async function logout(): Promise<void> {
  useApp.getState().resetSession();
  try {
    await invoke("auth_logout");
  } catch (error) {
    useApp.getState().resetSession();
    useApp.getState().setError(
      error instanceof Error ? error.message : "Could not clear the saved session from the keychain.",
    );
    throw error;
  }
  useApp.getState().resetSession();
}

export function joinVoice(channelId: string, guildId: string | null): Promise<void> {
  const state = useApp.getState();
  const voice = state.voice;
  const channel =
    [...(state.channels ?? []), ...(state.dms ?? [])].find((item) => item.id === channelId) ?? null;
  const users = state.users ?? {};
  const self = state.user;
  const seeded = new Map<string, { id: string; name: string; avatar?: string | null; muted: boolean; deafened: boolean }>();
  if (self?.id) {
    seeded.set(self.id, {
      id: self.id,
      name: self.name,
      avatar: self.avatar,
      muted: Boolean(voice.muted),
      deafened: Boolean(voice.deafened),
    });
  }
  for (const recipientId of channel?.recipientIds ?? []) {
    if (!recipientId || seeded.has(recipientId)) continue;
    const person = users[recipientId];
    seeded.set(recipientId, {
      id: recipientId,
      name: person?.name ?? `user ${recipientId.slice(-4)}`,
      avatar: person?.avatar ?? null,
      muted: false,
      deafened: false,
    });
  }
  useApp.getState().setVoice({
    ...voice,
    phase: "joining",
    channelId,
    channelName: channel?.name ?? voice.channelName,
    detail: "Connecting…",
    members: Array.from(seeded.values()),
  });
  return invoke("voice_join", { channelId, guildId });
}

export function leaveVoice(): Promise<void> {
  return invoke("voice_leave");
}

export function setMuted(muted: boolean): Promise<void> {
  return invoke("voice_set_muted", { muted });
}

export function setDeafened(deafened: boolean): Promise<void> {
  return invoke("voice_set_deafened", { deafened });
}

export function listAudioDevices(): Promise<AudioDevice[]> {
  return invoke<AudioDevice[]>("voice_devices");
}

export function updateSettings(patch: Partial<Settings>): Promise<Settings> {
  return invoke<Settings>("settings_update", { patch });
}

export function listCaptureSources(): Promise<CaptureSource[]> {
  return invoke<CaptureSource[]>("capture_sources");
}

export function startCapture(id: string): Promise<void> {
  return invoke("capture_start", { id });
}

export function stopCapture(): Promise<void> {
  return invoke("capture_stop");
}

export function setCapturePreview(enabled: boolean): Promise<void> {
  return invoke("capture_set_preview", { enabled });
}

export async function windowControl(action: "minimize" | "toggle" | "close"): Promise<void> {
  if (!inTauri()) return;
  const window = getCurrentWindow();
  if (action === "minimize") await window.minimize();
  if (action === "toggle") await window.toggleMaximize();
  if (action === "close") await window.close();
}
