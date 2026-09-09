#!/usr/bin/env node

import crypto from 'node:crypto';
import fs from 'node:fs/promises';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';

const host = process.env.DIRECT_GATEWAY_HOST ?? process.env.OHMYAGENT_BRIDGE_HOST ?? '127.0.0.1';
const port = Number(process.env.DIRECT_GATEWAY_PORT ?? process.env.OHMYAGENT_BRIDGE_PORT ?? '8765');
const localKey = process.env.DIRECT_GATEWAY_KEY ?? process.env.OHMYAGENT_BRIDGE_KEY ?? 'local-monkeycode-direct';
const configDir = process.env.MONKEYCODE_CONFIG_DIR ?? path.join(os.homedir(), 'Library/Application Support/com.chaitin.baizhi.monkeycode');
const keyPath = process.env.MONKEYCODE_OHMYAGENT_KEY ?? path.join(configDir, 'monkeycode-ohmyagent-key.json');
const settingsPath = process.env.OHMYAGENT_SETTINGS ?? path.join(configDir, 'ohmyagent/settings.json');
const maxBodyBytes = 32 * 1024 * 1024;

function sendJson(res, statusCode, value) {
  const body = JSON.stringify(value);
  res.writeHead(statusCode, {
    'content-type': 'application/json; charset=utf-8',
    'content-length': Buffer.byteLength(body),
    'cache-control': 'no-store',
  });
  res.end(body);
}

function sendError(res, statusCode, message, type = 'invalid_request_error') {
  sendJson(res, statusCode, { error: { message, type } });
}

function authorized(req) {
  return req.headers.authorization === `Bearer ${localKey}`;
}

async function readJson(req) {
  const chunks = [];
  let size = 0;
  for await (const chunk of req) {
    size += chunk.length;
    if (size > maxBodyBytes) throw new Error('request body is too large');
    chunks.push(chunk);
  }
  if (chunks.length === 0) return {};
  return JSON.parse(Buffer.concat(chunks).toString('utf8'));
}

function textFromContent(content) {
  if (typeof content === 'string') return content;
  if (!Array.isArray(content)) return '';
  return content.map((part) => {
    if (typeof part === 'string') return part;
    if (typeof part?.text === 'string') return part.text;
    return '';
  }).filter(Boolean).join('\n');
}

function promptFromCandidate(candidate, firstOnly = false) {
  if (typeof candidate === 'string') return candidate;
  if (!Array.isArray(candidate)) return '';
  if (firstOnly) return textFromContent(candidate[0]?.text ? candidate : [candidate[0]]);
  return textFromContent(candidate);
}

function promptFromRequest(body) {
  const system = promptFromCandidate(body.system, true);
  if (system) return system;

  const instructions = promptFromCandidate(body.instructions);
  if (instructions) return instructions;

  for (const collection of [body.messages, body.input]) {
    if (!Array.isArray(collection)) continue;
    for (const message of collection) {
      if (message?.role !== 'system' && message?.role !== 'developer') continue;
      const text = textFromContent(message.content ?? message.text ?? message.input);
      if (text) return text;
    }
  }
  return '';
}

function inputText(text) {
  return [{ type: 'input_text', text }];
}

function chatMessageText(content) {
  if (typeof content === 'string') return content;
  if (!Array.isArray(content)) return '';
  return content.map((part) => {
    if (typeof part === 'string') return part;
    if (typeof part?.text === 'string') return part.text;
    return '';
  }).filter(Boolean).join('\n');
}

function chatMessagesToInput(messages) {
  if (!Array.isArray(messages)) return [];
  return messages.map((message) => {
    const role = message?.role === 'system' ? 'developer' : message?.role;
    const text = chatMessageText(message?.content);
    return {
      role: role || 'user',
      content: inputText(text),
    };
  });
}

function responseOutputText(data) {
  if (typeof data?.output_text === 'string') return data.output_text;
  return (data?.output || [])
    .flatMap((item) => item?.content || [])
    .filter((item) => item?.type === 'output_text' && typeof item.text === 'string')
    .map((item) => item.text)
    .join('');
}

