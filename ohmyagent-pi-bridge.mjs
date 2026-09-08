#!/usr/bin/env node

import crypto from "node:crypto";
import http from "node:http";
import { spawn } from "node:child_process";

const host = process.env.OHMYAGENT_BRIDGE_HOST ?? "127.0.0.1";
const port = Number(process.env.OHMYAGENT_BRIDGE_PORT ?? "8765");
const bridgeKey = process.env.OHMYAGENT_BRIDGE_KEY ?? "local-ohmyagent-bridge";
const cwd = process.env.OHMYAGENT_BRIDGE_CWD ?? process.cwd();
const ohmyagentBin = process.env.OHMYAGENT_BIN ?? "/Applications/MonkeyCode.app/Contents/MacOS/ohmyagent";
const ohmyagentConfigDir = process.env.OHMYAGENT_CONFIG_DIR ??
  `${process.env.HOME}/Library/Application Support/com.chaitin.baizhi.monkeycode/ohmyagent`;
const internalModel = process.env.OHMYAGENT_MODEL ?? "monkeycode-ultra/gpt-6-astra";
const supportedModels = ["gpt-6-astra", "gpt-5.5", "gpt-5.4", "gpt-5.4-mini", "gpt-5.6-sol", "gpt-5.6-luna", "gpt-5.6-terra", "glm-5.3", "glm-5.3-flash", "grok-4.6"];
const requestTimeoutMs = Number(process.env.OHMYAGENT_BRIDGE_TIMEOUT_MS ?? "600000");
const maxBodyBytes = 8 * 1024 * 1024;

function responseId(prefix = "resp") {
  return `${prefix}_bridge_${crypto.randomBytes(12).toString("hex")}`;
}

function sendJson(res, statusCode, value) {
  const body = JSON.stringify(value);
  res.writeHead(statusCode, {
    "content-type": "application/json; charset=utf-8",
    "content-length": Buffer.byteLength(body),
    "cache-control": "no-store",
  });
  res.end(body);
}

function sendError(res, statusCode, message, type = "invalid_request_error") {
  sendJson(res, statusCode, { error: { message, type } });
}

async function readJson(req) {
  const chunks = [];
  let size = 0;
  for await (const chunk of req) {
    size += chunk.length;
    if (size > maxBodyBytes) throw new Error("request body is too large");
    chunks.push(chunk);
  }
  if (chunks.length === 0) return {};
  return JSON.parse(Buffer.concat(chunks).toString("utf8"));
}

function textFromContent(content) {
  if (typeof content === "string") return content;
  if (!Array.isArray(content)) return "";
  return content.map((part) => {
    if (typeof part === "string") return part;
    if (part?.type === "text" || part?.type === "input_text" || part?.type === "output_text") {
      return part.text ?? "";
    }
    if (part?.type === "tool_result") return `[tool result]\n${textFromContent(part.content)}`;
    return "";
  }).filter(Boolean).join("\n");
}

function promptFromRequest(body) {
  const sections = [];
  const instructions = textFromContent(body.instructions);
  if (instructions) sections.push(`[system]\n${instructions}`);

  const input = body.input ?? body.messages;
  if (typeof input === "string") {
    sections.push(`[user]\n${input}`);
  } else if (Array.isArray(input)) {
    const messages = input.map((item) => {
      if (typeof item === "string") return `[user]\n${item}`;
      const role = item?.role ?? item?.type ?? "user";
      const text = textFromContent(item?.content ?? item?.text ?? item?.input);
      return text ? `[${role}]\n${text}` : "";
    }).filter(Boolean);
    sections.push(messages.join("\n\n"));
  }

  if (sections.length === 0) throw new Error("request must include input or messages");
  return sections.join("\n\n");
}

function createResponse(text, model, usage = {}) {
  const messageId = responseId("msg");
  const inputTokens = Number(usage.input_tokens ?? 0);
  const outputTokens = Number(usage.output_tokens ?? 0);
  return {
    id: responseId(),
    object: "response",
    created_at: Math.floor(Date.now() / 1000),
    model,
    status: "completed",
    output: [{
      id: messageId,
      type: "message",
      status: "completed",
      role: "assistant",
      content: [{ type: "output_text", text, annotations: [] }],
    }],
    output_text: text,
    usage: {
      input_tokens: inputTokens,
      output_tokens: outputTokens,
      total_tokens: inputTokens + outputTokens,
    },
  };
}

