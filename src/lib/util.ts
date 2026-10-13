import { Channel } from "../types";

export function sortDms(channels: Channel[]): Channel[] {
  return [...(channels ?? [])].sort((a, b) => compareSnowflakeDesc(a.lastMessageId, b.lastMessageId));
}

export function compareSnowflakeDesc(a?: string | null, b?: string | null): number {
  const left = parseSnowflake(a);
  const right = parseSnowflake(b);
  if (left != null && right != null) return right > left ? 1 : right < left ? -1 : 0;
  if (left != null) return -1;
  if (right != null) return 1;
  return 0;
}

export function parseSnowflake(value?: string | null): bigint | null {
  if (!value) return null;
  try {
    return BigInt(value);
  } catch {
    return null;
  }
}

export function byPosition(a: Channel, b: Channel): number {
  return (a.position ?? 0) - (b.position ?? 0);
}

export function clock(iso: string): string {
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) return "";
  return date.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}
