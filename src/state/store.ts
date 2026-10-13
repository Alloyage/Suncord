import { create } from "zustand";
import { sortDms } from "../lib/util";
import {
  AsrStatus,
  Bootstrap,
  CaptureStats,
  Channel,
  ChatMessage,
  defaultSettings,
  defaultVoice,
  Guild,
  GuildFolder,
  Phase,
  Settings,
  Snapshot,
  User,
  VoiceStatus,
} from "../types";

const emptyCapture: CaptureStats = {
  running: false,
  source: "",
  width: 0,
  height: 0,
  fps: 0,
  captureMs: 0,
  dropped: 0,
  path: "idle",
  bitrateKbps: 0,
};

const emptyAsr: AsrStatus = { enabled: false, detail: "Captions are off.", caption: "", level: 0 };

type SessionSlice = {
  user: User | null;
  guilds: Guild[];
  folders: GuildFolder[];
  channels: Channel[];
  dms: Channel[];
  messages: Record<string, ChatMessage[]>;
  users: Record<string, User>;
  activeGuildId: string | null;
  activeChannelId: string | null;
  lastDmId: string | null;
};

function clearSession(): SessionSlice & {
  phase: Phase;
  statusDetail: string;
  error: string;
  voice: VoiceStatus;
  capture: CaptureStats;
  asr: AsrStatus;
  switcherOpen: boolean;
  settingsOpen: boolean;
  shareOpen: boolean;
  lightboxUrl: string | null;
} {
  return {
    phase: "offline",
    statusDetail: "",
    error: "",
    user: null,
    guilds: [],
    folders: [],
    channels: [],
    dms: [],
    messages: {},
    users: {},
    activeGuildId: null,
    activeChannelId: null,
    lastDmId: null,
    voice: defaultVoice,
    capture: emptyCapture,
    asr: emptyAsr,
    switcherOpen: false,
    settingsOpen: false,
    shareOpen: false,
    lightboxUrl: null,
  };
}

interface AppStore extends SessionSlice {
  booted: boolean;
  signedIn: boolean;
  phase: Phase;
  statusDetail: string;
  error: string;
  settings: Settings;
  voice: VoiceStatus;
  capture: CaptureStats;
  asr: AsrStatus;
  switcherOpen: boolean;
  settingsOpen: boolean;
  shareOpen: boolean;
  lightboxUrl: string | null;
  applyBootstrap: (boot: Bootstrap) => void;
  setSignedIn: (signedIn: boolean) => void;
  resetSession: () => void;
  setStatus: (phase: Phase, detail?: string) => void;
  setError: (message: string) => void;
  clearError: () => void;
  applySnapshot: (snapshot: Snapshot) => void;
  setPlace: (guildId: string | null, channelId: string | null) => void;
  replaceMessages: (channelId: string, messages: ChatMessage[]) => void;
  pushMessage: (message: ChatMessage) => void;
  removeMessages: (channelId: string, messageIds: string[]) => void;
  patchSettings: (patch: Partial<Settings>) => void;
  setVoice: (voice: VoiceStatus) => void;
  setCapture: (capture: CaptureStats) => void;
  setAsr: (asr: AsrStatus) => void;
  toggleSwitcher: () => void;
  setSwitcher: (open: boolean) => void;
  setSettingsOpen: (open: boolean) => void;
  setShareOpen: (open: boolean) => void;
}

