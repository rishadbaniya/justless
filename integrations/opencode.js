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

const JUSTLESS_SUBCOMMANDS = new Set([
  "affected", "code", "context", "discover", "doctor", "drain", "edit", "exec",
  "expand", "find", "history", "index", "init", "learn", "log", "lookup", "map",
  "mask", "mcp", "pack", "path", "proxy", "query", "read", "recall", "rename",
  "resume", "review", "rewrite", "run", "search", "stats", "statusline", "test",
  "wrap",
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

function inspectSegment(segment, depth = 0) {
  const tokens = [...tokenizeSegment(segment)];
  let i = 0;
  let disableVal;
  while (i < tokens.length) {
    // Strip leading shell grouping tokens `(` / `{` (standalone or attached).
    const tok = tokens[i].replace(/^[({]+/, "");
    if (!tok) {
      i += 1;
      continue;
    }
    tokens[i] = tok;
    if (tok === "!") {
      i += 1;
      continue;
    }
    const assign = tok.match(/^([A-Za-z_][A-Za-z0-9_]*)=(.*)$/);
    if (assign) {
      if (assign[1] === "JUSTLESS_DISABLE") disableVal = assign[2];
      i += 1;
      continue;
    }
    if (tok === "export") {
      i += 1;
      while (i < tokens.length) {
        const next = tokens[i].replace(/[)}]+$/, "");
        if (next.startsWith("-")) {
          i += 1;
          continue;
        }
        const exp = next.match(/^([A-Za-z_][A-Za-z0-9_]*)=(.*)$/);
        if (exp) {
          if (exp[1] === "JUSTLESS_DISABLE") disableVal = exp[2];
          i += 1;
          continue;
        }
        break;
      }
      continue;
    }
    if (tok === "env") {
      i += 1;
      while (i < tokens.length) {
        const arg = tokens[i];
        if (arg === "--") {
          i += 1;
          break;
        }
        const envAssign = arg.match(/^([A-Za-z_][A-Za-z0-9_]*)=(.*)$/);
        if (envAssign) {
          if (envAssign[1] === "JUSTLESS_DISABLE") disableVal = envAssign[2];
          i += 1;
          continue;
        }
        if (!arg.startsWith("-") || arg === "-") break;
        i += 1;
        if (arg === "-i" || arg === "--ignore-environment") {
          disableVal = undefined;
          continue;
        }
        if (arg === "-u" || arg === "--unset" || /^-[i0v]*u$/.test(arg)) {
          if (arg.includes("i")) disableVal = undefined;
          if (i < tokens.length) {
            if (tokens[i] === "JUSTLESS_DISABLE") disableVal = undefined;
            i += 1;
          }
          continue;
        }
        if (arg.startsWith("--unset=") || (arg.startsWith("-u") && arg.length > 2)) {
          const unsetName = arg.startsWith("--unset=") ? arg.slice("--unset=".length) : arg.slice(2);
          if (unsetName === "JUSTLESS_DISABLE") disableVal = undefined;
          continue;
        }
        if (arg === "-C" || arg === "--chdir" || /^-[i0v]*C$/.test(arg)) {
          if (arg.includes("i")) disableVal = undefined;
          if (i < tokens.length) i += 1;
          continue;
        }
        if (arg === "-S" || arg === "--split-string" || /^-[i0v]*S$/.test(arg)) {
          if (arg.includes("i")) disableVal = undefined;
          if (i < tokens.length) {
            const splitArg = tokens[i];
            i += 1;
            if (depth < 4) {
              tokens.splice(i, 0, ...tokenizeSegment(splitArg));
              depth += 1;
            }
          }
          continue;
        }
        if (arg.startsWith("--split-string=") || (arg.startsWith("-S") && arg.length > 2)) {
          const splitArg = arg.startsWith("--split-string=")
            ? arg.slice("--split-string=".length)
            : arg.slice(2);
          if (depth < 4) {
            tokens.splice(i, 0, ...tokenizeSegment(splitArg));
            depth += 1;
          }
          continue;
        }
      }
      continue;
    }
    if (tok === "command" || tok === "exec" || tok === "nohup" || tok === "time") {
      i += 1;
      while (i < tokens.length && tokens[i].startsWith("-")) {
        const flag = tokens[i];
        i += 1;
        if (flag === "--") break;
        if (tok === "exec" && flag === "-a" && i < tokens.length) i += 1;
      }
      continue;
    }
    break;
  }
  const inlineDisabled = Boolean(disableVal) && disableVal !== "0";
  if (i >= tokens.length) return { inlineDisabled, invocation: undefined };
  const exe = tokens[i].replace(/[)}]+$/, "");
  const base = exe.split(/[\\/]/).pop();
  const configured = binary().split(/[\\/]/).pop();
  if (base !== "justless" && base !== "justless.exe" && base !== configured) {
    return { inlineDisabled, invocation: undefined };
  }
  i += 1;
  while (i < tokens.length && tokens[i].startsWith("-")) {
    const flag = tokens[i];
    i += 1;
    if ((flag === "--root" || flag === "-C") && i < tokens.length) {
      i += 1;
    }
  }
  const subcommand = (tokens[i] ?? "").replace(/[)}]+$/, "");
  return { inlineDisabled, invocation: { subcommand } };
}

