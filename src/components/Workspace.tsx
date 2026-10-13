import { memo, useEffect, useLayoutEffect, useMemo, useRef, useState, type ReactNode } from "react";
import {
  addReaction,
  channelPins,
  deleteMessage,
  editMessage,
  fetchAttachmentText,
  joinVoice,
  leaveVoice,
  openExternal,
  refreshDms,
  selectChannel,
  sendMessage,
  setDeafened,
  setMuted,
  updateSettings,
} from "../lib/backend";
import { byPosition, clock, sortDms } from "../lib/util";
import { useApp } from "../state/store";
import {
  Attachment,
  Channel,
  ChatMessage,
  Embed,
  Guild,
  GuildFolder,
  isDirectChannel,
  isTextChannel,
  isVoiceChannel,
  User,
} from "../types";

export function Workspace() {
  const guilds = useApp((state) => state.guilds) ?? [];
  const folders = useApp((state) => state.folders) ?? [];
  const channels = useApp((state) => state.channels) ?? [];
  const dms = useApp((state) => state.dms) ?? [];
  const messages = useApp((state) => state.messages) ?? {};

  return (
    <div className="relative flex h-full min-h-0" onContextMenu={(event) => event.preventDefault()}>
      <ServerRail guilds={guilds ?? []} folders={folders ?? []} dms={dms ?? []} />
      <Sidebar guilds={guilds ?? []} channels={channels ?? []} dms={dms ?? []} />
      <ChatPane channels={channels ?? []} dms={dms ?? []} messages={messages ?? {}} />
      <Lightbox />
    </div>
  );
}

function ServerRail({
  guilds,
  folders,
  dms,
}: {
  guilds: Guild[];
  folders: GuildFolder[];
  dms: Channel[];
}) {
  const activeGuildId = useApp((state) => state.activeGuildId);
  const [openFolders, setOpenFolders] = useState<Record<string, boolean>>({});
  const byId = useMemo(() => Object.fromEntries((guilds ?? []).map((guild) => [guild.id, guild])), [guilds]);
  const rail = useMemo(() => buildRail(folders ?? [], guilds ?? []), [folders, guilds]);

  useEffect(() => {
    void refreshDms().catch(() => undefined);
  }, []);

  return (
    <nav className="flex w-[72px] shrink-0 flex-col items-center gap-2 overflow-y-auto overflow-x-visible bg-rail py-3">
      <button
        title="Direct Messages"
        aria-label="Direct Messages"
        onClick={() => {
          const list = useApp.getState().dms ?? dms ?? [];
          const lastDmId = useApp.getState().lastDmId;
          const next = list.some((dm) => dm.id === lastDmId) ? lastDmId : null;
          useApp.getState().setPlace(null, next);
          if (next) {
            void selectChannel(next, null).then((rows) => useApp.getState().replaceMessages(next, rows));
          }
          void refreshDms()
            .then(() => {
              const fresh = useApp.getState().dms ?? [];
              const preferred = useApp.getState().lastDmId;
              const resolved = fresh.some((dm) => dm.id === preferred) ? preferred : useApp.getState().activeChannelId;
              if (resolved && resolved !== useApp.getState().activeChannelId) {
                useApp.getState().setPlace(null, resolved);
                void selectChannel(resolved, null).then((rows) => useApp.getState().replaceMessages(resolved, rows));
              }
            })
            .catch(() => undefined);
        }}
        className={`flex h-12 w-12 shrink-0 items-center justify-center transition-all duration-150 ease-in-out ${
          activeGuildId == null
            ? "rounded-2xl bg-white text-black"
            : "rounded-[24px] bg-[#141414] text-cream hover:rounded-2xl hover:bg-[#1c1c1c]"
        }`}
      >
        <SunIcon />
      </button>
      <div className="h-px w-8 bg-line" />
      {rail.map((entry) => {
        if (entry.kind === "guild") {
          const guild = byId[entry.guildId];
          if (!guild) return null;
          return (
            <RailButton
              key={guild.id}
              label={(guild.name ?? "").trim() || "Server"}
              image={guild.icon}
              guildId={guild.id}
              active={activeGuildId === guild.id}
              onClick={() => openGuild(guild.id)}
            />
          );
        }
        const expanded =
          openFolders[entry.folder.id] ??
          entry.folder.guildIds.some((guildId) => guildId === activeGuildId);
        const previewGuilds = entry.folder.guildIds
          .map((guildId) => byId[guildId])
          .filter(Boolean)
          .slice(0, 4) as Guild[];
        return (
          <div key={entry.folder.id} className="flex w-full flex-col items-center gap-1">
            {!expanded ? (
              <FolderBadge
                title={entry.folder.name || "Folder"}
                color={entry.folder.color}
                guilds={previewGuilds}
                active={entry.folder.guildIds.some((guildId) => guildId === activeGuildId)}
                onClick={() => setOpenFolders((prev) => ({ ...prev, [entry.folder.id]: true }))}
              />
            ) : (
              <>
                <button
                  title={`Collapse ${entry.folder.name || "folder"}`}
                  aria-label={`Collapse ${entry.folder.name || "folder"}`}
                  onClick={() => setOpenFolders((prev) => ({ ...prev, [entry.folder.id]: false }))}
                  className="flex h-6 w-12 items-center justify-center rounded-md bg-[#1a1a1a] text-[10px] uppercase tracking-wide text-mute hover:bg-[#222] hover:text-cream"
                >
                  ▲
                </button>
                {entry.folder.guildIds.map((guildId) => {
                  const guild = byId[guildId];
                  if (!guild) return null;
                  return (
                    <RailButton
                      key={`${entry.folder.id}-${guild.id}`}
                      label={(guild.name ?? "").trim() || "Server"}
                      image={guild.icon}
                      guildId={guild.id}
                      active={activeGuildId === guild.id}
                      onClick={() => openGuild(guild.id)}
                    />
                  );
                })}
              </>
            )}
          </div>
        );
      })}
    </nav>
  );
}

function buildRail(folders: GuildFolder[], guilds: Guild[]) {
  type Entry = { kind: "guild"; guildId: string } | { kind: "folder"; folder: GuildFolder };
  if (!folders.length) {
    return guilds.map((guild) => ({ kind: "guild" as const, guildId: guild.id }));
  }
  const entries: Entry[] = [];
  const seen = new Set<string>();
  for (const folder of folders) {
    const ids = (folder.guildIds ?? []).filter((id) => id && !seen.has(id));
    if (!ids.length) continue;
    // Backend marks unfoldered singles as `loose-{id}`; everything else is a real folder.
    const loose =
      folder.id.startsWith("loose-") ||
      (ids.length === 1 && folder.name == null && folder.color == null && !/^\d+$/.test(folder.id));
    if (loose) {
      const guildId = ids[0];
      seen.add(guildId);
      entries.push({ kind: "guild", guildId });
      continue;
    }
    ids.forEach((id) => seen.add(id));
    entries.push({ kind: "folder", folder: { ...folder, guildIds: ids } });
  }
  for (const guild of guilds) {
    if (!seen.has(guild.id)) entries.push({ kind: "guild", guildId: guild.id });
  }
  return entries;
}

