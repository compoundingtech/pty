export const ACTIVITY_STATES = [
  "unknown",
  "active",
  "child_command",
  "idle",
] as const;

export type ActivityState = (typeof ACTIVITY_STATES)[number];
/** Alias used by stable model-free fixture schemas. */
export type ActivityFixtureState = ActivityState;

export interface ActivityStatus {
  state: ActivityState;
  generation: string;
  producerEpoch: string | null;
  sequence: number;
  turnId?: string;
  source?: string;
}

export interface ActivityClaim {
  op: "claim";
  producerEpoch: string;
  source?: string;
}

export interface ActivityUpdate {
  op: "set";
  producerEpoch: string;
  sequence: number;
  state: ActivityState;
  turnId?: string;
}

export type ActivityCommand = ActivityClaim | ActivityUpdate;

export interface ActivityResponse {
  ok: boolean;
  activity: ActivityStatus;
  error?: string;
}

const MAX_ACTIVITY_PAYLOAD_BYTES = 4096;
const MAX_EPOCH_LENGTH = 128;
const MAX_SOURCE_LENGTH = 64;
const MAX_TURN_ID_LENGTH = 256;

function boundedString(
  value: unknown,
  maxLength: number,
  optional = false,
): boolean {
  if (value === undefined && optional) return true;
  return typeof value === "string" && value.length > 0 && value.length <= maxLength;
}

export function parseActivityCommand(payload: Buffer): ActivityCommand | null {
  if (payload.length === 0 || payload.length > MAX_ACTIVITY_PAYLOAD_BYTES) return null;
  try {
    const parsed: unknown = JSON.parse(payload.toString("utf8"));
    if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) return null;
    const value = parsed as Record<string, unknown>;
    const producerEpoch = value.producerEpoch;
    if (
      typeof producerEpoch !== "string" ||
      !boundedString(producerEpoch, MAX_EPOCH_LENGTH)
    ) return null;
    if (value.op === "claim") {
      if (!boundedString(value.source, MAX_SOURCE_LENGTH, true)) return null;
      return {
        op: "claim",
        producerEpoch,
        ...(value.source === undefined ? {} : { source: value.source as string }),
      };
    }
    if (value.op !== "set") return null;
    if (
      !Number.isSafeInteger(value.sequence) ||
      Number(value.sequence) <= 0 ||
      !ACTIVITY_STATES.includes(value.state as ActivityState) ||
      !boundedString(value.turnId, MAX_TURN_ID_LENGTH, true)
    ) {
      return null;
    }
    return {
      op: "set",
      producerEpoch,
      sequence: Number(value.sequence),
      state: value.state as ActivityState,
      ...(value.turnId === undefined ? {} : { turnId: value.turnId as string }),
    };
  } catch {
    return null;
  }
}

export class ActivityLease<Owner extends object = object> {
  private owner: Owner | null = null;
  private status: ActivityStatus;

  constructor(private readonly generation: string) {
    this.status = this.unknownStatus();
  }

  snapshot(): ActivityStatus {
    return { ...this.status };
  }

  apply(owner: Owner, command: ActivityCommand): ActivityResponse {
    if (command.op === "claim") {
      if (this.owner !== null && this.owner !== owner) {
        return this.failure("activity lease already held");
      }
      this.owner = owner;
      this.status = {
        state: "unknown",
        generation: this.generation,
        producerEpoch: command.producerEpoch,
        sequence: 0,
        ...(command.source === undefined ? {} : { source: command.source }),
      };
      return { ok: true, activity: this.snapshot() };
    }

    if (this.owner !== owner) {
      return this.failure("activity lease identity mismatch");
    }
    if (this.status.producerEpoch !== command.producerEpoch) {
      this.release(owner);
      return this.failure("activity lease identity mismatch");
    }
    const expectedSequence = this.status.sequence + 1;
    if (command.sequence !== expectedSequence) {
      this.release(owner);
      return this.failure(`activity sequence must be ${expectedSequence}`);
    }
    this.status = {
      state: command.state,
      generation: this.generation,
      producerEpoch: command.producerEpoch,
      sequence: command.sequence,
      ...(command.turnId === undefined ? {} : { turnId: command.turnId }),
      ...(this.status.source === undefined ? {} : { source: this.status.source }),
    };
    return { ok: true, activity: this.snapshot() };
  }

  release(owner: Owner): void {
    if (this.owner !== owner) return;
    this.owner = null;
    this.status = this.unknownStatus();
  }

  private failure(error: string): ActivityResponse {
    return { ok: false, error, activity: this.snapshot() };
  }

  private unknownStatus(): ActivityStatus {
    return {
      state: "unknown",
      generation: this.generation,
      producerEpoch: null,
      sequence: 0,
    };
  }
}