function normalizeBody(body, model, developerPrompt) {
  const normalized = { ...body, model };
  const input = normalized.input;
  if (typeof input === 'string') {
    normalized.input = [
      { role: 'developer', content: inputText(developerPrompt) },
      { role: 'user', content: inputText(input) },
    ];
    delete normalized.instructions;
    delete normalized.system;
    return normalized;
  }
  if (Array.isArray(input)) {
    const hasDeveloper = input.some((message) => message?.role === 'system' || message?.role === 'developer');
    if (!hasDeveloper) {
      normalized.input = [
        { role: 'developer', content: inputText(developerPrompt) },
        ...input,
      ];
    }
    return normalized;
  }
  if (Array.isArray(normalized.messages)) {
    const hasSystem = normalized.messages.some((message) => message?.role === 'system' || message?.role === 'developer');
    if (!hasSystem) {
      normalized.messages = [
        { role: 'system', content: developerPrompt },
        ...normalized.messages,
      ];
    }
    return normalized;
  }
  normalized.input = [{ role: 'developer', content: inputText(developerPrompt) }];
  return normalized;
}

async function loadRuntime(requestedModel) {
  const [keyConfig, settings] = await Promise.all([
    fs.readFile(keyPath, 'utf8').then(JSON.parse),
    fs.readFile(settingsPath, 'utf8').then(JSON.parse),
  ]);
  const baseUrl = String(keyConfig.base_url ?? '').replace(/\/$/, '');
  const models = Object.values(settings.models ?? {});
  const normalizedModel = requestedModel.includes('/') ? requestedModel : `monkeycode-ultra/${requestedModel}`;
  const modelConfig = models.find((entry) => (
    entry.base_url === baseUrl &&
    (entry.model === requestedModel || entry.model === normalizedModel)
  ));
  if (!baseUrl || !keyConfig.api_key || !keyConfig.signing_secret) {
    throw new Error('OhMyAgent key is missing base_url, api_key, or signing_secret');
  }
  if (!modelConfig) throw new Error(`proxy model is not configured: ${requestedModel}`);
  return {
    baseUrl,
    apiKey: modelConfig.api_key || keyConfig.api_key,
    model: modelConfig.model,
    signingSecret: keyConfig.signing_secret,
    modelIds: models.filter((entry) => entry.base_url === baseUrl).map((entry) => entry.model),
  };
}

async function handleChatCompletions(req, res) {
  if (!authorized(req)) {
    sendError(res, 401, 'invalid local gateway API key', 'authentication_error');
    return;
  }
  let body;
  try {
    body = await readJson(req);
  } catch (error) {
    sendError(res, 400, error.message);
    return;
  }
  const requestedModel = typeof body.model === 'string' && body.model ? body.model : 'gpt-6-astra';
  let runtime;
  try {
    runtime = await loadRuntime(requestedModel);
  } catch (error) {
    sendError(res, 500, error.message, 'gateway_configuration_error');
    return;
  }
  const messages = Array.isArray(body.messages) ? body.messages : [];
  const developerPrompt = promptFromRequest({ messages }) || 'You are a helpful assistant.';
  const outgoingBody = {
    ...body,
    model: runtime.model,
    input: chatMessagesToInput(messages),
  };
  delete outgoingBody.messages;
  delete outgoingBody.stream;
  const signature = crypto.createHmac('sha256', runtime.signingSecret).update(developerPrompt, 'utf8').digest('hex');
  let upstream;
  try {
    upstream = await fetch(`${runtime.baseUrl}/responses`, {
      method: 'POST',
      headers: {
        Authorization: `Bearer ${runtime.apiKey}`,
        'X-OhMyAgent-Signature': `v1=${signature}`,
        'Content-Type': 'application/json',
        Accept: 'application/json',
      },
      body: JSON.stringify(outgoingBody),
    });
  } catch (error) {
    sendError(res, 502, `gateway request failed: ${error.message}`, 'upstream_error');
    return;
  }
  if (!upstream.ok) {
    const detail = (await upstream.text()).slice(0, 2000);
    sendError(res, upstream.status, detail, 'upstream_error');
    return;
  }
  let data;
  try {
    data = await upstream.json();
  } catch (error) {
    sendError(res, 502, `invalid gateway response: ${error.message}`, 'upstream_error');
    return;
  }
  sendJson(res, 200, {
    id: data.id || `chatcmpl-${crypto.randomUUID()}`,
    object: 'chat.completion',
    created: Math.floor(Date.now() / 1000),
    model: requestedModel,
    choices: [{
      index: 0,
      message: { role: 'assistant', content: responseOutputText(data) },
      finish_reason: 'stop',
    }],
    usage: data.usage || undefined,
  });
}