function openGuild(guildId: string) {
  const nextChannels = (useApp.getState().channels ?? [])
    .filter((channel) => channel.guildId === guildId && isTextChannel(channel.kind))
    .sort((a, b) => a.position - b.position);
  const next = nextChannels[0]?.id ?? null;
  useApp.getState().setPlace(guildId, next);
  if (next) void selectChannel(next, guildId).then((rows) => useApp.getState().replaceMessages(next, rows));
}

function FolderBadge({
  title,
  color,
  guilds,
  active,
  onClick,
}: {
  title: string;
  color?: number | null;
  guilds: Guild[];
  active: boolean;
  onClick: () => void;
}) {
  const tint =
    color != null && color >= 0
      ? `#${color.toString(16).padStart(6, "0")}`
      : "#1a1a1a";
  const cells = [0, 1, 2, 3].map((index) => guilds[index] ?? null);
  return (
    <button
      title={title}
      aria-label={title}
      onClick={onClick}
      className={`grid h-12 w-12 shrink-0 grid-cols-2 grid-rows-2 gap-0.5 overflow-hidden rounded-2xl p-0.5 transition-all duration-150 ease-in-out ${
        active ? "ring-2 ring-white/80" : "hover:rounded-2xl"
      }`}
      style={{ background: tint }}
    >
      {cells.map((guild, index) =>
        guild ? (
          <FolderCell key={guild.id} guild={guild} />
        ) : (
          <span key={`empty-${index}`} className="h-full w-full" aria-hidden />
        ),
      )}
    </button>
  );
}

function FolderCell({ guild }: { guild: Guild }) {
  const [broken, setBroken] = useState(false);
  const src = resolveGuildIcon(guild.icon, guild.id);
  useEffect(() => setBroken(false), [src]);
  const showImage = Boolean(src) && !broken;
  return (
    <span className="flex h-full w-full items-center justify-center overflow-hidden rounded-[3px] bg-transparent">
      {showImage ? (
        <img
          src={src!}
          alt=""
          className="h-full w-full object-cover"
          draggable={false}
          onError={() => setBroken(true)}
        />
      ) : (
        <span className="flex h-full w-full items-center justify-center bg-black/25 text-[9px] font-semibold text-cream/90">
          {(guild.name || "?").slice(0, 1).toUpperCase()}
        </span>
      )}
    </span>
  );
}

function RailButton({
  label,
  active,
  image,
  guildId,
  onClick,
}: {
  label: string;
  active: boolean;
  image?: string | null;
  guildId?: string;
  onClick: () => void;
}) {
  const [broken, setBroken] = useState(false);
  const src = resolveGuildIcon(image, guildId);
  useEffect(() => setBroken(false), [src]);
  const showImage = Boolean(src) && !broken;
  return (
    <button
      title={label}
      onClick={onClick}
      className={`flex h-12 w-12 shrink-0 items-center justify-center overflow-hidden transition-all duration-150 ease-in-out ${
        active
          ? "rounded-2xl bg-[#1a1a1a] ring-2 ring-white/80"
          : "rounded-[24px] bg-[#141414] hover:rounded-2xl hover:bg-[#1c1c1c]"
      }`}
    >
      {showImage ? (
        <img
          src={src!}
          alt=""
          width={48}
          height={48}
          className="h-12 w-12 object-cover"
          draggable={false}
          onError={() => setBroken(true)}
        />
      ) : null}
    </button>
  );
}

function resolveGuildIcon(icon?: string | null, guildId?: string): string | null {
  if (!icon) return null;
  let value = icon.trim();
  if (!value || value === "null" || value === "undefined") return null;
  if (/^https?:\/\//i.test(value)) {
    return value.replace(/\.webp(\?|$)/i, ".png$1");
  }
  value = value.replace(/\.(png|jpe?g|gif|webp)$/i, "").split("?")[0] ?? value;
  if (!/^(a_)?[0-9a-f]{8,}$/i.test(value)) return null;
  if (!guildId) return null;
  const ext = value.startsWith("a_") ? "gif" : "png";
  return `https://cdn.discordapp.com/icons/${guildId}/${value}.${ext}?size=128`;
}

function SunIcon() {
  return (
    <svg className="shrink-0" width="22" height="22" viewBox="0 0 24 24" fill="none" aria-hidden="true">
      <circle cx="12" cy="12" r="4.25" fill="currentColor" />
      <g stroke="currentColor" strokeWidth="1.85" strokeLinecap="round">
        <path d="M12 2.25v2.2" />
        <path d="M12 19.55v2.2" />
        <path d="M4.93 4.93l1.56 1.56" />
        <path d="M17.51 17.51l1.56 1.56" />
        <path d="M2.25 12h2.2" />
        <path d="M19.55 12h2.2" />
        <path d="M4.93 19.07l1.56-1.56" />
        <path d="M17.51 6.49l1.56-1.56" />
      </g>
    </svg>
  );
}

