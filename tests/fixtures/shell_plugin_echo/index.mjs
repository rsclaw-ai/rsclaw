import readline from "node:readline";

const stdin = readline.createInterface({ input: process.stdin });
let nextNegId = -1;
const pending = new Map(); // -id → resolver, for plugin-initiated calls

function send(obj) {
  process.stdout.write(JSON.stringify(obj) + "\n");
}

function hostCall(method, params) {
  const id = nextNegId--;
  send({ id, method, params });
  return new Promise((resolve, reject) => {
    pending.set(id, { resolve, reject });
  });
}

stdin.on("line", async (line) => {
  let msg;
  try {
    msg = JSON.parse(line);
  } catch {
    return;
  }

  // Response to a plugin-initiated request (negative id we issued earlier).
  if (typeof msg.id === "number" && msg.id < 0 && pending.has(msg.id)) {
    const p = pending.get(msg.id);
    pending.delete(msg.id);
    if (msg.error) p.reject(new Error(typeof msg.error === "string" ? msg.error : JSON.stringify(msg.error)));
    else p.resolve(msg.result);
    return;
  }

  // Host-initiated request (positive id).
  if (msg.method === "tool_call") {
    const tool = msg.params?.tool;
    const args = msg.params?.args ?? {};
    const ctx = msg.params?._ctx ?? {};

    if (tool === "echo") {
      send({ id: msg.id, result: { echoed: args } });
      return;
    }

    if (tool === "notify_then_echo") {
      try {
        await hostCall("notify", { text: `notify: ${args.msg}`, _ctx: ctx });
        send({ id: msg.id, result: { echoed: args, notified: true } });
      } catch (e) {
        send({ id: msg.id, error: e.message });
      }
      return;
    }

    send({ id: msg.id, error: `unknown tool: ${tool}` });
    return;
  }

  // Hooks / other inbound methods aren't exercised by these tests, but
  // be polite: respond with an error so the host's pending oneshot resolves.
  if (typeof msg.id === "number" && msg.id > 0) {
    send({ id: msg.id, error: `unknown method: ${msg.method}` });
  }
});
