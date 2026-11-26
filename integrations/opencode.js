// Managed by Justless. OpenCode v1 project plugin.
import { execFile } from "node:child_process";
import path from "node:path";

function binary() {
  return process.env.JUSTLESS_BIN || "justless";
}

const readonly = [
  "affected", "code", "context", "discover", "expand", "find", "history", "log",
  "lookup", "map", "pack", "path", "query", "read", "recall", "resume", "search",
  "stats",
];

async function queryTool(directory) {
  try {
    const { tool } = await import("@opencode-ai/plugin");
    return tool({
      description:
        "Read this repository through Justless before reading files blindly. Pack builds " +
        "task context, map lists areas, hubs, communities and entry points, lookup ranks " +
        "code, docs and config for a question, query finds a symbol, its callers, references " +
        "or tests, path traces a relationship between two names, read with --outline shows " +
        "a file's structure, expand prints an exact span, code prints a source range, log " +
        "and search read an archived command run, recall restores a retired tool result, " +
        "affected lists dependents, stats and discover report measured savings.",
      args: {
        command: tool.schema.enum(readonly),
        args: tool.schema.array(tool.schema.string()).optional(),
      },
      async execute({ command, args }) {
        if (!readonly.includes(command)) {
          throw new Error(`justless: ${command} is not a read-only command`);
        }
        // Like the MCP tools, replay archived output only with secrets redacted.
        if ((args ?? []).some((arg) => arg === "--unredacted" || arg.startsWith("--unredacted="))) {
          throw new Error("justless: --unredacted is not available through this tool");
        }
        return new Promise((resolve, reject) => {
          execFile(binary(), ["--root", directory, command, ...(args ?? [])], {
            timeout: 30_000, maxBuffer: 64 * 1024 * 1024, encoding: "utf8",
          }, (error, stdout, stderr) => {
            if (error) return reject(new Error((stderr || error.message).trim() || "justless failed"));
            resolve(stdout);
          });
        });
      },
    });
  } catch {
    return undefined;
  }
}

function mask(directory, model, parts) {
  return new Promise((resolve, reject) => {
    const child = execFile(binary(), ["--root", directory, "mask", "--model", model], {
      timeout: 5000, maxBuffer: 16 * 1024 * 1024, encoding: "utf8",
    }, (error, stdout) => {
      if (error) return reject(error);
      try { resolve(JSON.parse(stdout).messages[0].parts); }
      catch (error) { reject(error); }
    });
    child.stdin.on("error", reject);
    child.stdin.end(JSON.stringify({ messages: [{ info: { role: "assistant" }, parts }] }));
  });
}


function runClaudeHook(directory, mode, payload) {
  return new Promise((resolve) => {
    const child = execFile(binary(), ["hook-claude", mode], {
      cwd: directory, timeout: 10_000, maxBuffer: 64 * 1024 * 1024, encoding: "utf8",
    }, (error, output) => {
      if (error || !output) return resolve();
      try { resolve(JSON.parse(output)); }
      catch { resolve(); }
    });
    child.stdin.on("error", () => resolve());
    child.stdin.end(JSON.stringify(payload));
  });
}

const RETRIEVAL_SUBCOMMANDS = new Set([
  "recall", "log", "expand", "search", "read", "code", "run", "exec",
]);

const RUN_MARKER_RE = /^\[justless run-[A-Za-z0-9]+(?:\]| · )/m;
const RECALL_MARKER_RE = /^\[justless recall [0-9a-f]{64}(?:\]| · )/m;

function isCompactedOutput(text) {
  if (typeof text !== "string" || !text) return false;
  if (text.startsWith("[justless ") || text.startsWith("[justless]")) return true;
  if (text.includes("[justless run-") && RUN_MARKER_RE.test(text)) return true;
  if (text.includes("[justless recall ") && RECALL_MARKER_RE.test(text)) return true;
  return false;
}