function Sidebar({
  guilds,
  channels,
  dms,
}: {
  guilds: Guild[];
  channels: Channel[];
  dms: Channel[];
}) {
  const activeGuildId = useApp((state) => state.activeGuildId);
  const activeChannelId = useApp((state) => state.activeChannelId);
  const user = useApp((state) => state.user);
  const settings = useApp((state) => state.settings);
  const voice = useApp((state) => state.voice) ?? { phase: "idle", muted: false, deafened: false, level: 0, members: [] };
  const asr = useApp((state) => state.asr) ?? { enabled: false, caption: "", level: 0 };
  const [mics, setMics] = useState<MediaDeviceInfo[]>([]);
  const visible = useMemo(() => {
    if (!activeGuildId) return sortDms(dms ?? []);
    return (channels ?? []).filter((channel) => channel.guildId === activeGuildId);
  }, [activeGuildId, channels, dms]);
  const title = activeGuildId
    ? guilds.find((guild) => guild.id === activeGuildId)?.name ?? "Server"
    : "Direct messages";
  const inVoice = Boolean(voice.channelId) && voice.phase !== "idle";
  const members = voice.members ?? [];
  const micLevel = useLiveMicLevel(settings?.inputDevice ?? "", inVoice && !voice.muted);

  useEffect(() => {
    let alive = true;
    const load = async () => {
      try {
        // Permission unlocks device labels in Chromium / WebView2.
        const probe = await navigator.mediaDevices.getUserMedia({ audio: true, video: false });
        probe.getTracks().forEach((track) => track.stop());
        const devices = await navigator.mediaDevices.enumerateDevices();
        if (alive) setMics(devices.filter((device) => device.kind === "audioinput"));
      } catch {
        if (alive) setMics([]);
      }
    };
    void load();
    const onChange = () => void load();
    navigator.mediaDevices?.addEventListener?.("devicechange", onChange);
    return () => {
      alive = false;
      navigator.mediaDevices?.removeEventListener?.("devicechange", onChange);
    };
  }, []);

  return (
    <aside className="relative z-10 flex w-[260px] shrink-0 flex-col bg-side">
      <div className="relative z-10 flex h-12 items-center border-b border-line px-4 text-sm font-semibold">{title}</div>
      <div className="min-h-0 flex-1 overflow-auto px-2 py-2">
        {visible.length === 0 ? (
          <p className="px-2 py-3 text-sm text-mute">
            {activeGuildId ? "No channels in this server yet." : "No direct messages yet."}
          </p>
        ) : activeGuildId ? (
          <GuildChannels channels={visible} activeId={activeChannelId} guildId={activeGuildId} />
        ) : (
          <DmList channels={visible} activeId={activeChannelId} />
        )}
      </div>
      <div className="shrink-0 border-t border-line px-3 pb-3 pt-2.5">
        {inVoice ? (
          <div className="mb-2 rounded bg-black/40 px-2 py-2">
            <p className="text-[11px] uppercase tracking-wide text-mute">Voice connected</p>
            <p className="truncate text-sm">{voice.channelName || "Voice channel"}</p>
            <p className="truncate text-xs text-mute">{voice.detail}</p>
            {members.length ? (
              <div className="mt-2 space-y-1">
                {members.map((member) => (
                  <div key={member.id} className="flex items-center gap-2 text-xs text-mute">
                    <span className="flex h-5 w-5 overflow-hidden rounded-full bg-[#1a1a1a]">
                      {member.avatar ? <img src={member.avatar} alt="" className="h-full w-full object-cover" /> : null}
                    </span>
                    <span className="truncate text-cream">{member.name}</span>
                    {member.muted || member.deafened ? <MemberState muted={member.muted} deafened={member.deafened} /> : null}
                  </div>
                ))}
              </div>
            ) : null}
          </div>
        ) : null}
        <label className="mb-2 block px-0.5 text-[11px] text-mute">
          Microphone
          <select
            className="mt-1 w-full truncate rounded bg-[#141414] px-2 py-1.5 text-xs text-cream outline-none transition-all duration-150 ease-in-out"
            value={settings?.inputDevice ?? ""}
            onChange={(event) => {
              const inputDevice = event.target.value;
              useApp.getState().patchSettings({ inputDevice });
              void updateSettings({ inputDevice }).then((next) => useApp.getState().patchSettings(next));
            }}
          >
            <option value="">System default</option>
            {mics.map((device) => (
              <option key={device.deviceId} value={device.label || device.deviceId}>
                {device.label || `Mic ${device.deviceId.slice(0, 8)}`}
              </option>
            ))}
          </select>
        </label>
        {inVoice ? (
          <div className="mb-2 h-1.5 overflow-hidden rounded bg-black" title="Microphone level" aria-hidden={!inVoice}>
            <div
              className="mic-level h-full origin-left bg-white/80"
              style={{ transform: `scaleX(${Math.min(1, Math.max(0.02, micLevel * 1.6))})` }}
            />
          </div>
        ) : null}
        <div className="mb-2 flex items-center gap-1">
          <VoiceToggle
            label={voice.muted ? "Unmute microphone" : "Mute microphone"}
            active={Boolean(voice.muted)}
            onClick={() => void setMuted(!voice.muted)}
            icon="mute"
          />
          <VoiceToggle
            label={voice.deafened ? "Undeafen" : "Deafen headphones"}
            active={Boolean(voice.deafened)}
            onClick={() => void setDeafened(!voice.deafened)}
            icon="deafen"
          />
          {inVoice ? (
            <>
              <VoiceToggle label="Disconnect from voice" active={false} onClick={() => void leaveVoice()} icon="disconnect" />
              <VoiceToggle
                label="Share screen"
                active={false}
                onClick={() => useApp.getState().setShareOpen(true)}
                icon="share"
              />
            </>
          ) : null}
        </div>
        {asr.caption ? <p className="mb-2 px-1 text-xs text-mute">{asr.caption}</p> : null}
        <div className="flex min-w-0 items-center gap-2 overflow-visible py-0.5">
          <div className="flex h-8 w-8 shrink-0 items-center justify-center overflow-hidden rounded-full bg-[#1a1a1a] text-xs">
            {user?.avatar ? (
              <img src={user.avatar} alt="" className="h-full w-full object-cover" />
            ) : (
              (user?.name ?? "?").slice(0, 1)
            )}
          </div>
          <div className="min-w-0 flex-1 truncate text-sm">{user?.name ?? "Account"}</div>
          <button
            type="button"
            title={asr.enabled ? "Turn off local captions" : "Turn on local captions"}
            aria-label={asr.enabled ? "Turn off local captions" : "Turn on local captions"}
            className={`flex h-8 shrink-0 items-center justify-center rounded px-2 text-xs transition-all duration-150 ease-in-out ${asr.enabled ? "bg-white text-black" : "text-mute hover:bg-white/10 hover:text-cream"}`}
            onClick={() => {
              const next = !useApp.getState().settings.asrEnabled;
              useApp.getState().patchSettings({ asrEnabled: next });
              void updateSettings({ asrEnabled: next });
            }}
          >
            CC
          </button>
          <button
            type="button"
            title="User settings"
            className="flex h-8 w-8 shrink-0 items-center justify-center rounded text-xs text-mute transition-all duration-150 ease-in-out hover:bg-white/10 hover:text-cream"
            onClick={() => useApp.getState().setSettingsOpen(true)}
            aria-label="User settings"
          >
            ···
          </button>
        </div>
      </div>
    </aside>
  );
}