export const useApp = create<AppStore>((set) => ({
  booted: false,
  signedIn: false,
  settings: defaultSettings,
  ...clearSession(),

  applyBootstrap: (boot) => {
    const signedIn = Boolean(boot?.signedIn);
    if (!signedIn) {
      set({ booted: true, signedIn: false, settings: { ...defaultSettings, ...(boot?.settings ?? {}) }, ...clearSession() });
      return;
    }
    const snapshot = normalizeSnapshot(boot?.snapshot);
    set({
      booted: true,
      signedIn: true,
      phase: normalizePhase(boot?.phase),
      error: "",
      user: snapshot.user,
      guilds: snapshot.guilds,
      folders: snapshot.folders ?? [],
      channels: snapshot.channels,
      dms: snapshot.dms,
      messages: snapshot.messages,
      users: mergeUsers({}, snapshot.users, snapshot.user, snapshot.messages),
      activeGuildId: snapshot.activeGuildId ?? null,
      activeChannelId: snapshot.activeChannelId ?? null,
      lastDmId: snapshot.activeGuildId == null ? snapshot.activeChannelId ?? null : null,
      settings: { ...defaultSettings, ...(boot?.settings ?? {}) },
    });
  },

  setSignedIn: (signedIn) => set(signedIn ? { signedIn, error: "" } : { signedIn }),
  resetSession: () => set({ signedIn: false, ...clearSession() }),
  setStatus: (phase, statusDetail) =>
    set({ phase: normalizePhase(phase), statusDetail: statusDetail ?? "" }),
  setError: (error) => set({ error: error ?? "" }),
  clearError: () => set({ error: "" }),

  applySnapshot: (snapshot) =>
    set((state) => {
      const next = normalizeSnapshot(snapshot);
      const signedOut = !state.signedIn && !next.user;
      if (signedOut) return { ...clearSession(), signedIn: false };
      return {
        user: next.user ?? state.user,
        guilds: mergeGuilds(state.guilds, next.guilds),
        folders: next.folders?.length ? next.folders : state.folders,
        channels: next.channels.length ? next.channels : state.channels,
        dms: next.dms,
        messages: mergeMessages(state.messages, next.messages),
        users: mergeUsers(state.users, next.users, next.user ?? state.user, next.messages),
        activeGuildId: state.activeGuildId ?? next.activeGuildId ?? null,
        activeChannelId: state.activeChannelId ?? next.activeChannelId ?? null,
      };
    }),

  setPlace: (activeGuildId, activeChannelId) =>
    set((state) => ({
      activeGuildId,
      activeChannelId,
      lastDmId: activeGuildId == null && activeChannelId ? activeChannelId : state.lastDmId,
    })),

  replaceMessages: (channelId, messages) =>
    set((state) => ({
      messages: { ...state.messages, [channelId]: messages ?? [] },
      users: mergeUsers(state.users, undefined, undefined, { [channelId]: messages ?? [] }),
    })),

  pushMessage: (message) =>
    set((state) => ({
      messages: {
        ...state.messages,
        [message.channelId]: upsertMessage(state.messages[message.channelId] ?? [], message),
      },
      users: mergeUsers(state.users, undefined, {
        id: message.authorId,
        name: message.author,
        avatar: message.avatar,
      }),
    })),

  removeMessages: (channelId, messageIds) =>
    set((state) => {
      const removed = new Set(messageIds);
      const current = state.messages[channelId] ?? [];
      if (!current.some((message) => removed.has(message.id))) return state;
      return {
        messages: {
          ...state.messages,
          [channelId]: current.filter((message) => !removed.has(message.id)),
        },
      };
    }),

  patchSettings: (patch) => set((state) => ({ settings: { ...state.settings, ...patch } })),
  setVoice: (voice) =>
    set((state) => {
      const phase = voice.phase ?? state.voice.phase;
      const normalized =
        phase === "reconnecting" ? "joining" : phase === "connecting" ? "joining" : phase;
      return {
        voice: {
          ...state.voice,
          ...voice,
          phase: normalized,
          channelId: voice.channelId !== undefined ? voice.channelId : state.voice.channelId,
          channelName: voice.channelName !== undefined ? voice.channelName : state.voice.channelName,
          members: voice.members ?? state.voice.members,
        },
      };
    }),
  setCapture: (capture) => set({ capture }),
  setAsr: (asr) => set({ asr }),
  toggleSwitcher: () => set((state) => ({ switcherOpen: !state.switcherOpen })),
  setSwitcher: (switcherOpen) => set({ switcherOpen }),
  setSettingsOpen: (settingsOpen) => set({ settingsOpen }),
  setShareOpen: (shareOpen) => set({ shareOpen }),
}));

function normalizePhase(phase: unknown): Phase {
  if (phase === "connecting" || phase === "ready" || phase === "reconnecting" || phase === "offline") return phase;
  return "offline";
}