function parseJustlessInvocation(segment) {
  return inspectSegment(segment).invocation;
}

function hasInlineDisable(command) {
  if (typeof command !== "string" || !command.trim()) return false;
  return splitShellSegments(command).some((seg) => inspectSegment(seg).inlineDisabled);
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
    return inv !== undefined && JUSTLESS_SUBCOMMANDS.has(inv.subcommand);
  });
}

async function afterBash(directory, command, stdout, sessionId) {
  // Never re-compact explicit `justless ...` calls, commands with inline
  // `JUSTLESS_DISABLE=<non-0>`, or outputs already carrying Justless markers.
  if (hasInlineDisable(command) || isJustlessCommand(command) || isCompactedOutput(stdout)) {
    return undefined;
  }
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

function normalizeCommand(raw) {
  if (typeof raw !== "string") return;
  const trimmed = raw.trim();
  if (!trimmed) return;
  let out = "";
  let inSingle = false;
  let inDouble = false;
  let escaped = false;
  let pendingSpace = false;
  for (let i = 0; i < trimmed.length; i += 1) {
    const ch = trimmed[i];
    if (escaped) {
      if (pendingSpace && out) {
        out += " ";
        pendingSpace = false;
      }
      out += ch;
      escaped = false;
      continue;
    }
    if (!inSingle && ch === "\\") {
      if (pendingSpace && out) {
        out += " ";
        pendingSpace = false;
      }
      out += ch;
      escaped = true;
      continue;
    }
    if (!inDouble && ch === "'") {
      if (pendingSpace && out) {
        out += " ";
        pendingSpace = false;
      }
      inSingle = !inSingle;
      out += ch;
      continue;
    }
    if (!inSingle && ch === "\"") {
      if (pendingSpace && out) {
        out += " ";
        pendingSpace = false;
      }
      inDouble = !inDouble;
      out += ch;
      continue;
    }
    if (!inSingle && !inDouble && /\s/.test(ch)) {
      pendingSpace = true;
    } else {
      if (pendingSpace && out) {
        out += " ";
        pendingSpace = false;
      }
      out += ch;
    }
  }
  return out || undefined;
}

function optUintField(input, keys) {
  for (const key of keys) {
    const val = input[key];
    if (val !== undefined && val !== null) {
      const n = parsePositiveInt(val);
      if (n === undefined) return { ok: false };
      return { ok: true, value: n };
    }
  }
  return { ok: true, value: undefined };
}

function scopeFingerprint(input, keys) {
  const parts = [];
  for (const key of keys) {
    const val = input[key];
    if (val !== undefined && val !== null) {
      parts.push(`${key}=${JSON.stringify(val)}`);
    }
  }
  return parts.join("\0");
}

function partTargetKey(part) {
  const input = part?.state?.input;
  if (!input || typeof input !== "object" || Array.isArray(input)) return;
  const tool = typeof part.tool === "string" ? part.tool.trim().toLowerCase() : "";
  if (tool === "read" || tool === "read_file" || tool === "view_file") {
    if (input.pages != null || input.ByteOffset != null || input.byte_offset != null) return;
    const p = normalizePathKey(
      input.file_path ?? input.filePath ?? input.path ?? input.Path ?? input.AbsolutePath ?? input.file,
    );
    if (!p) return;
    const hasLineSpan = input.StartLine != null || input.start_line != null || input.startLine != null
      || input.EndLine != null || input.end_line != null || input.endLine != null;
    let whole = false;
    let range;
    if (hasLineSpan) {
      const start = optUintField(input, ["StartLine", "start_line", "startLine"]);
      const end = optUintField(input, ["EndLine", "end_line", "endLine"]);
      if (!start.ok || !end.ok) return;
      if (start.value === undefined && end.value === undefined) {
        whole = true;
        range = "whole";
      } else {
        range = `lines:${start.value ?? ""}:${end.value ?? ""}`;
      }
    } else {
      const offset = optUintField(input, ["offset", "Offset"]);
      const limit = optUintField(input, ["limit", "Limit"]);
      if (!offset.ok || !limit.ok) return;
      if (offset.value === undefined && limit.value === undefined) {
        whole = true;
        range = "whole";
      } else {
        range = `offset:${offset.value ?? ""}:${limit.value ?? ""}`;
      }
    }
    return { key: `read:${p}:${range}`, readPath: p, wholeRead: whole };
  }
  if (
    tool === "bash" || tool === "run_command" || tool === "exec_command"
    || tool === "shell" || tool === "sh" || tool === "terminal"
  ) {
    const rawCmd = input.command ?? input.CommandLine ?? input.cmd ?? input.script;
    const cmd = normalizeCommand(rawCmd);
    if (!cmd || isJustlessCommand(cmd)) return;
    const cwd = normalizePathKey(input.cwd ?? input.workdir ?? input.Cwd ?? input.working_directory) ?? "";
    return { key: `bash:${cwd}:${cmd}` };
  }
  if (tool === "grep" || tool === "grep_search" || tool === "ripgrep" || tool === "rg") {
    const pat = input.pattern ?? input.Query ?? input.query ?? input.regex;
    if (typeof pat !== "string" || !pat.trim()) return;
    const p = normalizePathKey(
      input.path ?? input.SearchPath ?? input.directory ?? input.dir ?? input.cwd ?? input.file,
    ) ?? "";
    const scope = scopeFingerprint(input, [
      "-A", "-B", "-C", "-i", "-n",
      "CaseInsensitive", "FixedStrings", "Includes", "MatchPerLine",
      "context", "glob", "head_limit", "include", "multiline", "offset", "output_mode", "type",
    ]);
    return { key: `grep:${p}:${pat.trim()}:${scope}` };
  }
  if (tool === "glob" || tool === "find_by_name" || tool === "list_files") {
    const pat = input.pattern ?? input.Pattern ?? input.glob;
    if (typeof pat !== "string" || !pat.trim()) return;
    const p = normalizePathKey(
      input.path ?? input.SearchDirectory ?? input.directory ?? input.dir ?? input.cwd,
    ) ?? "";
    const scope = scopeFingerprint(input, ["Excludes", "Extensions", "MaxDepth", "Type", "type"]);
    return { key: `glob:${p}:${pat.trim()}:${scope}` };
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
      // Only forward `input` when the real output is also sent; blanked unsent
      // parts (already-retired stubs, attachments, errors, or non-shared calls)
      // must never look like valid superseding calls to `justless mask`.
      if (send && part.state.input !== undefined) state.input = part.state.input;
      if (!send && part.state.attachments?.length) state.attachments = [true];
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