function splitShellSegments(command) {
  const segments = [];
  let current = "";
  let inSingle = false;
  let inDouble = false;
  let escaped = false;
  for (let i = 0; i < command.length; i += 1) {
    const ch = command[i];
    if (escaped) {
      current += ch;
      escaped = false;
      continue;
    }
    if (!inSingle && ch === "\\") {
      current += ch;
      escaped = true;
      continue;
    }
    if (!inDouble && ch === "'") {
      inSingle = !inSingle;
      current += ch;
      continue;
    }
    if (!inSingle && ch === "\"") {
      inDouble = !inDouble;
      current += ch;
      continue;
    }
    if (!inSingle && !inDouble) {
      const two = command.slice(i, i + 2);
      if (two === "&&" || two === "||") {
        if (current.trim()) segments.push(current.trim());
        current = "";
        i += 1;
        continue;
      }
      if (ch === ";" || ch === "|" || ch === "\n") {
        if (current.trim()) segments.push(current.trim());
        current = "";
        continue;
      }
    }
    current += ch;
  }
  if (current.trim()) segments.push(current.trim());
  return segments;
}

function tokenizeSegment(segment) {
  const tokens = [];
  let cur = "";
  let inSingle = false;
  let inDouble = false;
  let escaped = false;
  for (let i = 0; i < segment.length; i += 1) {
    const ch = segment[i];
    if (escaped) {
      cur += ch;
      escaped = false;
      continue;
    }
    if (!inSingle && ch === "\\") {
      escaped = true;
      continue;
    }
    if (!inDouble && ch === "'") {
      inSingle = !inSingle;
      continue;
    }
    if (!inSingle && ch === "\"") {
      inDouble = !inDouble;
      continue;
    }
    if (!inSingle && !inDouble && /\s/.test(ch)) {
      if (cur) {
        tokens.push(cur);
        cur = "";
      }
      continue;
    }
    cur += ch;
  }
  if (cur) tokens.push(cur);
  return tokens;
}

function parseJustlessInvocation(segment) {
  const tokens = tokenizeSegment(segment);
  let i = 0;
  while (i < tokens.length) {
    const tok = tokens[i];
    if (/^[A-Za-z_][A-Za-z0-9_]*=/.test(tok)) {
      i += 1;
      continue;
    }
    if (tok === "env" || tok === "command" || tok === "exec" || tok === "nohup" || tok === "time") {
      i += 1;
      while (i < tokens.length && (tokens[i].startsWith("-") || /^[A-Za-z_][A-Za-z0-9_]*=/.test(tokens[i]))) {
        const flag = tokens[i];
        i += 1;
        if (flag === "--") break;
      }
      continue;
    }
    break;
  }
  if (i >= tokens.length) return undefined;
  const exe = tokens[i];
  const base = exe.split(/[\\/]/).pop();
  const configured = binary().split(/[\\/]/).pop();
  if (base !== "justless" && base !== "justless.exe" && base !== configured) {
    return undefined;
  }
  i += 1;
  while (i < tokens.length && tokens[i].startsWith("-")) {
    const flag = tokens[i];
    i += 1;
    if ((flag === "--root" || flag === "-C") && i < tokens.length) {
      i += 1;
    }
  }
  return { subcommand: tokens[i] ?? "" };
}

function isJustlessCommand(command) {
  if (typeof command !== "string" || !command.trim()) return false;
  const segments = splitShellSegments(command);
  if (segments.length === 0) return false;
  if (segments.length === 1) {
    return parseJustlessInvocation(segments[0]) !== undefined;
  }
  return segments.some((seg) => {
    const inv = parseJustlessInvocation(seg);
    return inv !== undefined && RETRIEVAL_SUBCOMMANDS.has(inv.subcommand);
  });
}