function useLiveMicLevel(preferredLabel: string, enabled: boolean): number {
  const [level, setLevel] = useState(0);
  useEffect(() => {
    if (!enabled) {
      setLevel(0);
      return;
    }
    let cancelled = false;
    let stream: MediaStream | null = null;
    let context: AudioContext | null = null;
    let frame = 0;
    const run = async () => {
      try {
        const devices = await navigator.mediaDevices.enumerateDevices();
        const match = preferredLabel
          ? devices.find((device) => device.kind === "audioinput" && (device.label === preferredLabel || device.deviceId === preferredLabel))
          : undefined;
        stream = await navigator.mediaDevices.getUserMedia({
          audio: match ? { deviceId: { exact: match.deviceId }, echoCancellation: true, noiseSuppression: true } : true,
          video: false,
        });
        if (cancelled) {
          stream.getTracks().forEach((track) => track.stop());
          return;
        }
        context = new AudioContext();
        const source = context.createMediaStreamSource(stream);
        const analyser = context.createAnalyser();
        analyser.fftSize = 512;
        analyser.smoothingTimeConstant = 0.35;
        source.connect(analyser);
        const samples = new Uint8Array(analyser.fftSize);
        const tick = () => {
          if (cancelled) return;
          analyser.getByteTimeDomainData(samples);
          let sum = 0;
          for (let i = 0; i < samples.length; i += 1) {
            const sample = (samples[i] - 128) / 128;
            sum += sample * sample;
          }
          const rms = Math.sqrt(sum / samples.length);
          setLevel(Math.min(1, rms * 4.2));
          frame = requestAnimationFrame(tick);
        };
        frame = requestAnimationFrame(tick);
      } catch {
        if (!cancelled) setLevel(0);
      }
    };
    void run();
    return () => {
      cancelled = true;
      cancelAnimationFrame(frame);
      stream?.getTracks().forEach((track) => track.stop());
      void context?.close();
    };
  }, [preferredLabel, enabled]);
  return level;
}

function MemberState({ muted, deafened }: { muted: boolean; deafened: boolean }) {
  return (
    <span className="flex gap-1 text-mute" aria-label={`${muted ? "Muted" : ""}${muted && deafened ? ", " : ""}${deafened ? "Deafened" : ""}`}>
      {muted ? <VoiceGlyph icon="mute" /> : null}
      {deafened ? <VoiceGlyph icon="deafen" /> : null}
    </span>
  );
}

function VoiceToggle({ label, active, onClick, icon }: { label: string; active: boolean; onClick: () => void; icon: "mute" | "deafen" | "disconnect" | "share" }) {
  return (
    <button
      type="button"
      title={label}
      aria-label={label}
      aria-pressed={active}
      onClick={onClick}
      className={`flex h-8 w-8 shrink-0 items-center justify-center rounded transition-all duration-150 ease-in-out ${active ? "bg-white text-black" : "text-mute hover:bg-white/10 hover:text-cream"}`}
    >
      <VoiceGlyph icon={icon} />
    </button>
  );
}

function VoiceGlyph({ icon }: { icon: "mute" | "deafen" | "disconnect" | "share" }) {
  const common = { width: 15, height: 15, viewBox: "0 0 24 24", fill: "none", stroke: "currentColor", strokeWidth: 2, strokeLinecap: "round" as const, strokeLinejoin: "round" as const, "aria-hidden": true, className: "shrink-0 overflow-visible" };
  if (icon === "mute") return <svg {...common}><path d="M12 2a3 3 0 0 0-3 3v7a3 3 0 0 0 5.2 2.1" /><path d="M5 10v2a7 7 0 0 0 11.3 5.5" /><path d="M12 19v3" /><path d="M3 3l18 18" /></svg>;
  if (icon === "deafen") return <svg {...common}><path d="M4 13a8 8 0 0 1 16 0" /><path d="M4 13v4h4v-5" /><path d="M20 13v4h-4v-5" /><path d="M3 3l18 18" /></svg>;
  if (icon === "disconnect") {
    return (
      <svg {...common}>
        <path d="M10.68 13.31a16 16 0 0 0 3.41 2.6l1.27-1.27a2 2 0 0 1 2.11-.45 12.84 12.84 0 0 0 2.81.7 2 2 0 0 1 1.72 2v3a2 2 0 0 1-2.18 2 19.79 19.79 0 0 1-8.63-3.07 19.42 19.42 0 0 1-6-6 19.79 19.79 0 0 1-3.07-8.67A2 2 0 0 1 4.11 2h3a2 2 0 0 1 2 1.72 12.84 12.84 0 0 0 .7 2.81 2 2 0 0 1-.45 2.11L8.09 9.91" />
        <path d="M23 1 1 23" />
      </svg>
    );
  }
  return <svg {...common}><rect x="3" y="4" width="13" height="11" rx="2" /><path d="M16 8l5-3v9l-5-3" /></svg>;
}

function GuildChannels({ channels, activeId, guildId }: { channels: Channel[]; activeId: string | null; guildId: string }) {
  const list = channels ?? [];
  const categories = list.filter((channel) => channel.kind === 4).sort(byPosition);
  const loose = list
    .filter(
      (channel) =>
        !channel.parentId && channel.kind !== 4 && (isTextChannel(channel.kind) || isVoiceChannel(channel.kind)),
    )
    .sort(byPosition);
  return (
    <div>
      {loose.map((channel) => (
        <ChannelRow key={channel.id} channel={channel} active={channel.id === activeId} guildId={guildId} />
      ))}
      {categories.map((category) => (
        <div key={category.id} className="mt-3">
          <div className="px-2 pb-1 text-[11px] uppercase tracking-wide text-mute">{category.name || "Category"}</div>
          {list
            .filter(
              (channel) =>
                channel.parentId === category.id && (isTextChannel(channel.kind) || isVoiceChannel(channel.kind)),
            )
            .sort(byPosition)
            .map((channel) => (
              <ChannelRow key={channel.id} channel={channel} active={channel.id === activeId} guildId={guildId} />
            ))}
        </div>
      ))}
    </div>
  );
}

function DmList({ channels, activeId }: { channels: Channel[]; activeId: string | null }) {
  return (
    <div>
      {sortDms(channels ?? []).map((channel) => (
        <ChannelRow key={channel.id} channel={channel} active={channel.id === activeId} guildId={null} />
      ))}
    </div>
  );
}

