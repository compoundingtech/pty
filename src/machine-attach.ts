import * as net from "node:net";
import type { Readable, Writable } from "node:stream";
import {
  MachineFrameReader,
  MachineProtocolError,
  DaemonExtensionType,
  decodeDaemonAdmissionV2,
  decodeMachineRequest,
  decodeMachineResponse,
  encodeDaemonOpenV2,
  encodeMachineRequest,
  encodeMachineResponse,
  reduceMachineAttach,
  type MachineAttachState,
  type MachineOutcome,
  type MachineOpenV2,
  type MachineResponse,
  type StreamFailurePhase,
} from "./machine-protocol.ts";
import {
  MessageType,
  encodeStatus,
} from "./protocol.ts";
import { getSocketPath, validateName } from "./sessions.ts";

export interface MachineAttachV2Options {
  readonly input?: Readable;
  readonly output?: Writable;
  readonly diagnostics?: Writable;
  readonly connect?: (socketPath: string) => net.Socket;
  /** The injected socket is already connected (for example, a routed fabric
   *  tunnel). The default local connector still waits for `connect`. */
  readonly preconnected?: boolean;
  readonly signal?: AbortSignal;
}

function diagnostic(stream: Writable, message: string): void {
  stream.write(`pty machine-attach-v2: ${message}\n`);
}

function writeWithBackpressure(stream: Writable, bytes: Buffer): Promise<void> {
  return new Promise<void>((resolve, reject) => {
    let settled = false;
    const cleanup = () => {
      stream.removeListener("error", onError);
      stream.removeListener("close", onClose);
      stream.removeListener("finish", onFinish);
    };
    const succeed = () => {
      if (settled) return;
      settled = true;
      cleanup();
      resolve();
    };
    const fail = (error: Error) => {
      if (settled) return;
      settled = true;
      cleanup();
      reject(error);
    };
    const onError = (error: Error) => fail(error);
    const onClose = () => fail(new Error("stream closed before write completed"));
    const onFinish = () => fail(new Error("stream finished before write completed"));
    if (stream.destroyed || stream.writableEnded) {
      fail(new Error("stream is not writable"));
      return;
    }
    stream.once("error", onError);
    stream.once("close", onClose);
    stream.once("finish", onFinish);
    try {
      stream.write(bytes, (error) => error ? fail(error) : succeed());
    } catch (error) {
      fail(error instanceof Error ? error : new Error(String(error)));
    }
  });
}

/**
 * Bridge one headless machine-attach-v2 stream to the exact daemon selected by
 * its OPEN frame. The bridge never resolves aliases, reads metadata, reserves
 * terminal bytes, or falls back to interactive attach.
 */
