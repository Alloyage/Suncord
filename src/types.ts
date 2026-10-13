export type Phase = "offline" | "connecting" | "ready" | "reconnecting";

export interface User {
  id: string;
  name: string;
  avatar?: string | null;
}

export interface Guild {
  id: string;
  name: string;
  icon?: string | null;
}

export interface GuildFolder {
  id: string;
  name?: string | null;
  color?: number | null;
  guildIds: string[];
}

export interface Channel {
  id: string;
  guildId?: string | null;
  name: string;
  kind: number;
  parentId?: string | null;
  position: number;
  icon?: string | null;
  lastMessageId?: string | null;
}

export interface Attachment {
  id: string;
  filename: string;
  url: string;
  proxyUrl?: string | null;
  contentType?: string | null;
  width?: number | null;
  height?: number | null;
}

export interface Media {
  url: string;
  proxyUrl?: string | null;
  width?: number | null;
  height?: number | null;
}

export interface Embed {
  title?: string | null;
  description?: string | null;
  url?: string | null;
  color?: number | null;
  image?: Media | null;
  thumbnail?: Media | null;
  video?: Media | null;
}

export interface ChatMessage {
  id: string;
  channelId: string;
  authorId: string;
  author: string;
  avatar?: string | null;
  content: string;
  timestamp: string;
  nonce?: string | null;
  pending: boolean;
  attachments?: Attachment[];
  embeds?: Embed[];
}

export interface Settings {
  bufferFrames: number;
  hardwareAccel: boolean;
  adaptiveThrottle: boolean;
  captureFps: number;
  hwPreference: string;
  asrEnabled: boolean;
  asrModelPath: string;
  inputDevice: string;
  outputDevice: string;
}

export interface Snapshot {
  user: User | null;
  guilds: Guild[];
  channels: Channel[];
  dms: Channel[];
  messages: Record<string, ChatMessage[]>;
  users?: Record<string, User>;
  folders?: GuildFolder[];
  activeGuildId?: string | null;
  activeChannelId?: string | null;
}

export interface Bootstrap {
  signedIn: boolean;
  snapshot: Snapshot;
  settings: Settings;
  phase: Phase;
}

export interface VoiceMember {
  id: string;
  name: string;
  avatar?: string | null;
  muted: boolean;
  deafened: boolean;
}

export interface VoiceStatus {
  phase: string;
  channelId?: string | null;
  channelName?: string | null;
  detail: string;
  muted: boolean;
  deafened: boolean;
  level: number;
  sampleRate: number;
  channels: number;
  bufferFrames: number;
  hardwareAccel: boolean;
  adaptiveThrottle: boolean;
  transport: string;
  dave: string;
  encoder: string;
  members?: VoiceMember[];
}

export interface CaptureStats {
  running: boolean;
  source: string;
  width: number;
  height: number;
  fps: number;
  captureMs: number;
  dropped: number;
  path: string;
}

export interface CaptureFrame {
  width: number;
  height: number;
  pngBase64: string;
}

export interface CaptureSource {
  id: string;
  kind: string;
  name: string;
  width: number;
  height: number;
}

export interface AudioDevice {
  name: string;
  input: boolean;
  output: boolean;
}

export interface AsrStatus {
  enabled: boolean;
  detail: string;
  caption: string;
  level: number;
}

export interface StatusEvent {
  phase: Phase;
  detail: string;
}

export interface HistoryEvent {
  channelId: string;
  messages: ChatMessage[];
}

export interface MessageDeleteEvent {
  channelId: string;
  messageIds: string[];
}

export interface Note {
  message: string;
}

export interface AuthEvent {
  signedIn: boolean;
}

export const defaultSettings: Settings = {
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

export const defaultVoice: VoiceStatus = {
  phase: "idle",
  channelId: null,
  channelName: null,
  detail: "Not in voice",
  muted: false,
  deafened: false,
  level: 0,
  sampleRate: 48000,
  channels: 2,
  bufferFrames: 480,
  hardwareAccel: true,
  adaptiveThrottle: false,
  transport: "idle",
  dave: "idle",
  encoder: "opus not linked",
  members: [],
};

export function isTextChannel(kind: number): boolean {
  return kind === 0 || kind === 1 || kind === 3 || kind === 5;
}

export function isVoiceChannel(kind: number): boolean {
  return kind === 2 || kind === 13;
}

export function isDirectChannel(kind: number): boolean {
  return kind === 1 || kind === 3;
}
