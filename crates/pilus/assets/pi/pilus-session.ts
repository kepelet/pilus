// installed by pilus; reinstalling or updating pilus overwrites this file.
// add custom extensions beside this file instead of editing it.
// PILUS_INTEGRATION_VERSION=2
// @ts-nocheck
//
// Two jobs, both keyed by the session this pi process is in:
//
// 1. Tell the Luvus daemon which session this pane is in
//    (`pane.report_session`). Pi writes its session with appendFileSync and
//    holds no file open, so this report is the only reliable link from a
//    pane to its conversation.
// 2. Serve a small RPC socket at `<agent dir>/pilus/<session id>.sock` so
//    Pilus can follow the live turn (streaming text, tool output, busy
//    state, context usage) and drive it natively (prompt, steer, follow-up,
//    abort, model, thinking) instead of typing into the terminal.
//
// Protocol (newline-delimited JSON, protocol 1):
//   server -> client, pushed:
//     {"type":"hello","protocol":1,"sessionId","sessionFile","pid"}
//     {"type":"state","busy","blocked","pending","model","thinking","context"}
//     {"type":"stream","message":<partial assistant message>}
//     {"type":"stream_end"}
//     {"type":"tool","phase":"start"|"update"|"end","toolCallId","toolName",
//      "output","isError"}
//   client -> server: {"id","cmd",...}; answered by
//     {"type":"response","id","ok","error"?,"data"?}
//     cmd: "prompt" {text, deliverAs?: "steer"|"followUp"} | "abort"
//          | "set_model" {provider, model} | "set_thinking" {level}
//          | "models" | "state"