async function handleResponses(req, res) {
  if (!authorized(req)) {
    sendError(res, 401, 'invalid local gateway API key', 'authentication_error');
    return;
  }
  let body;
  try {
    body = await readJson(req);
  } catch (error) {
    sendError(res, 400, error.message);
    return;
  }
  const requestedModel = typeof body.model === 'string' && body.model ? body.model : 'gpt-6-astra';
  let runtime;
  try {
    runtime = await loadRuntime(requestedModel);
  } catch (error) {
    sendError(res, 500, error.message, 'gateway_configuration_error');
    return;
  }
  const developerPrompt = promptFromRequest(body) || 'You are a helpful assistant.';
  const outgoingBody = normalizeBody(body, runtime.model, developerPrompt);
  const signature = crypto.createHmac('sha256', runtime.signingSecret).update(developerPrompt, 'utf8').digest('hex');
  let upstream;
  try {
    upstream = await fetch(`${runtime.baseUrl}/responses`, {
      method: 'POST',
      headers: {
        Authorization: `Bearer ${runtime.apiKey}`,
        'X-OhMyAgent-Signature': `v1=${signature}`,
        'Content-Type': 'application/json',
        Accept: outgoingBody.stream === true ? 'text/event-stream' : 'application/json',
      },
      body: JSON.stringify(outgoingBody),
    });
  } catch (error) {
    sendError(res, 502, `gateway request failed: ${error.message}`, 'upstream_error');
    return;
  }
  const contentType = upstream.headers.get('content-type') || 'application/json';
  res.writeHead(upstream.status, {
    'content-type': contentType,
    'cache-control': upstream.headers.get('cache-control') || 'no-store',
  });
  if (!upstream.body) {
    res.end();
    return;
  }
  for await (const chunk of upstream.body) res.write(chunk);
  res.end();
}

const server = http.createServer(async (req, res) => {
  try {
    const requestUrl = new URL(req.url || '/', `http://${req.headers.host || 'localhost'}`);
    const pathname = requestUrl.pathname.replace(/\/+$/, '') || '/';
    if (req.method === 'GET' && (pathname === '/health' || pathname === '/v1/health')) {
      sendJson(res, 200, { ok: true, mode: 'direct-signed-gateway', endpoint: 'proxy.monkeycode-ai.com/v1' });
      return;
    }
    if (req.method === 'GET' && (pathname === '/models' || pathname === '/v1/models')) {
      if (!authorized(req)) {
        sendError(res, 401, 'invalid local gateway API key', 'authentication_error');
        return;
      }
      const runtime = await loadRuntime('gpt-6-astra');
      const ids = [...new Set(runtime.modelIds.flatMap((id) => [id, id.replace(/^monkeycode-(?:basic|pro|ultra)\//, '')]))];
      sendJson(res, 200, { object: 'list', data: ids.map((id) => ({ id, object: 'model', owned_by: 'monkeycode' })) });
      return;
    }
    if (req.method === 'POST' && (pathname === '/responses' || pathname === '/v1/responses')) {
      await handleResponses(req, res);
      return;
    }
    if (req.method === 'POST' && (pathname === '/chat/completions' || pathname === '/v1/chat/completions')) {
      await handleChatCompletions(req, res);
      return;
    }
    sendError(res, 404, 'not found');
  } catch (error) {
    sendError(res, 500, error.message, 'direct_gateway_error');
  }
});

server.listen(port, host, () => {
  console.log(`direct-gateway-server listening on http://${host}:${port}`);
  console.log('mode=direct-signed-gateway');
});

function shutdown() {
  server.close(() => process.exit(0));
}

process.on('SIGINT', shutdown);
process.on('SIGTERM', shutdown);