async function afterBash(directory, command, stdout, sessionId) {
  // Never re-compact explicit `justless recall|log|expand|search|...` calls or
  // outputs already carrying Justless compaction/recall markers.
  if (isJustlessCommand(command) || isCompactedOutput(stdout)) return undefined;
  const payload = {
    hook_event_name: "PostToolUse",
    tool_name: "Bash",
    tool_input: { command },
    tool_response: { stdout, stderr: "", interrupted: false, isImage: false },
    cwd: directory,
  };
  if (typeof sessionId === "string" && sessionId) payload.session_id = sessionId;
  const parsed = await runClaudeHook(directory, "after-bash", payload);
  const next = parsed?.hookSpecificOutput?.updatedToolOutput?.stdout;
  return typeof next === "string" && next !== stdout && next.length < stdout.length
    ? next
    : undefined;
}

function hasToolError(metadata) {
  if (!metadata || typeof metadata !== "object") return false;
  if (metadata.error || metadata.isError || metadata.truncated === true) return true;
  if (typeof metadata.exit === "number" && metadata.exit !== 0) return true;
  return false;
}

function parsePositiveInt(value) {
  if (typeof value === "number" && Number.isInteger(value) && value >= 0) return value;
  if (typeof value === "string" && /^\d+$/.test(value.trim())) return Number(value.trim());
  return undefined;
}

function extractReadContent(raw) {
  if (
    raw.includes("(File has more lines")
    || raw.includes("(Output truncated")
    || raw.includes("Use 'offset' to read beyond")
  ) {
    return undefined;
  }
  let body = raw;
  let footerTotal;
  const trimmed = raw.trimEnd();
  if (trimmed.startsWith("<file>\n") && trimmed.endsWith("</file>")) {
    let inner = trimmed.slice("<file>\n".length, -"</file>".length);
    const footerMatch = inner.match(/\n?\(End of file - total (\d+) lines\)\n?$/);
    if (footerMatch) {
      footerTotal = Number(footerMatch[1]);
      inner = inner.slice(0, footerMatch.index);
    }
    if (inner.endsWith("\n")) inner = inner.slice(0, -1);
    const numbered = inner.length === 0 ? [] : inner.split("\n");
    const unwrapped = [];
    for (const line of numbered) {
      const m = line.match(/^\s*\d+\| ?(.*)$/);
      if (!m) return undefined;
      unwrapped.push(m[1]);
    }
    body = unwrapped.join("\n");
  } else if (body.endsWith("\n")) {
    body = body.slice(0, -1);
  }
  const numLines = body.length === 0 ? 0 : body.split("\n").length;
  return { content: body, numLines, footerTotal };
}

async function afterRead(directory, args, rawOutput, metadata, sessionId) {
  if (rawOutput.length < 512 || isCompactedOutput(rawOutput)) return;
  if (hasToolError(metadata)) return;
  if (args?.allow_large === true || args?.allow_large === "true" || args?.pages != null) return;
  const rawPath = args?.filePath ?? args?.file_path ?? args?.path;
  if (typeof rawPath !== "string" || !rawPath.trim()) return;
  const extracted = extractReadContent(rawOutput);
  if (!extracted || extracted.numLines === 0) return;

  const hasOffset = args?.offset !== undefined && args?.offset !== null;
  const hasLimit = args?.limit !== undefined && args?.limit !== null;
  let startLine = 1;
  let totalLines = extracted.footerTotal ?? extracted.numLines;
  const toolInput = { file_path: path.isAbsolute(rawPath) ? rawPath : path.resolve(directory, rawPath) };

  if (hasOffset || hasLimit) {
    if (extracted.numLines <= 150) return;
    const offset = hasOffset ? parsePositiveInt(args.offset) : 1;
    const limit = hasLimit ? parsePositiveInt(args.limit) : undefined;
    if (offset === undefined || (hasLimit && limit === undefined)) return;
    const metaTotal = parsePositiveInt(metadata?.totalLines);
    const knownTotal = extracted.footerTotal ?? metaTotal;
    if (knownTotal === undefined) return;
    startLine = offset;
    totalLines = knownTotal;
    if (hasOffset) toolInput.offset = offset;
    if (hasLimit) toolInput.limit = limit;
  }

  const payload = {
    hook_event_name: "PostToolUse",
    tool_name: "Read",
    cwd: directory,
    tool_input: toolInput,
    tool_response: {
      type: "text",
      file: {
        filePath: toolInput.file_path,
        content: extracted.content,
        startLine,
        numLines: extracted.numLines,
        totalLines,
      },
    },
  };
  if (typeof sessionId === "string" && sessionId) payload.session_id = sessionId;
  const parsed = await runClaudeHook(directory, "after-read", payload);
  const content = parsed?.hookSpecificOutput?.updatedToolOutput?.file?.content;
  if (typeof content !== "string" || !content) return;
  const note = parsed?.hookSpecificOutput?.additionalContext;
  const next = typeof note === "string" && note ? `${content}\n\n${note}` : content;
  return next.length < rawOutput.length ? next : undefined;
}