function ChannelRow({ channel, active, guildId }: { channel: Channel; active: boolean; guildId: string | null }) {
  const voice = isVoiceChannel(channel.kind);
  const dm = guildId == null && isDirectChannel(channel.kind);
  const label = channel.name || (dm ? "Direct message" : "channel");
  return (
    <button
      className={`flex w-full items-center gap-2 rounded px-2 py-1.5 text-left text-sm transition-all duration-150 ease-in-out ${active ? "bg-white/10 text-cream" : "text-mute hover:bg-white/5 hover:text-cream"}`}
      onClick={() => {
        if (voice) {
          void joinVoice(channel.id, guildId);
          return;
        }
        useApp.getState().setPlace(guildId, channel.id);
        void selectChannel(channel.id, guildId).then((rows) => useApp.getState().replaceMessages(channel.id, rows));
      }}
    >
      {dm ? (
        <span className="flex h-6 w-6 shrink-0 items-center justify-center overflow-hidden rounded-full bg-[#1a1a1a] text-[10px]">
          {channel.icon ? (
            <img src={channel.icon} alt="" className="h-full w-full object-cover" />
          ) : (
            label.slice(0, 1).toUpperCase()
          )}
        </span>
      ) : (
        <span className="w-4 text-center text-xs">{voice ? "♪" : "#"}</span>
      )}
      <span className="truncate">{label}</span>
    </button>
  );
}

function ChatPane({
  channels,
  dms,
  messages,
}: {
  channels: Channel[];
  dms: Channel[];
  messages: Record<string, ChatMessage[]>;
}) {
  const activeChannelId = useApp((state) => state.activeChannelId);
  const rows = activeChannelId ? messages?.[activeChannelId] ?? [] : [];
  const channel = [...(channels ?? []), ...(dms ?? [])].find((item) => item?.id === activeChannelId) ?? null;
  const dm = channel ? isDirectChannel(channel.kind) && !channel.guildId : false;
  const [pinsOpen, setPinsOpen] = useState(false);
  const [pins, setPins] = useState<ChatMessage[]>([]);
  const heading = !channel
    ? "Select a channel"
    : isVoiceChannel(channel.kind)
      ? channel.name || "Voice"
      : dm
        ? channel.name || "Direct message"
        : `# ${channel.name || "channel"}`;

  return (
    <section className="relative z-0 flex min-w-0 flex-1 flex-col bg-ink">
      <header className="relative z-10 flex h-12 shrink-0 items-center gap-2 border-b border-line bg-ink px-4 text-sm font-semibold">
        {dm && channel?.icon ? (
          <span className="flex h-6 w-6 overflow-hidden rounded-full bg-panel">
            <img src={channel.icon} alt="" className="h-full w-full object-cover" />
          </span>
        ) : null}
        <span className="min-w-0 flex-1 truncate">{heading}</span>
        {channel && !isVoiceChannel(channel.kind) ? (
          <div className="flex items-center gap-1">
            <button
              className="rounded px-2 py-1 text-xs text-mute hover:bg-white/10 hover:text-cream"
              onClick={() => {
                if (!channel) return;
                void channelPins(channel.id)
                  .then((rows) => {
                    setPins(rows ?? []);
                    setPinsOpen(true);
                  })
                  .catch((error) => {
                    useApp.getState().setError(error instanceof Error ? error.message : "Could not load pins.");
                  });
              }}
            >
              Pins
            </button>
            {dm ? (
              <button
                className="rounded px-2 py-1 text-xs text-mute hover:bg-white/10 hover:text-cream"
                onClick={() => void joinVoice(channel.id, null)}
              >
                Call
              </button>
            ) : null}
          </div>
        ) : null}
      </header>
      {activeChannelId ? (
        <MessageList key={activeChannelId} channelId={activeChannelId} messages={rows ?? []} />
      ) : (
        <div className="flex min-h-0 flex-1 items-center justify-center px-4 text-sm text-mute">
          Pick a server or direct message to get started.
        </div>
      )}
      <Composer channelId={activeChannelId} channelName={channel?.name ?? ""} dm={dm} />
      {pinsOpen ? (
        <Modal title="Pinned messages" onClose={() => setPinsOpen(false)}>
          {pins.length === 0 ? (
            <p className="text-sm text-mute">No pinned messages in this channel.</p>
          ) : (
            <div className="max-h-[60vh] space-y-3 overflow-auto">
              {pins.map((message) => (
                <MessageRow key={message.id} message={message} />
              ))}
            </div>
          )}
        </Modal>
      ) : null}
    </section>
  );
}

function MessageList({ channelId, messages }: { channelId: string | null; messages: ChatMessage[] }) {
  const scroller = useRef<HTMLDivElement>(null);
  const stickBottom = useRef(true);
  const suppressing = useRef(false);
  const end = useRef<HTMLDivElement>(null);

  const snapBottom = () => {
    const node = scroller.current;
    if (!node) return;
    suppressing.current = true;
    node.scrollTop = node.scrollHeight;
    end.current?.scrollIntoView({ block: "end" });
    requestAnimationFrame(() => {
      if (scroller.current) scroller.current.scrollTop = scroller.current.scrollHeight;
      suppressing.current = false;
    });
  };

  useLayoutEffect(() => {
    stickBottom.current = true;
    snapBottom();
    const t1 = window.setTimeout(snapBottom, 16);
    const t2 = window.setTimeout(snapBottom, 80);
    return () => {
      window.clearTimeout(t1);
      window.clearTimeout(t2);
    };
  }, [channelId]);

  useLayoutEffect(() => {
    if (!stickBottom.current) return;
    snapBottom();
  }, [messages]);

  return (
    <div
      ref={scroller}
      className="min-h-0 flex-1 overflow-auto px-4 py-3"
      onScroll={() => {
        if (suppressing.current) return;
        const node = scroller.current;
        if (!node) return;
        stickBottom.current = node.scrollHeight - node.scrollTop - node.clientHeight < 120;
      }}
    >
      {(messages ?? []).map((message) => (
        <MessageRow key={message.id} message={message} />
      ))}
      <div ref={end} aria-hidden className="h-px w-full" />
    </div>
  );
}

const QUICK_REACTIONS = ["👍", "❤️", "😂", "😮", "😢", "🎉"];

