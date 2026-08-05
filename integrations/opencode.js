// Managed by Justless. OpenCode v1 project plugin.
import { execFile } from "node:child_process";

function mask(directory, parts) {
  return new Promise((resolve, reject) => {
    const child = execFile("justless", ["--root", directory, "mask"], {
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

export const Justless = async ({ directory }) => ({
  "experimental.chat.messages.transform": async (_input, { messages }) => {
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
      const masked = await mask(directory, parts);
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
});