async function afterSearch(directory, kind, args, rawOutput, metadata, sessionId) {
  if (rawOutput.length < 1024 || isCompactedOutput(rawOutput)) return;
  if (hasToolError(metadata)) return;

  const toolInput = {};
  if (typeof args?.pattern === "string") toolInput.pattern = args.pattern;
  if (typeof args?.path === "string") toolInput.path = args.path;
  const globArg = typeof args?.glob === "string"
    ? args.glob
    : (typeof args?.include === "string" ? args.include : undefined);
  if (globArg) toolInput.glob = globArg;

  let payload;
  if (kind === "Grep") {
    const numLines = rawOutput.split("\n").length;
    payload = {
      hook_event_name: "PostToolUse",
      tool_name: "Grep",
      cwd: directory,
      tool_input: toolInput,
      tool_response: { mode: "content", content: rawOutput, numLines, numFiles: 1 },
    };
  } else {
    const filenames = rawOutput.replace(/\r?\n$/, "").split(/\r?\n/).filter(Boolean);
    if (filenames.length < 10) return;
    if (filenames.some((f) => f.startsWith("(") || f.startsWith("No ") || f.startsWith("Found "))) {
      return;
    }
    payload = {
      hook_event_name: "PostToolUse",
      tool_name: "Glob",
      cwd: directory,
      tool_input: toolInput,
      tool_response: { filenames, numFiles: filenames.length, truncated: false },
    };
  }
  if (typeof sessionId === "string" && sessionId) payload.session_id = sessionId;
  const parsed = await runClaudeHook(directory, "after-search", payload);
  const updated = parsed?.hookSpecificOutput?.updatedToolOutput;
  let compact;
  if (kind === "Grep" && typeof updated?.content === "string" && updated.content) {
    compact = updated.content;
  } else if (kind === "Glob" && Array.isArray(updated?.filenames) && updated.filenames.length) {
    compact = updated.filenames.join("\n");
  }
  if (!compact) return;
  const note = parsed?.hookSpecificOutput?.additionalContext;
  const next = typeof note === "string" && note ? `${compact}\n${note}` : compact;
  return next.length < rawOutput.length ? next : undefined;
}