const MessageRow = memo(function MessageRow({ message }: { message: ChatMessage }) {
  const users = useApp((state) => state.users) ?? {};
  const me = useApp((state) => state.user);
  const [menu, setMenu] = useState<{ x: number; y: number } | null>(null);
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(message.content);
  const time = clock(message.timestamp);
  const author = message.author || "Unknown";
  const attachments = message.attachments ?? [];
  const embeds = message.embeds ?? [];
  const mine = Boolean(me?.id && me.id === message.authorId);

  useEffect(() => {
    if (!menu) return;
    const close = () => setMenu(null);
    window.addEventListener("click", close);
    window.addEventListener("blur", close);
    window.addEventListener("resize", close);
    return () => {
      window.removeEventListener("click", close);
      window.removeEventListener("blur", close);
      window.removeEventListener("resize", close);
    };
  }, [menu]);

  useEffect(() => {
    if (!editing) setDraft(message.content);
  }, [editing, message.content]);

  return (
    <article
      className={`msg relative flex gap-3 py-1.5 ${message.pending ? "opacity-60" : ""}`}
      onContextMenu={(event) => {
        event.preventDefault();
        event.stopPropagation();
        setMenu({ x: event.clientX, y: event.clientY });
      }}
    >
      <div className="mt-0.5 flex h-9 w-9 shrink-0 items-center justify-center overflow-hidden rounded-full bg-panel text-xs">
        {message.avatar ? (
          <img alt="" src={message.avatar} className="h-full w-full object-cover" />
        ) : (
          author.slice(0, 1)
        )}
      </div>
      <div className="min-w-0 flex-1">
        <div className="flex items-baseline gap-2">
          <span className="text-sm font-semibold">{author}</span>
          <time className="text-[11px] text-mute">{time}</time>
        </div>
        {editing ? (
          <form
            className="mt-1"
            onSubmit={(event) => {
              event.preventDefault();
              const content = draft.trim();
              if (!content) return;
              void editMessage(message.channelId, message.id, content)
                .then((updated) => {
                  useApp.getState().pushMessage({ ...updated, pending: false });
                  setEditing(false);
                })
                .catch((error) => {
                  useApp.getState().setError(error instanceof Error ? error.message : "Could not edit message.");
                });
            }}
          >
            <textarea
              value={draft}
              rows={3}
              autoFocus
              className="w-full resize-none rounded-md bg-[#141414] px-3 py-2 text-sm outline-none"
              onChange={(event) => setDraft(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === "Escape") {
                  event.preventDefault();
                  setEditing(false);
                  setDraft(message.content);
                }
                if (event.key === "Enter" && !event.shiftKey) {
                  event.preventDefault();
                  (event.currentTarget.form as HTMLFormElement | null)?.requestSubmit();
                }
              }}
            />
            <div className="mt-1 flex gap-2 text-xs text-mute">
              <button type="submit" className="rounded px-2 py-1 text-cream hover:bg-white/10">
                Save
              </button>
              <button
                type="button"
                className="rounded px-2 py-1 hover:bg-white/10"
                onClick={() => {
                  setEditing(false);
                  setDraft(message.content);
                }}
              >
                Cancel
              </button>
            </div>
          </form>
        ) : message.content ? (
          <p className="whitespace-pre-wrap break-words text-sm leading-6">
            {renderContent(message.content, users)}
          </p>
        ) : null}
        {attachments.length > 0 ? <AttachmentList attachments={attachments} /> : null}
        {embeds.length > 0 ? <EmbedList embeds={embeds} /> : null}
      </div>
      {menu ? (
        <MessageContextMenu
          x={menu.x}
          y={menu.y}
          mine={mine}
          onClose={() => setMenu(null)}
          onEdit={() => {
            setMenu(null);
            setEditing(true);
            setDraft(message.content);
          }}
          onReact={(emoji) => {
            setMenu(null);
            void addReaction(message.channelId, message.id, emoji).catch((error) => {
              useApp.getState().setError(error instanceof Error ? error.message : "Could not add reaction.");
            });
          }}
          onCopyText={() => {
            setMenu(null);
            void navigator.clipboard.writeText(message.content || "").catch(() => undefined);
          }}
          onCopyId={() => {
            setMenu(null);
            void navigator.clipboard.writeText(message.id).catch(() => undefined);
          }}
          onDelete={() => {
            setMenu(null);
            void deleteMessage(message.channelId, message.id)
              .then(() => useApp.getState().removeMessages(message.channelId, [message.id]))
              .catch((error) => {
                useApp.getState().setError(error instanceof Error ? error.message : "Could not delete message.");
              });
          }}
        />
      ) : null}
    </article>
  );
});

function MessageContextMenu({
  x,
  y,
  mine,
  onClose,
  onEdit,
  onReact,
  onCopyText,
  onCopyId,
  onDelete,
}: {
  x: number;
  y: number;
  mine: boolean;
  onClose: () => void;
  onEdit: () => void;
  onReact: (emoji: string) => void;
  onCopyText: () => void;
  onCopyId: () => void;
  onDelete: () => void;
}) {
  const left = Math.min(x, window.innerWidth - 220);
  const top = Math.min(y, window.innerHeight - 280);
  return (
    <div
      className="fixed z-50 w-52 overflow-hidden rounded-md border border-line bg-[#111] py-1 shadow-xl"
      style={{ left, top }}
      onClick={(event) => event.stopPropagation()}
      onContextMenu={(event) => event.preventDefault()}
    >
      <div className="flex gap-1 border-b border-line px-2 py-1.5">
        {QUICK_REACTIONS.map((emoji) => (
          <button
            key={emoji}
            type="button"
            className="flex h-7 w-7 items-center justify-center rounded hover:bg-white/10"
            onClick={() => onReact(emoji)}
            title={`React ${emoji}`}
          >
            {emoji}
          </button>
        ))}
      </div>
      {mine ? (
        <button type="button" className="block w-full px-3 py-1.5 text-left text-sm hover:bg-white/10" onClick={onEdit}>
          Edit Message
        </button>
      ) : null}
      <button type="button" className="block w-full px-3 py-1.5 text-left text-sm hover:bg-white/10" onClick={onCopyText}>
        Copy Text
      </button>
      <button type="button" className="block w-full px-3 py-1.5 text-left text-sm hover:bg-white/10" onClick={onCopyId}>
        Copy Message ID
      </button>
      {mine ? (
        <button type="button" className="block w-full px-3 py-1.5 text-left text-sm text-ember hover:bg-white/10" onClick={onDelete}>
          Delete Message
        </button>
      ) : null}
      <button type="button" className="block w-full px-3 py-1.5 text-left text-sm text-mute hover:bg-white/10" onClick={onClose}>
        Close
      </button>
    </div>
  );
}

function renderContent(content: string, users: Record<string, User>) {
  const parts = content.split(/(https?:\/\/[^\s<]+|<@!?\d+>)/g);
  return parts.map((part, index) => {
    const mention = part.match(/^<@!?(\d+)>$/);
    if (mention) {
      const id = mention[1];
      const name = users[id]?.name || `user ${id.slice(-4)}`;
      return (
        <span key={`${index}-${id}`} className="rounded bg-white/10 px-1 text-cream">
          @{name}
        </span>
      );
    }
    if (/^https?:\/\//i.test(part)) {
      return (
        <button
          key={`${index}-link`}
          className="text-cream underline decoration-white/30 underline-offset-2"
          onClick={() => void openExternal(part)}
        >
          {part}
        </button>
      );
    }
    return <span key={`${index}-${part.slice(0, 8)}`}>{part}</span>;
  });
}

