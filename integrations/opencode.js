// Managed by Justless. OpenCode v1 project plugin.
import { execFile } from "node:child_process";

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


function afterBash(directory, command, stdout) {
  return new Promise((resolve) => {
    const child = execFile(binary(), ["hook-claude", "after-bash"], {
      cwd: directory, timeout: 10_000, maxBuffer: 64 * 1024 * 1024, encoding: "utf8",
    }, (error, output) => {
      if (error) return resolve();
      try {
        const next = JSON.parse(output).hookSpecificOutput?.updatedToolOutput?.stdout;
        resolve(typeof next === "string" && next !== stdout ? next : undefined);
      } catch { resolve(); }
    });
    child.stdin.on("error", () => resolve());
    child.stdin.end(JSON.stringify({
      hook_event_name: "PostToolUse",
      tool_name: "Bash",
      tool_input: { command },
      tool_response: { stdout, stderr: "", interrupted: false, isImage: false },
      cwd: directory,
    }));
  });
}

// The model decides whether retiring old results is safe. Justless leaves requests
// for Claude models that tie thinking to earlier turns unchanged.
function selectedModel(messages) {
  for (const { info } of [...messages].reverse()) {
    if (info.role === "user" && info.model?.providerID && info.model?.modelID) {
      return `${info.model.providerID}/${info.model.modelID}`;
    }
  }
  for (const { info } of [...messages].reverse()) {
    if (info.role === "assistant" && info.providerID && info.modelID) {
      return `${info.providerID}/${info.modelID}`;
    }
  }
}

export const Justless = async ({ directory }) => {
  const hooks = {
  "tool.execute.after": async (input, output) => {
    if (input?.tool !== "bash" && input?.tool !== "Bash") return;
    const command = input?.args?.command;
    if (typeof command !== "string" || typeof output?.output !== "string") return;
    // A failed, killed or unreported exit keeps its whole result.
    if (output.metadata?.exit !== 0) return;
    const next = await afterBash(directory, command, output.output);
    if (next !== undefined) output.output = next;
  },
  "experimental.chat.messages.transform": async (_input, { messages }) => {
    const model = selectedModel(messages);
    if (!model) return;
    const locations = [];
    for (const [i, message] of messages.entries()) {
      if (message.info.role !== "assistant") continue;
      for (const [j, part] of message.parts.entries()) {
        if (part.type === "tool" && ["completed", "error"].includes(part.state.status)) {
          locations.push({ i, j, part });
        }
      }
    }
    const cutoff = locations.length - 3;
    if (cutoff <= 0) return;
    const parts = locations.map(({ part }, i) => {
      const eligible = i < cutoff && part.state.status === "completed" && !part.state.attachments?.length;
      return { type: "tool", callID: part.callID, tool: part.tool, state: {
        status: part.state.status, output: eligible ? part.state.output : "",
      }};
    });
    if (!parts.some(p => p.state.output?.length >= 256)) return;
    try {
      const masked = await mask(directory, model, parts);
      // Copy only changed parts into the outbound view. Keep session records intact.
      for (const [k, { i, j, part }] of locations.entries()) {
        const output = masked[k].state.output;
        if (output === parts[k].state.output) continue;
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