function normalizePathKey(raw) {
  if (typeof raw !== "string") return;
  let s = raw.trim().replace(/^file:\/\//, "").replace(/\/+/g, "/");
  while (s.startsWith("./") && s.length > 2) s = s.slice(2);
  if (s.length > 1) s = s.replace(/\/+$/, "");
  return s || undefined;
}

function partTargetKey(part) {
  const input = part?.state?.input;
  if (!input || typeof input !== "object") return;
  const tool = typeof part.tool === "string" ? part.tool.trim().toLowerCase() : "";
  if (tool === "read" || tool === "read_file" || tool === "view_file") {
    if (input.pages != null || input.ByteOffset != null || input.byte_offset != null) return;
    const p = normalizePathKey(input.filePath ?? input.file_path ?? input.path ?? input.Path ?? input.file);
    if (!p) return;
    const whole = input.offset == null && input.limit == null
      && input.StartLine == null && input.EndLine == null
      && input.start_line == null && input.end_line == null;
    const range = whole
      ? "whole"
      : `${input.offset ?? ""}:${input.limit ?? ""}:${input.StartLine ?? input.start_line ?? ""}:${input.EndLine ?? input.end_line ?? ""}`;
    return { key: `read:${p}:${range}`, readPath: p, wholeRead: whole };
  }
  if (tool === "bash" || tool === "run_command" || tool === "exec_command" || tool === "shell") {
    const cmd = input.command ?? input.CommandLine ?? input.cmd;
    if (typeof cmd !== "string" || !cmd.trim() || isJustlessCommand(cmd)) return;
    const cwd = normalizePathKey(input.cwd ?? input.workdir) ?? "";
    return { key: `bash:${cwd}:${cmd.trim()}` };
  }
  if (tool === "grep" || tool === "grep_search" || tool === "glob" || tool === "find_by_name") {
    const pat = input.pattern ?? input.Query ?? input.query;
    if (typeof pat !== "string" || !pat.trim()) return;
    const p = normalizePathKey(input.path ?? input.directory ?? input.SearchPath) ?? "";
    const extra = `${input.glob ?? ""}|${input.include ?? ""}|${input.type ?? ""}|${input.output_mode ?? ""}`;
    return { key: `${tool}:${p}:${pat.trim()}:${extra}` };
  }
}

// The model decides whether retiring old results is safe. Justless leaves requests
// for Claude models that tie thinking to earlier turns unchanged.
function selectedModel(messages) {
  for (const message of [...(messages ?? [])].reverse()) {
    const info = message?.info;
    if (info?.role === "user" && info.model?.providerID && info.model?.modelID) {
      return `${info.model.providerID}/${info.model.modelID}`;
    }
  }
  for (const message of [...(messages ?? [])].reverse()) {
    const info = message?.info;
    if (info?.role === "assistant" && info.providerID && info.modelID) {
      return `${info.providerID}/${info.modelID}`;
    }
  }
}

// JUSTLESS_DISABLE=1 turns the plugin's hooks off, as it does every Justless hook.
function disabled() {
  const value = process.env.JUSTLESS_DISABLE;
  return Boolean(value) && value !== "0";
}

export const Justless = async ({ directory }) => {
  const hooks = {
  "tool.execute.after": async (input, output) => {
    try {
      if (disabled()) return;
      if (typeof output?.output !== "string" || !output.output) return;
      const tool = input?.tool;
      if (tool === "bash" || tool === "Bash") {
        const command = input?.args?.command;
        if (typeof command !== "string") return;
        // A failed, killed or unreported exit keeps its whole result.
        if (output.metadata?.exit !== 0) return;
        const next = await afterBash(directory, command, output.output, input?.sessionID);
        if (next !== undefined) output.output = next;
        return;
      }
      if (tool === "read" || tool === "Read") {
        const next = await afterRead(
          directory, input?.args, output.output, output.metadata, input?.sessionID,
        );
        if (next !== undefined) output.output = next;
        return;
      }
      if (tool === "grep" || tool === "Grep" || tool === "glob" || tool === "Glob") {
        const kind = (tool === "grep" || tool === "Grep") ? "Grep" : "Glob";
        const next = await afterSearch(
          directory, kind, input?.args, output.output, output.metadata, input?.sessionID,
        );
        if (next !== undefined) output.output = next;
      }
    } catch {
      // Never throw on unexpected host payload shapes; keep the raw output intact.
    }
  },
  "experimental.chat.messages.transform": async (_input, { messages }) => {
    if (disabled()) return;
    const model = selectedModel(messages);
    if (!model) return;
    const locations = [];
    for (const [i, message] of (messages ?? []).entries()) {
      if (message?.info?.role !== "assistant") continue;
      for (const [j, part] of (message.parts ?? []).entries()) {
        if (part?.type === "tool" && ["completed", "error"].includes(part.state?.status)) {
          locations.push({ i, j, part });
        }
      }
    }
    const cutoff = Math.max(0, locations.length - 3);
    // Track duplicate outputs and superseded targets so `justless mask` can retire
    // earlier identical or superseded copies even inside the keep-3 window while
    // keeping unrelated recent outputs out of IPC.
    const counts = new Map();
    const lastIndex = new Map();
    const targetCounts = new Map();
    const lastTargetIndex = new Map();
    const readPathCounts = new Map();
    const lastWholeRead = new Map();
    const targetMeta = new Array(locations.length);
    const isRetirable = (part) => (
      part.state.status === "completed"
      && !part.state.attachments?.length
      && typeof part.state.output === "string"
      && !part.state.output.startsWith("[justless recall ")
    );
    for (const [i, { part }] of locations.entries()) {
      const ok = isRetirable(part);
      if (ok && part.state.output.length >= 256) {
        const out = part.state.output;
        counts.set(out, (counts.get(out) ?? 0) + 1);
        lastIndex.set(out, i);
      }
      if (ok) {
        const meta = partTargetKey(part);
        targetMeta[i] = meta;
        if (meta) {
          targetCounts.set(meta.key, (targetCounts.get(meta.key) ?? 0) + 1);
          lastTargetIndex.set(meta.key, i);
          if (meta.readPath) {
            readPathCounts.set(meta.readPath, (readPathCounts.get(meta.readPath) ?? 0) + 1);
          }
          if (meta.wholeRead) lastWholeRead.set(meta.readPath, i);
        }
      }
    }
    let hasCandidate = false;
    const sent = new Array(locations.length).fill(false);
    const parts = locations.map(({ part }, i) => {
      const canRetire = isRetirable(part);
      const out = canRetire ? part.state.output : "";
      const dup = canRetire && (counts.get(out) ?? 0) > 1;
      const meta = targetMeta[i];
      const supersededLater = meta && (
        i < (lastTargetIndex.get(meta.key) ?? -1)
        || (meta.readPath && i < (lastWholeRead.get(meta.readPath) ?? -1))
      );
      const targetShared = meta && (
        (targetCounts.get(meta.key) ?? 0) > 1
        || (meta.readPath && (readPathCounts.get(meta.readPath) ?? 0) > 1 && lastWholeRead.has(meta.readPath))
      );
      if (canRetire && out.length >= 256 && (i < cutoff || i < (lastIndex.get(out) ?? -1) || supersededLater)) {
        hasCandidate = true;
      }
      const send = canRetire && (i < cutoff || dup || targetShared);
      sent[i] = send;
      const state = {
        status: part.state.status,
        output: send ? out : "",
      };
      if (part.state.input !== undefined) state.input = part.state.input;
      return { type: "tool", callID: part.callID, tool: part.tool, state };
    });
    if (!hasCandidate) return;
    try {
      const masked = await mask(directory, model, parts);
      if (!Array.isArray(masked) || masked.length !== locations.length) return;
      // Copy only changed parts into the outbound view. Keep session records intact.
      for (const [k, { i, j, part }] of locations.entries()) {
        if (!sent[k]) continue;
        const output = masked[k]?.state?.output;
        if (typeof output !== "string" || output === parts[k].state.output) continue;
        const message = messages[i];
        const updated = [...message.parts];
        updated[j] = { ...part, state: { ...part.state, output } };
        messages[i] = { ...message, parts: updated };
      }
    } catch {
      // The host can send the original request if the CLI or its archive is unavailable.
    }
  },
  };

  const tool = await queryTool(directory);
  if (tool) hooks.tool = { justless: tool };
  return hooks;
};