function AttachmentList({ attachments }: { attachments: Attachment[] }) {
  return (
    <div className="mt-2 flex flex-col gap-2">
      {attachments.map((attachment) => {
        const src = attachment.proxyUrl || attachment.url;
        if (isVideoAttachment(attachment)) {
          return (
            <video
              key={attachment.id}
              src={src}
              controls
              autoPlay
              loop
              muted
              playsInline
              className="max-h-80 max-w-md rounded-md bg-black"
            />
          );
        }
        if (isImageAttachment(attachment)) {
          return (
            <button
              key={attachment.id}
              className="block max-w-md text-left"
              onClick={() => useApp.setState({ lightboxUrl: attachment.url || src })}
            >
              <img
                src={src}
                alt={attachment.filename || "attachment"}
                className="max-h-80 max-w-full rounded-md object-contain"
                loading="lazy"
              />
            </button>
          );
        }
        if (isTextAttachment(attachment)) {
          return <TextAttachmentPreview key={attachment.id} attachment={attachment} src={src} />;
        }
        return (
          <button
            key={attachment.id}
            className="text-left text-sm text-cream underline decoration-white/30 underline-offset-2"
            onClick={() => void openExternal(attachment.url)}
          >
            {attachment.filename || "Download attachment"}
          </button>
        );
      })}
    </div>
  );
}

function TextAttachmentPreview({ attachment, src }: { attachment: Attachment; src: string }) {
  const [text, setText] = useState<string | null>(null);
  const [error, setError] = useState(false);
  useEffect(() => {
    let cancelled = false;
    setText(null);
    setError(false);
    // Discord CDN blocks browser CORS — pull through the Tauri backend instead.
    const load = async () => {
      const candidates = [attachment.url, src, attachment.proxyUrl].filter(
        (value, index, list): value is string => Boolean(value) && list.indexOf(value) === index,
      );
      let lastError: unknown = null;
      for (const candidate of candidates) {
        try {
          const body = await fetchAttachmentText(candidate);
          if (cancelled) return;
          setText(body.length > 80_000 ? `${body.slice(0, 80_000)}\n…` : body);
          return;
        } catch (error) {
          lastError = error;
        }
      }
      if (!cancelled) {
        console.warn("attachment preview failed", lastError);
        setError(true);
      }
    };
    void load();
    return () => {
      cancelled = true;
    };
  }, [attachment.url, attachment.proxyUrl, src]);
  return (
    <div className="max-w-lg overflow-hidden rounded-md border border-line bg-[#0e0e0e]">
      <div className="flex items-center justify-between gap-2 border-b border-white/5 px-3 py-1.5">
        <span className="truncate text-xs text-mute">{attachment.filename || "text file"}</span>
        <button
          type="button"
          className="shrink-0 text-xs text-cream underline decoration-white/30 underline-offset-2"
          onClick={() => void openExternal(attachment.url)}
        >
          Download
        </button>
      </div>
      {error ? (
        <p className="px-3 py-2 text-xs text-mute">Could not load preview.</p>
      ) : text == null ? (
        <p className="px-3 py-2 text-xs text-mute">Loading preview…</p>
      ) : (
        <pre className="max-h-72 overflow-auto whitespace-pre-wrap break-words px-3 py-2 font-mono text-[11px] leading-relaxed text-cream/90">
          <code>{text}</code>
        </pre>
      )}
    </div>
  );
}

function EmbedList({ embeds }: { embeds: Embed[] }) {
  return (
    <div className="mt-2 flex flex-col gap-2">
      {embeds.map((embed, index) => {
        const image = embed.image || embed.thumbnail;
        const video = embed.video;
        const imageUrl = image?.proxyUrl || image?.url;
        const videoUrl = video?.proxyUrl || video?.url;
        const accent = embed.color != null ? `#${embed.color.toString(16).padStart(6, "0")}` : "#3a3a3a";
        return (
          <div
            key={`${embed.title ?? "embed"}-${index}`}
            className="max-w-md overflow-hidden rounded-md border border-line bg-panel/60"
            style={{ borderLeftColor: accent, borderLeftWidth: 3 }}
          >
            <div className="space-y-1 px-3 py-2">
              {embed.title ? (
                embed.url ? (
                  <button
                    className="text-left text-sm font-semibold text-cream"
                    onClick={() => void openExternal(embed.url!)}
                  >
                    {embed.title}
                  </button>
                ) : (
                  <p className="text-sm font-semibold">{embed.title}</p>
                )
              ) : null}
              {embed.description ? (
                <p className="whitespace-pre-wrap break-words text-sm text-mute">{embed.description}</p>
              ) : null}
            </div>
            {videoUrl && isVideoUrl(videoUrl) ? (
              <video src={videoUrl} controls autoPlay loop muted playsInline className="max-h-80 w-full bg-black" />
            ) : imageUrl ? (
              <button
                className="block w-full"
                onClick={() => useApp.setState({ lightboxUrl: image?.url || imageUrl })}
              >
                <img src={imageUrl} alt="" className="max-h-80 w-full object-cover" loading="lazy" />
              </button>
            ) : null}
          </div>
        );
      })}
    </div>
  );
}

function isImageAttachment(attachment: Attachment): boolean {
  const type = attachment.contentType?.toLowerCase() ?? "";
  if (type.startsWith("image/")) return true;
  return /\.(png|jpe?g|gif|webp|avif|bmp)$/i.test(attachment.filename || attachment.url);
}

function isVideoAttachment(attachment: Attachment): boolean {
  const type = attachment.contentType?.toLowerCase() ?? "";
  if (type.startsWith("video/")) return true;
  return isVideoUrl(attachment.filename || attachment.url);
}

function isTextAttachment(attachment: Attachment): boolean {
  const type = attachment.contentType?.toLowerCase() ?? "";
  if (
    type.startsWith("text/") ||
    type.includes("json") ||
    type.includes("xml") ||
    type.includes("javascript") ||
    type.includes("typescript") ||
    type.includes("octet-stream")
  ) {
    // octet-stream still needs a text-like extension (e.g. .nix from Discord).
    if (type.includes("octet-stream")) {
      return /\.(nix|txt|log|md|markdown|rs|json|toml|yml|yaml|conf|cfg|ini|env|sh|ps1|bat|py|ts|tsx|js|jsx|css|html|c|cpp|h|hpp|go|java)$/i.test(
        attachment.filename || attachment.url,
      );
    }
    return true;
  }
  return /\.(nix|txt|log|md|markdown|csv|tsv|json|xml|yml|yaml|toml|ini|cfg|conf|env|rs|ts|tsx|js|jsx|css|html|py|go|java|c|cpp|h|hpp|sh|ps1|bat)$/i.test(
    attachment.filename || attachment.url,
  );
}

