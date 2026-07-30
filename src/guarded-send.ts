export const MAX_GUARDED_DATA_BYTES = 64 * 1024;

export interface GuardedSendCommand {
  generation: string;
  ioRevision: number;
  data: string;
}

export interface GuardedSendResponse {
  ok: boolean;
  generation: string;
  ioRevision: number;
  error?: string;
}

// JSON control-character escaping can expand one input byte to six wire bytes
// (for example NUL becomes "\\u0000"). Keep the wire packet bounded while
// accepting every payload whose decoded UTF-8 data is within the public cap.
const MAX_GUARDED_COMMAND_BYTES = MAX_GUARDED_DATA_BYTES * 6 + 512;
const MAX_GENERATION_LENGTH = 128;
const GUARDED_SEND_KEYS = new Set(["generation", "ioRevision", "data"]);

export function parseGuardedSendCommand(
  payload: Buffer,
): GuardedSendCommand | null {
  if (payload.length === 0 || payload.length > MAX_GUARDED_COMMAND_BYTES) {
    return null;
  }
  try {
    const parsed: unknown = JSON.parse(payload.toString("utf8"));
    if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
      return null;
    }
    const value = parsed as Record<string, unknown>;
    if (
      Object.keys(value).some((key) => !GUARDED_SEND_KEYS.has(key)) ||
      typeof value.generation !== "string" ||
      value.generation.length === 0 ||
      value.generation.length > MAX_GENERATION_LENGTH ||
      !Number.isSafeInteger(value.ioRevision) ||
      Number(value.ioRevision) < 0 ||
      typeof value.data !== "string" ||
      value.data.length === 0 ||
      Buffer.byteLength(value.data, "utf8") > MAX_GUARDED_DATA_BYTES
    ) {
      return null;
    }
    return {
      generation: value.generation,
      ioRevision: Number(value.ioRevision),
      data: value.data,
    };
  } catch {
    return null;
  }
}