import { createConnection, createServer } from "node:net";
import { chmodSync, mkdirSync, rmSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

const PROTOCOL = 1;
const socketPath = process.env.LUVUS_API_ADDRESS || process.env.LUVUS_SOCKET_PATH;
const pane = process.env.LUVUS_PANE_ID;
// The daemon's `session_id` schema; it also keeps the socket name a plain
// file name.
const SESSION_ID = /^[A-Za-z0-9._:-]{1,256}$/;
// Unix socket paths are capped near 104 bytes on macOS; past that the RPC
// channel is skipped and Pilus falls back to reading the file.
const MAX_SOCKET_PATH = 100;
// Streaming deltas coalesce to this cadence; the panel cannot paint faster
// than a few frames per update anyway.
const STREAM_MS = 50;
// Tool output is a live tail, not a transcript; the full result lands in
// the session file.
const TOOL_TAIL = 4000;

function agentDir(): string {
  return process.env.PI_CODING_AGENT_DIR || join(homedir(), ".pi", "agent");
}

function reportingEnabled(): boolean {
  return process.env.LUVUS_ENV === "1" && !!socketPath && !!pane;
}

// The control socket answers one request per connection, so every call opens
// its own. Failures are swallowed: a missing daemon must never break pi.
function send(method: string, params: Record<string, unknown>): Promise<void> {
  if (!reportingEnabled()) return Promise.resolve();
  return new Promise((resolve) => {
    let done = false;
    const socket = createConnection(socketPath!);
    const finish = () => {
      if (done) return;
      done = true;
      socket.destroy();
      resolve();
    };
    socket.on("error", finish);
    socket.on("connect", () =>
      socket.write(`${JSON.stringify({ id: `pilus-${Date.now()}`, method, params })}\n`),
    );
    socket.on("data", finish);
    socket.on("end", finish);
    const timeout = setTimeout(finish, 500);
    timeout.unref?.();
  });
}

let reported: string | undefined;

async function release(): Promise<void> {
  if (!reported) return;
  const session_id = reported;
  reported = undefined;
  await send("pane.release_session", { agent: "pi", pane, session_id });
}

export default function (pi) {
  // The latest context from any event; commands arrive between events and
  // need one to act on.
  let ctx;
  let busy = false;
  let blocked = false;
  let server;
  let serverPath: string | undefined;
  let sidecarPath: string | undefined;
  const clients = new Set();
  let pendingStream;
  let streamTimer;
  const toolTimers = new Map();

  const remember = (eventCtx) => {
    if (eventCtx) ctx = eventCtx;
  };

  const broadcast = (event) => {
    if (clients.size === 0) return;
    const line = `${JSON.stringify(event)}\n`;
    for (const client of clients) {
      if (!client.destroyed) client.write(line);
    }
  };

  const modelInfo = (model) =>
    model
      ? {
          provider: model.provider,
          id: model.id,
          name: model.name ?? model.id,
          contextWindow: model.contextWindow ?? null,
          reasoning: !!model.reasoning,
        }
      : null;

  const state = () => {
    let context = null;
    try {
      const usage = ctx?.getContextUsage?.();
      if (usage) {
        context = {
          tokens: usage.tokens ?? null,
          contextWindow: usage.contextWindow,
          percent: usage.percent ?? null,
        };
      }
    } catch {}
    let thinking = null;
    try {
      thinking = pi.getThinkingLevel?.() ?? ctx?.thinkingLevel ?? null;
    } catch {}
    let pending = false;
    try {
      pending = !!ctx?.hasPendingMessages?.();
    } catch {}
    return {
      type: "state",
      busy,
      blocked,
      pending,
      model: modelInfo(ctx?.model),
      thinking,
      context,
    };
  };

  const pushState = () => broadcast(state());

  const flushStream = () => {
    streamTimer = undefined;
    if (!pendingStream) return;
    const message = pendingStream;
    pendingStream = undefined;
    broadcast({ type: "stream", message });
  };

  const textOf = (result) => {
    const blocks = Array.isArray(result?.content) ? result.content : [];
    const text = blocks
      .filter((block) => block?.type === "text" && typeof block.text === "string")
      .map((block) => block.text)
      .join("\n");
    return text.length > TOOL_TAIL ? text.slice(text.length - TOOL_TAIL) : text;
  };

  const respond = (client, id, ok, extra = {}) => {
    if (client.destroyed) return;
    client.write(`${JSON.stringify({ type: "response", id, ok, ...extra })}\n`);
  };

  const handle = async (client, request) => {
    const { id, cmd } = request ?? {};
    try {
      switch (cmd) {
        case "prompt": {
          const text = typeof request.text === "string" ? request.text.trim() : "";
          if (!text) return respond(client, id, false, { error: "empty prompt" });
          const deliverAs = request.deliverAs === "followUp" ? "followUp" : "steer";
          const idle = ctx?.isIdle?.() ?? !busy;
          pi.sendUserMessage(text, idle ? { expandPromptTemplates: true } : {
            deliverAs,
            expandPromptTemplates: true,
          });
          respond(client, id, true);
          pushState();
          return;
        }
        case "abort":
          ctx?.abort?.();
          return respond(client, id, true);
        case "set_model": {
          const model = ctx?.modelRegistry?.find?.(request.provider, request.model);
          if (!model) return respond(client, id, false, { error: "unknown model" });
          const changed = await pi.setModel(model);
          if (!changed) return respond(client, id, false, { error: "model is not authenticated" });
          respond(client, id, true);
          pushState();
          return;
        }
        case "set_thinking":
          pi.setThinkingLevel(request.level);
          respond(client, id, true);
          pushState();
          return;
        case "models": {
          const models = (ctx?.modelRegistry?.getAvailable?.() ?? []).map(modelInfo);
          return respond(client, id, true, { data: { models } });
        }
        case "state":
          return respond(client, id, true, { data: state() });
        default:
          return respond(client, id, false, { error: `unknown command ${cmd}` });
      }
    } catch (error) {
      respond(client, id, false, { error: String(error?.message ?? error) });
    }
  };

  const stopServer = () => {
    for (const client of clients) client.destroy();
    clients.clear();
    server?.close();
    server = undefined;
    for (const path of [serverPath, sidecarPath]) {
      if (path) rmSync(path, { force: true });
    }
    serverPath = undefined;
    sidecarPath = undefined;
  };

  const startServer = (sessionId: string, sessionFile: string) => {
    stopServer();
    const dir = join(agentDir(), "pilus");
    const path = join(dir, `${sessionId}.sock`);
    if (Buffer.byteLength(path) > MAX_SOCKET_PATH) return;
    try {
      mkdirSync(dir, { recursive: true, mode: 0o700 });
      // A crashed pi leaves its socket behind; the session is ours now.
      rmSync(path, { force: true });
    } catch {
      return;
    }
    const next = createServer((client) => {
      clients.add(client);
      client.setEncoding("utf8");
      let buffer = "";
      client.on("data", (chunk) => {
        buffer += chunk;
        let newline;
        while ((newline = buffer.indexOf("\n")) !== -1) {
          const line = buffer.slice(0, newline).trim();
          buffer = buffer.slice(newline + 1);
          if (!line) continue;
          let request;
          try {
            request = JSON.parse(line);
          } catch {
            continue;
          }
          void handle(client, request);
        }
      });
      const drop = () => clients.delete(client);
      client.on("close", drop);
      client.on("error", drop);
      client.write(
        `${JSON.stringify({
          type: "hello",
          protocol: PROTOCOL,
          sessionId,
          sessionFile,
          pid: process.pid,
        })}\n`,
      );
      client.write(`${JSON.stringify(state())}\n`);
    });
    next.on("error", () => stopServer());
    next.listen(path, () => {
      try {
        chmodSync(path, 0o600);
      } catch {}
      // The sidecar tells a remote Pilus which runtime can relay the socket
      // over SSH, since a non-interactive shell may not have node on PATH.
      sidecarPath = join(dir, `${sessionId}.json`);
      try {
        writeFileSync(
          sidecarPath,
          JSON.stringify({ protocol: PROTOCOL, pid: process.pid, socket: path, execPath: process.execPath }),
          { mode: 0o600 },
        );
      } catch {}
    });
    next.unref?.();
    server = next;
    serverPath = path;
  };

  // Fires on startup, /new, /resume, /fork, and reload, each time with the
  // session now active, so one handler keeps the daemon and socket current.
  pi.on("session_start", async (_event, eventCtx) => {
    remember(eventCtx);
    const manager = eventCtx?.sessionManager;
    const file = manager?.getSessionFile?.();
    // `--no-session` runs have no file to read; report nothing for them.
    const id = file ? manager?.getSessionId?.() : undefined;
    if (!id || !SESSION_ID.test(id)) {
      stopServer();
      await release();
      return;
    }
    startServer(id, file);
    if (id === reported) return;
    await release();
    reported = id;
    await send("pane.report_session", { agent: "pi", pane, session_id: id });
  });

  // A switch is followed by `session_start` for the next session, which
  // replaces the report and the socket; only quitting leaves neither.
  pi.on("session_shutdown", async (event, eventCtx) => {
    remember(eventCtx);
    stopServer();
    if (event?.reason === "quit") await release();
  });

  pi.on("agent_start", (_event, eventCtx) => {
    remember(eventCtx);
    busy = true;
    pushState();
  });
  // `agent_settled` is the point pi will not continue on its own; a queued
  // follow-up starts a new run with its own `agent_start`.
  pi.on("agent_settled", (_event, eventCtx) => {
    remember(eventCtx);
    busy = false;
    pushState();
  });

  pi.on("message_update", (event, eventCtx) => {
    remember(eventCtx);
    if (event?.message?.role !== "assistant") return;
    pendingStream = event.message;
    if (!streamTimer) {
      streamTimer = setTimeout(flushStream, STREAM_MS);
      streamTimer.unref?.();
    }
  });
  pi.on("message_end", (event, eventCtx) => {
    remember(eventCtx);
    if (event?.message?.role !== "assistant") return;
    if (streamTimer) clearTimeout(streamTimer);
    streamTimer = undefined;
    pendingStream = undefined;
    broadcast({ type: "stream_end" });
    pushState();
  });

  pi.on("tool_execution_start", (event, eventCtx) => {
    remember(eventCtx);
    broadcast({
      type: "tool",
      phase: "start",
      toolCallId: event.toolCallId,
      toolName: event.toolName,
      output: "",
      isError: false,
    });
  });
  pi.on("tool_execution_update", (event, eventCtx) => {
    remember(eventCtx);
    const key = event.toolCallId;
    toolTimers.set(key, event);
    if (toolTimers.get(`${key}:timer`)) return;
    const timer = setTimeout(() => {
      const latest = toolTimers.get(key);
      toolTimers.delete(key);
      toolTimers.delete(`${key}:timer`);
      if (!latest) return;
      broadcast({
        type: "tool",
        phase: "update",
        toolCallId: latest.toolCallId,
        toolName: latest.toolName,
        output: textOf(latest.partialResult),
        isError: false,
      });
    }, STREAM_MS * 2);
    timer.unref?.();
    toolTimers.set(`${key}:timer`, timer);
  });
  pi.on("tool_execution_end", (event, eventCtx) => {
    remember(eventCtx);
    const timer = toolTimers.get(`${event.toolCallId}:timer`);
    if (timer) clearTimeout(timer);
    toolTimers.delete(event.toolCallId);
    toolTimers.delete(`${event.toolCallId}:timer`);
    broadcast({
      type: "tool",
      phase: "end",
      toolCallId: event.toolCallId,
      toolName: event.toolName,
      output: textOf(event.result),
      isError: !!event.isError,
    });
  });

  pi.on("ui_prompt_start", (_event, eventCtx) => {
    remember(eventCtx);
    blocked = true;
    pushState();
  });
  pi.on("ui_prompt_end", (_event, eventCtx) => {
    remember(eventCtx);
    blocked = false;
    pushState();
  });

  for (const name of ["turn_end", "model_select", "thinking_level_select", "session_compact"]) {
    pi.on(name, (_event, eventCtx) => {
      remember(eventCtx);
      pushState();
    });
  }
}