function isVideoUrl(value: string): boolean {
  return /\.(mp4|webm|mov)(\?|$)/i.test(value);
}

function Composer({
  channelId,
  channelName,
  dm,
}: {
  channelId: string | null;
  channelName: string;
  dm: boolean;
}) {
  const [value, setValue] = useState("");
  useEffect(() => setValue(""), [channelId]);
  const placeholder = !channelId
    ? "Select a channel to chat"
    : dm
      ? `Message @${channelName}`
      : `Message #${channelName}`;
  return (
    <form
      className="shrink-0 border-t border-line bg-ink"
      onSubmit={(event) => {
        event.preventDefault();
        void submit(channelId, value, setValue);
      }}
    >
      <textarea
        value={value}
        disabled={!channelId}
        rows={1}
        placeholder={placeholder}
        className="block w-full resize-none border-0 bg-transparent px-4 py-3.5 text-center text-sm leading-6 outline-none placeholder:text-center placeholder:text-[#5c5c5c] disabled:opacity-50"
        onChange={(event) => setValue(event.target.value)}
        onKeyDown={(event) => {
          if (event.key === "Enter" && !event.shiftKey) {
            event.preventDefault();
            void submit(channelId, value, setValue);
          }
        }}
      />
    </form>
  );
}

async function submit(channelId: string | null, value: string, setValue: (next: string) => void) {
  const content = value.trim();
  if (!channelId || !content) return;
  const nonce = `${Date.now()}${Math.floor(Math.random() * 1_000_000)}`;
  const user = useApp.getState().user;
  useApp.getState().pushMessage({
    id: `local-${nonce}`,
    channelId,
    authorId: user?.id ?? "",
    author: user?.name ?? "You",
    avatar: user?.avatar,
    content: content.slice(0, 2000),
    timestamp: new Date().toISOString(),
    nonce,
    pending: true,
    attachments: [],
    embeds: [],
  });
  setValue("");
  try {
    const sent = await sendMessage(channelId, content.slice(0, 2000), nonce);
    useApp.getState().pushMessage({ ...sent, pending: false });
  } catch (error) {
    useApp.getState().setError(error instanceof Error ? error.message : "Message was not sent.");
  }
}

function Modal({ title, onClose, children }: { title: string; onClose: () => void; children: ReactNode }) {
  return (
    <div className="absolute inset-0 z-30 flex items-center justify-center bg-black/60 p-6" onMouseDown={onClose}>
      <div
        className="w-full max-w-xl rounded-lg border border-line bg-side p-4"
        onMouseDown={(event) => event.stopPropagation()}
      >
        <div className="mb-3 flex items-center justify-between">
          <h2 className="text-sm font-semibold">{title}</h2>
          <button className="text-mute" onClick={onClose}>
            Close
          </button>
        </div>
        {children}
      </div>
    </div>
  );
}

function Lightbox() {
  const url = useApp((state) => state.lightboxUrl);
  const [zoom, setZoom] = useState(1);
  const [offset, setOffset] = useState({ x: 0, y: 0 });
  const drag = useRef<{ x: number; y: number; ox: number; oy: number } | null>(null);

  useEffect(() => {
    setZoom(1);
    setOffset({ x: 0, y: 0 });
  }, [url]);

  useEffect(() => {
    if (!url) return;
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") useApp.setState({ lightboxUrl: null });
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [url]);

  if (!url) return null;
  return (
    <div
      className="absolute inset-0 z-40 flex flex-col bg-black/90"
      onMouseDown={() => useApp.setState({ lightboxUrl: null })}
    >
      <div
        className="flex h-12 shrink-0 items-center justify-between border-b border-white/10 px-4 text-sm"
        onMouseDown={(event) => event.stopPropagation()}
      >
        <span className="text-mute">Image · scroll to zoom · drag to pan</span>
        <div className="flex items-center gap-2">
          <button
            type="button"
            className="rounded px-2 py-1 text-mute hover:bg-white/10 hover:text-cream"
            onClick={() => setZoom((value) => Math.max(0.4, value - 0.25))}
          >
            −
          </button>
          <span className="w-12 text-center text-xs text-mute">{Math.round(zoom * 100)}%</span>
          <button
            type="button"
            className="rounded px-2 py-1 text-mute hover:bg-white/10 hover:text-cream"
            onClick={() => setZoom((value) => Math.min(6, value + 0.25))}
          >
            +
          </button>
          <button
            type="button"
            className="rounded px-2 py-1 text-mute hover:bg-white/10 hover:text-cream"
            onClick={() => {
              setZoom(1);
              setOffset({ x: 0, y: 0 });
            }}
          >
            Reset
          </button>
          <button
            type="button"
            className="rounded px-2 py-1 text-mute hover:bg-white/10 hover:text-cream"
            onClick={() => useApp.setState({ lightboxUrl: null })}
          >
            Close
          </button>
        </div>
      </div>
      <div
        className="relative min-h-0 flex-1 cursor-grab overflow-hidden active:cursor-grabbing"
        onMouseDown={(event) => {
          event.stopPropagation();
          drag.current = { x: event.clientX, y: event.clientY, ox: offset.x, oy: offset.y };
        }}
        onMouseMove={(event) => {
          if (!drag.current) return;
          setOffset({
            x: drag.current.ox + (event.clientX - drag.current.x),
            y: drag.current.oy + (event.clientY - drag.current.y),
          });
        }}
        onMouseUp={() => {
          drag.current = null;
        }}
        onMouseLeave={() => {
          drag.current = null;
        }}
        onWheel={(event) => {
          event.preventDefault();
          const delta = event.deltaY > 0 ? -0.12 : 0.12;
          setZoom((value) => Math.min(6, Math.max(0.4, value + delta)));
        }}
      >
        <img
          src={url}
          alt=""
          draggable={false}
          className="pointer-events-none absolute left-1/2 top-1/2 max-h-none max-w-none select-none object-contain"
          style={{
            transform: `translate(calc(-50% + ${offset.x}px), calc(-50% + ${offset.y}px)) scale(${zoom})`,
            transformOrigin: "center center",
            maxHeight: "92%",
            maxWidth: "92%",
          }}
        />
      </div>
    </div>
  );
}