export function machineAttachV2(options: MachineAttachV2Options = {}): Promise<MachineOutcome> {
  const input = options.input ?? process.stdin;
  const output = options.output ?? process.stdout;
  const diagnostics = options.diagnostics ?? process.stderr;
  const connect = options.connect ?? ((socketPath: string) => net.createConnection(socketPath));
  const requestReader = new MachineFrameReader();
  const daemonReader = new MachineFrameReader();

  let socket: net.Socket | null = null;
  let phase: "await-open" | "connecting" | "await-admission" | "await-ready" | "streaming" | "detaching" | "finishing" | "finished" = "await-open";
  let attachState: MachineAttachState = { _tag: "AwaitAdmission" };
  let requestedOpen: MachineOpenV2 | null = null;
  let outputWrites = Promise.resolve();
  let daemonWrites = Promise.resolve();
  let resolveCompletion: (outcome: MachineOutcome) => void;
  let rejectCompletion: (error: Error) => void;
  const completion = new Promise<MachineOutcome>((resolve, reject) => {
    resolveCompletion = resolve;
    rejectCompletion = reject;
  });
  const isClosing = (): boolean => phase === "finishing" || phase === "finished";

  const stopInput = (): void => {
    input.pause();
    input.removeListener("data", handleInputData);
    input.removeListener("end", handleInputEnd);
    input.removeListener("error", handleInputError);
    options.signal?.removeEventListener("abort", handleAbort);
  };

  const failBrokenOutput = (error: unknown): void => {
    if (phase === "finished") return;
    phase = "finished";
    stopInput();
    output.removeListener("error", handleOutputError);
    try { socket?.destroy(); } catch {}
    const failure = error instanceof Error ? error : new Error(String(error));
    diagnostic(diagnostics, `stdout failed: ${failure.message}`);
    rejectCompletion(failure);
  };
  const handleOutputError = (error: Error): void => failBrokenOutput(error);

  const enqueueOutput = (response: MachineResponse): void => {
    if (phase === "finished") return;
    const next = reduceMachineAttach(attachState, { _tag: "Frame", frame: response });
    if (next._tag === "ProtocolViolation") {
      finishFailure("stream", "adapter-lifecycle-violation", next.reason);
      return;
    }
    attachState = next;
    outputWrites = outputWrites
      .then(() => writeWithBackpressure(output, encodeMachineResponse(response)))
      .catch((error) => { failBrokenOutput(error); });
  };

  const finish = (outcome: MachineOutcome, alreadyEmitted = false): void => {
    if (isClosing()) return;
    if (!alreadyEmitted) enqueueOutput(outcome);
    phase = "finishing";
    stopInput();
    try { socket?.destroy(); } catch {}
    outputWrites.then(
      () => output.end(() => {
        if (phase !== "finished") {
          const complete = reduceMachineAttach(attachState, { _tag: "Eof" });
          if (complete._tag !== "Complete") {
            const reason = complete._tag === "ProtocolViolation" ? complete.reason : complete._tag;
            failBrokenOutput(new Error(`adapter emitted invalid EOF: ${reason}`));
            return;
          }
          attachState = complete;
          phase = "finished";
          output.removeListener("error", handleOutputError);
          resolveCompletion(outcome);
        }
      }),
      failBrokenOutput,
    );
  };

  const finishFailure = (
    failurePhase: "admission" | StreamFailurePhase,
    reason: string,
    detail?: string,
  ): void => {
    if (isClosing()) return;
    if (attachState._tag === "AwaitAdmission") {
      finish({
        _tag: "AdmissionFailure",
        reason: reason === "not-found"
          ? "not-found"
          : reason === "permission-denied"
            ? "permission-denied"
            : reason === "malformed-request"
              ? "malformed-request"
              : reason === "unsupported-daemon"
                ? "unsupported-daemon"
                : "transport-failure",
        ...(detail === undefined ? {} : { detail }),
      });
      return;
    }
    finish({
      _tag: "StreamFailure",
      phase: failurePhase === "admission" ? "baseline" : failurePhase,
      reason,
      ...(detail === undefined ? {} : { diagnostic: detail }),
    });
  };

  const enqueueDaemon = (bytes: Buffer): void => {
    const target = socket;
    if (!target || target.destroyed) {
      finishFailure("stream", "daemon-connection-closed");
      return;
    }
    daemonWrites = daemonWrites.then(() => writeWithBackpressure(target, bytes));
    const queueTail = daemonWrites;
    input.pause();
    queueTail.then(
      () => { if (queueTail === daemonWrites && phase === "streaming") input.resume(); },
      (error) => finishFailure("stream", "daemon-write-failed", error instanceof Error ? error.message : String(error)),
    );
  };

  const handleDaemonFrame = (frame: { type: number; payload: Buffer }): void => {
    if (isClosing()) return;
    if (phase === "await-admission") {
      if (frame.type === MessageType.STATUS && frame.payload.length > 0) {
        finish({
          _tag: "AdmissionFailure",
          reason: "unsupported-daemon",
          detail: "The session daemon does not support machine attach v2",
        });
        return;
      }
      if (frame.type !== DaemonExtensionType.ADMISSION_V2) {
        finishFailure("admission", "unexpected-daemon-response", `received frame type ${frame.type}`);
        return;
      }
      let admission;
      try { admission = decodeDaemonAdmissionV2(frame); }
      catch (error) {
        finishFailure("admission", "malformed-daemon-admission", (error as Error).message);
        return;
      }
      if (admission._tag === "Rejected") {
        finish({
          _tag: "AdmissionFailure",
          reason: admission.reason,
          ...(admission.detail === undefined ? {} : { detail: admission.detail }),
        });
        return;
      }
      if (!requestedOpen || admission.generation !== requestedOpen.expectedGeneration) {
        finish({
          _tag: "AdmissionFailure",
          reason: "generation-mismatch",
          detail: "Daemon acceptance did not bind the requested generation",
        });
        return;
      }
      const missingCapability = requestedOpen.requiredCapabilities.find(
        (capability) => !admission.capabilities.includes(capability),
      );
      if (missingCapability) {
        finish({
          _tag: "AdmissionFailure",
          reason: "unsupported-capability",
          detail: `Daemon acceptance omitted ${missingCapability}`,
        });
        return;
      }
      enqueueOutput({
        _tag: "Hello",
        protocol: admission.protocol,
        generation: admission.generation,
        capabilities: admission.capabilities,
        build: admission.build,
      });
      phase = "await-ready";
      input.resume();
      return;
    }

    let response: MachineResponse;
    try { response = decodeMachineResponse(frame); }
    catch (error) {
      finishFailure(phase === "await-ready" ? "baseline" : "stream", "malformed-daemon-frame", (error as Error).message);
      return;
    }
    enqueueOutput(response);
    if (response._tag === "Ready") {
      phase = "streaming";
      input.resume();
    } else if (response._tag === "Exited" || response._tag === "Detached" || response._tag === "StreamFailure") {
      finish(response, true);
    }
  };

  const bindDaemon = (target: net.Socket): void => {
    target.on("data", (chunk: Buffer) => {
      target.pause();
      try {
        for (const frame of daemonReader.feed(chunk)) handleDaemonFrame(frame);
      } catch (error) {
        finishFailure(phase === "await-ready" ? "baseline" : "stream", "malformed-daemon-stream", (error as Error).message);
      }
      outputWrites.finally(() => { if (!isClosing() && !target.destroyed) target.resume(); });
    });
    target.on("error", (error: NodeJS.ErrnoException) => {
      const reason = error.code === "ENOENT" || error.code === "ECONNREFUSED"
        ? "not-found"
        : error.code === "EACCES" || error.code === "EPERM"
          ? "permission-denied"
          : "daemon-transport-failed";
      finishFailure(phase === "await-admission" || phase === "connecting" ? "admission" : "stream", reason, error.message);
    });
    target.on("close", () => {
      if (!isClosing()) finishFailure(phase === "await-ready" ? "baseline" : "stream", "daemon-stream-ended");
    });
  };

  const handleInputData = (chunk: Buffer | string): void => {
    if (isClosing()) return;
    let requests;
    try {
      requests = requestReader.feed(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk));
    } catch (error) {
      finishFailure("admission", "malformed-request", (error as Error).message);
      return;
    }
    for (const wireFrame of requests) {
      let request;
      try { request = decodeMachineRequest(wireFrame); }
      catch (error) {
        finishFailure(phase === "await-open" ? "admission" : "stream", "malformed-request", (error as Error).message);
        return;
      }
      if (phase === "await-open") {
        if (request._tag !== "Open") {
          finish({ _tag: "AdmissionFailure", reason: "malformed-request", detail: "OPEN must be the first frame" });
          return;
        }
        try { validateName(request.sessionId); }
        catch (error) {
          finish({ _tag: "AdmissionFailure", reason: "malformed-request", detail: (error as Error).message });
          return;
        }
        phase = "connecting";
        requestedOpen = request;
        input.pause();
        try {
          socket = connect(getSocketPath(request.sessionId));
          bindDaemon(socket);
          const connected = () => {
            if (isClosing()) return;
            phase = "await-admission";
            enqueueDaemon(encodeDaemonOpenV2(request));
            enqueueDaemon(encodeStatus());
          };
          if (options.preconnected) process.nextTick(connected);
          else socket.once("connect", connected);
        } catch (error) {
          finishFailure("admission", "daemon-transport-failed", (error as Error).message);
        }
        continue;
      }
      if (phase === "await-ready" && request._tag === "Detach") {
        phase = "detaching";
        input.pause();
        enqueueDaemon(encodeMachineRequest(request));
        continue;
      }
      if (phase !== "streaming") {
        if (attachState._tag === "AwaitAdmission") {
          finish({ _tag: "AdmissionFailure", reason: "malformed-request", detail: `${request._tag} arrived before HELLO` });
        } else {
          finishFailure("baseline", "request-before-ready", request._tag);
        }
        return;
      }
      switch (request._tag) {
        case "Open":
          finishFailure("stream", "duplicate-open");
          return;
        case "Input":
          enqueueDaemon(encodeMachineRequest(request));
          break;
        case "Resize":
          enqueueDaemon(encodeMachineRequest(request));
          break;
        case "Detach":
          phase = "detaching";
          input.pause();
          enqueueDaemon(encodeMachineRequest(request));
          break;
      }
    }
  };

  const handleInputEnd = (): void => {
    if (!isClosing() && phase !== "detaching") {
      finishFailure(phase === "await-open" || phase === "await-admission" ? "admission" : "shutdown", "request-stream-ended");
    }
  };
  const handleInputError = (error: Error): void => {
    if (!isClosing()) finishFailure(phase === "await-open" ? "admission" : "stream", "request-stream-failed", error.message);
  };
  const handleAbort = (): void => {
    if (!isClosing()) finishFailure(phase === "await-open" || phase === "await-admission" ? "admission" : "shutdown", "cancelled");
  };
  input.on("data", handleInputData);
  input.on("end", handleInputEnd);
  input.on("error", handleInputError);
  output.on("error", handleOutputError);
  if (options.signal) {
    if (options.signal.aborted) {
      handleAbort();
    } else {
      options.signal.addEventListener("abort", handleAbort, { once: true });
    }
  }
  input.resume();

  return completion;
}