function runOhmyagent(prompt, requestedModel) {
  return new Promise((resolve, reject) => {
    const child = spawn(ohmyagentBin, [
      "--cwd", cwd,
      "--model", internalModel,
      "--permission-mode", "auto",
      "--output-format", "json",
      "--prompt", prompt,
    ], {
      cwd,
      env: {
        ...process.env,
        OHMYAGENT_CONFIG_DIR: ohmyagentConfigDir,
      },
      stdio: ["ignore", "pipe", "pipe"],
    });

    let stdout = "";
    let stderr = "";
    let finished = false;
    let timer;

    const finish = (callback) => {
      if (finished) return;
      finished = true;
      clearTimeout(timer);
      callback();
    };

    child.stdout.setEncoding("utf8");
    child.stderr.setEncoding("utf8");
    child.stdout.on("data", (chunk) => { stdout += chunk; });
    child.stderr.on("data", (chunk) => { stderr += chunk; });
    child.on("error", (error) => finish(() => reject(error)));
    child.on("close", (code, signal) => finish(() => {
      const events = stdout.split("\n").map((line) => {
        try { return JSON.parse(line); } catch { return null; }
      }).filter(Boolean);
      const done = [...events].reverse().find((event) => event.type === "model_done");
      const usage = events.find((event) => event.type === "usage")?.data ?? {};
      const text = done?.data?.text ?? "";
      if (code !== 0 || !text) {
        const detail = stderr.trim() || done?.data?.error || `ohmyagent exited with code ${code ?? "unknown"}${signal ? ` (${signal})` : ""}`;
        reject(new Error(detail));
        return;
      }
      resolve({ text, usage, requestedModel });
    }));
    timer = setTimeout(() => {
      child.kill("SIGTERM");
      finish(() => reject(new Error(`ohmyagent timed out after ${requestTimeoutMs}ms`)));
    }, requestTimeoutMs);
  });
}

function authorized(req) {
  const value = req.headers.authorization ?? "";
  return value === `Bearer ${bridgeKey}`;
}

function writeSse(res, event, data) {
  res.write(`event: ${event}\ndata: ${JSON.stringify(data)}\n\n`);
}

async function handleResponses(req, res) {
  if (!authorized(req)) {
    sendError(res, 401, "invalid bridge API key", "authentication_error");
    return;
  }

  let body;
  try {
    body = await readJson(req);
  } catch (error) {
    sendError(res, 400, error.message);
    return;
  }

  let prompt;
  try {
    prompt = promptFromRequest(body);
  } catch (error) {
    sendError(res, 400, error.message);
    return;
  }

  const requestedModel = body.model ?? "gpt-6-astra";
  if (!supportedModels.includes(requestedModel)) { sendError(res, 400, `unsupported model: ${requestedModel}`); return; }
  try {
    const result = await runOhmyagent(prompt, requestedModel);
    const response = createResponse(result.text, requestedModel, result.usage);
    if (body.stream === true) {
      const createdResponse = { ...response, status: "in_progress", output: [] };
      res.writeHead(200, {
        "content-type": "text/event-stream; charset=utf-8",
        "cache-control": "no-cache",
        connection: "keep-alive",
      });
      writeSse(res, "response.created", { type: "response.created", response: createdResponse });
      writeSse(res, "response.output_item.added", {
        type: "response.output_item.added",
        output_index: 0,
        item: response.output[0],
      });
      writeSse(res, "response.content_part.added", {
        type: "response.content_part.added",
        item_id: response.output[0].id,
        output_index: 0,
        content_index: 0,
        part: response.output[0].content[0],
      });
      writeSse(res, "response.output_text.delta", {
        type: "response.output_text.delta",
        item_id: response.output[0].id,
        output_index: 0,
        content_index: 0,
        delta: result.text,
      });
      writeSse(res, "response.output_text.done", {
        type: "response.output_text.done",
        item_id: response.output[0].id,
        output_index: 0,
        content_index: 0,
        text: result.text,
      });
      writeSse(res, "response.content_part.done", {
        type: "response.content_part.done",
        item_id: response.output[0].id,
        output_index: 0,
        content_index: 0,
        part: response.output[0].content[0],
      });
      writeSse(res, "response.output_item.done", {
        type: "response.output_item.done",
        output_index: 0,
        item: response.output[0],
      });
      writeSse(res, "response.completed", { type: "response.completed", response });
      res.end("data: [DONE]\n\n");
      return;
    }
    sendJson(res, 200, response);
  } catch (error) {
    sendError(res, 502, error.message, "ohmyagent_error");
  }
}

const server = http.createServer(async (req, res) => {
  try {
    if (req.method === "GET" && (req.url === "/health" || req.url === "/v1/health")) {
      sendJson(res, 200, { ok: true, model: internalModel, cwd });
      return;
    }
    if (req.method === "GET" && (req.url === "/models" || req.url === "/v1/models")) {
      if (!authorized(req)) {
        sendError(res, 401, "invalid bridge API key", "authentication_error");
        return;
      }
      sendJson(res, 200, {
        object: "list",
        data: supportedModels.map((id) => ({ id, object: "model", owned_by: "monkeycode" })),
      });
      return;
    }
    if (req.method === "POST" && (req.url === "/responses" || req.url === "/v1/responses")) {
      await handleResponses(req, res);
      return;
    }
    sendError(res, 404, "not found", "invalid_request_error");
  } catch (error) {
    sendError(res, 500, error.message, "bridge_error");
  }
});

server.listen(port, host, () => {
  console.log(`ohmyagent-pi-bridge listening on http://${host}:${port}`);
  console.log(`model=${internalModel}`);
  console.log(`cwd=${cwd}`);
});

function shutdown() {
  server.close(() => process.exit(0));
}

process.on("SIGINT", shutdown);
process.on("SIGTERM", shutdown);