function mergeGuilds(previous: Guild[], incoming: Guild[]): Guild[] {
  if (!incoming.length) return previous;
  const byId = new Map(previous.map((guild) => [guild.id, guild]));
  return incoming.map((guild) => {
    const old = byId.get(guild.id);
    if (!old) return guild;
    return {
      ...guild,
      name: guild.name?.trim() ? guild.name : old.name,
      icon: guild.icon ?? old.icon ?? null,
    };
  });
}

function normalizeSnapshot(snapshot: Snapshot | null | undefined): Snapshot {
  const guilds = (snapshot?.guilds ?? []).filter(Boolean).map((guild) => ({
    id: String(guild.id ?? ""),
    name: String(guild.name ?? ""),
    icon: guild.icon ?? null,
  })).filter((guild) => guild.id);
  const channels = (snapshot?.channels ?? []).map(normalizeChannel).filter((channel) => channel.id);
  const dms = sortDms((snapshot?.dms ?? []).map(normalizeChannel).filter((channel) => channel.id));
  const users = Object.fromEntries(
    Object.entries(snapshot?.users ?? {})
      .filter(([, user]) => user?.id)
      .map(([id, user]) => [id, { id: String(user.id), name: String(user.name ?? "User"), avatar: user.avatar ?? null }]),
  );
  return {
    user: snapshot?.user ?? null,
    guilds,
    channels,
    dms,
    messages: snapshot?.messages && typeof snapshot.messages === "object" ? snapshot.messages : {},
    users,
    folders: (snapshot?.folders ?? [])
      .filter(Boolean)
      .map((folder) => ({
        id: String(folder.id ?? ""),
        name: folder.name ?? null,
        color: folder.color ?? null,
        guildIds: Array.isArray(folder.guildIds) ? folder.guildIds.map(String) : [],
      }))
      .filter((folder) => folder.id && folder.guildIds.length),
    activeGuildId: snapshot?.activeGuildId ?? null,
    activeChannelId: snapshot?.activeChannelId ?? null,
  };
}

function normalizeChannel(channel: Channel): Channel {
  return {
    id: String(channel?.id ?? ""),
    guildId: channel?.guildId ?? null,
    name: String(channel?.name ?? "channel"),
    kind: Number(channel?.kind ?? 0),
    parentId: channel?.parentId ?? null,
    position: Number(channel?.position ?? 0),
    icon: channel?.icon ?? null,
    lastMessageId: channel?.lastMessageId ?? null,
  };
}

function mergeUsers(
  local: Record<string, User>,
  incoming?: Record<string, User>,
  user?: User | null,
  messages?: Record<string, ChatMessage[]>,
): Record<string, User> {
  const next = { ...local };
  for (const [id, row] of Object.entries(incoming ?? {})) if (row?.id) next[id] = row;
  if (user?.id) next[user.id] = user;
  for (const rows of Object.values(messages ?? {})) {
    for (const message of rows ?? []) {
      if (!message?.authorId) continue;
      next[message.authorId] = {
        id: message.authorId,
        name: message.author || next[message.authorId]?.name || "User",
        avatar: message.avatar ?? next[message.authorId]?.avatar ?? null,
      };
    }
  }
  return next;
}

function mergeMessages(
  local: Record<string, ChatMessage[]>,
  incoming: Record<string, ChatMessage[]>,
): Record<string, ChatMessage[]> {
  const next = { ...local };
  for (const [id, rows] of Object.entries(incoming ?? {})) {
    if (Array.isArray(rows) && !next[id]?.length) next[id] = rows;
  }
  return next;
}

function upsertMessage(list: ChatMessage[], message: ChatMessage): ChatMessage[] {
  const byId = list.findIndex((row) => row.id === message.id);
  if (byId >= 0) return list.map((row, index) => (index === byId ? { ...message, pending: false } : row));
  if (message.nonce) {
    const byNonce = list.findIndex((row) => row.pending && row.nonce === message.nonce);
    if (byNonce >= 0) return list.map((row, index) => (index === byNonce ? { ...message, pending: false } : row));
  }
  return [...list, message].slice(-200);
}
