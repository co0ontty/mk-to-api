#!/usr/bin/env node

import crypto from 'node:crypto';
import fs from 'node:fs/promises';
import http from 'node:http';
import https from 'node:https';
import os from 'node:os';
import path from 'node:path';

const args = process.argv.slice(2);
const flag = (name, fallback) => {
  const index = args.indexOf(name);
  return index >= 0 ? args[index + 1] ?? fallback : fallback;
};
const defaultConfigDir = path.join(os.homedir(), 'Library/Application Support/com.chaitin.baizhi.monkeycode');
const configPath = flag('--config', process.env.MONKEYCODE_GATEWAY_CONFIG || path.join(defaultConfigDir, 'direct-gateway.json'));
let fileConfig = {};
try { fileConfig = JSON.parse(await fs.readFile(configPath, 'utf8')); }
catch (error) { if (error.code !== 'ENOENT') throw new Error(`cannot read gateway config: ${configPath} (${error.message})`); }
const configValue = (name, fallback) => fileConfig[name.replaceAll('-', '_')] ?? fileConfig[name] ?? fallback;
const configured = (name, envName, fallback) => flag(`--${name}`, process.env[envName] ?? configValue(name, fallback));
const listValue = (value) => Array.isArray(value) ? value.join(',') : String(value ?? '');
const host = configured('host', 'DIRECT_GATEWAY_HOST', process.env.OHMYAGENT_BRIDGE_HOST ?? '127.0.0.1');
const port = Number(configured('port', 'DIRECT_GATEWAY_PORT', process.env.OHMYAGENT_BRIDGE_PORT ?? '8123'));
const localKey = configured('key', 'DIRECT_GATEWAY_KEY', process.env.OHMYAGENT_BRIDGE_KEY ?? 'local-monkeycode-direct');
const authRequired = String(configured('auth-required', 'DIRECT_GATEWAY_AUTH_REQUIRED', fileConfig.auth_required ?? 'true')) !== 'false';
const trustProxy = String(configured('trust-proxy', 'DIRECT_GATEWAY_TRUST_PROXY', fileConfig.trust_proxy ?? 'false')) === 'true';
const allowedOrigins = listValue(configured('allowed-origins', 'DIRECT_GATEWAY_ALLOWED_ORIGINS', fileConfig.allowed_origins)).split(',').map((v) => v.trim()).filter(Boolean);
const allowedIps = listValue(configured('allowed-ips', 'DIRECT_GATEWAY_ALLOWED_IPS', fileConfig.allowed_ips)).split(',').map(normalizeIp).filter(Boolean);
const tlsCertPath = configured('tls-cert', 'DIRECT_GATEWAY_TLS_CERT', fileConfig.tls_cert);
const tlsKeyPath = configured('tls-key', 'DIRECT_GATEWAY_TLS_KEY', fileConfig.tls_key);
const configDir = configured('config-dir', 'MONKEYCODE_CONFIG_DIR', defaultConfigDir);
const keyPath = configured('key-file', 'MONKEYCODE_OHMYAGENT_KEY', path.join(configDir, 'monkeycode-ohmyagent-key.json'));
const settingsPath = configured('settings', 'OHMYAGENT_SETTINGS', path.join(configDir, 'ohmyagent/settings.json'));
const maxBodyBytes = Number(configured('max-body-bytes', 'DIRECT_GATEWAY_MAX_BODY_BYTES', fileConfig.max_body_bytes ?? 32 * 1024 * 1024));

function normalizeIp(value) { return String(value ?? '').trim().replace(/^::ffff:/, '').split('%')[0]; }
function errorBody(message, type = 'invalid_request_error', code = null, param = null) {
  return { error: { message, type, param, code } };
}
function corsHeaders(req) {
  const origin = req?.headers.origin;
  if (!origin || !originAllowed(origin)) return {};
  return { 'access-control-allow-origin': origin, vary: 'Origin' };
}
function sendJson(req, res, statusCode, value, extraHeaders = {}) {
  const body = JSON.stringify(value);
  res.writeHead(statusCode, { 'content-type': 'application/json; charset=utf-8', 'content-length': Buffer.byteLength(body), 'cache-control': 'no-store', ...corsHeaders(req), ...extraHeaders });
  res.end(body);
}
function sendError(req, res, statusCode, message, type = 'invalid_request_error', code = null, param = null) {
  sendJson(req, res, statusCode, errorBody(message, type, code, param));
}
function bearerToken(req) {
  const match = /^Bearer\s+(.+)$/i.exec(req.headers.authorization ?? '');
  return match?.[1];
}
function safeEqual(left, right) {
  const a = Buffer.from(String(left ?? ''));
  const b = Buffer.from(String(right ?? ''));
  return a.length === b.length && crypto.timingSafeEqual(a, b);
}
function authorized(req) { return !authRequired || safeEqual(bearerToken(req), localKey); }
function originAllowed(origin) { return allowedOrigins.length === 0 || allowedOrigins.includes('*') || allowedOrigins.includes(origin); }
function clientIp(req) {
  if (trustProxy) {
    const forwarded = req.headers['x-forwarded-for'];
    if (forwarded) return normalizeIp(String(forwarded).split(',')[0]);
  }
  return normalizeIp(req.socket.remoteAddress);
}
function ipAllowed(req) { return allowedIps.length === 0 || allowedIps.includes(clientIp(req)); }
async function readJson(req) {
  const chunks = []; let size = 0;
  for await (const chunk of req) {
    size += chunk.length;
    if (size > maxBodyBytes) { const error = new Error('request body is too large'); error.statusCode = 413; throw error; }
    chunks.push(chunk);
  }
  if (!chunks.length) return {};
  try { return JSON.parse(Buffer.concat(chunks).toString('utf8')); }
  catch { const error = new Error('invalid JSON request body'); error.statusCode = 400; throw error; }
}
function textFromContent(content) {
  if (typeof content === 'string') return content;
  if (!Array.isArray(content)) return '';
  return content.map((part) => typeof part === 'string' ? part : (typeof part?.text === 'string' ? part.text : '')).filter(Boolean).join('\n');
}
function developerPromptFromBody(body) {
  if (typeof body.instructions === 'string' && body.instructions) return body.instructions;
  for (const collection of [body.messages, body.input]) {
    if (!Array.isArray(collection)) continue;
    const texts = collection.filter((item) => item?.role === 'system' || item?.role === 'developer').map((item) => textFromContent(item.content ?? item.text)).filter(Boolean);
    if (texts.length) return texts.join('\n\n');
  }
  return 'You are a helpful assistant.';
}
function inputContent(content) {
  if (typeof content === 'string') return [{ type: 'input_text', text: content }];
  if (!Array.isArray(content)) return [{ type: 'input_text', text: '' }];
  const parts = [];
  for (const part of content) {
    if (typeof part === 'string') parts.push({ type: 'input_text', text: part });
    else if (part?.type === 'text' || part?.type === 'input_text') parts.push({ type: 'input_text', text: part.text ?? '' });
    else if (part?.type === 'image_url' && part.image_url?.url) parts.push({ type: 'input_image', image_url: part.image_url.url, detail: part.image_url.detail ?? 'auto' });
    else if (part?.type === 'input_image') parts.push(part);
  }
  return parts.length ? parts : [{ type: 'input_text', text: '' }];
}
function messagesToInput(messages) {
  if (!Array.isArray(messages)) return [];
  return messages.filter((message) => message && ['system', 'developer', 'user', 'assistant'].includes(message.role)).map((message) => ({ role: message.role === 'system' ? 'developer' : message.role, content: inputContent(message.content) }));
}
function normalizeResponseInput(input) {
  if (typeof input === 'string') return [{ role: 'user', content: inputContent(input) }];
  if (!Array.isArray(input)) return [];
  return input.map((item) => {
    if (typeof item === 'string') return { role: 'user', content: inputContent(item) };
    if (item?.type && item.type !== 'message') return item;
    return { ...item, role: item?.role === 'system' ? 'developer' : (item?.role ?? 'user'), content: inputContent(item?.content ?? item?.text ?? '') };
  });
}
function responseOutputText(data) {
  if (typeof data?.output_text === 'string') return data.output_text;
  return (data?.output ?? []).flatMap((item) => item?.content ?? []).filter((item) => item?.type === 'output_text' && typeof item.text === 'string').map((item) => item.text).join('');
}
function responseFinishReason(data) {
  if (data?.status === 'incomplete') return data.incomplete_details?.reason === 'max_output_tokens' ? 'length' : 'stop';
  return 'stop';
}
function normalizedUsage(usage) {
  if (!usage) return undefined;
  const prompt = Number(usage.input_tokens ?? usage.prompt_tokens ?? 0);
  const completion = Number(usage.output_tokens ?? usage.completion_tokens ?? 0);
  return { prompt_tokens: prompt, completion_tokens: completion, total_tokens: Number(usage.total_tokens ?? prompt + completion) };
}
function normalizeChatRequest(body, model) {
  if (!Array.isArray(body.messages) || !body.messages.length) { const error = new Error('messages must be a non-empty array'); error.statusCode = 400; throw error; }
  const outgoing = { model, input: messagesToInput(body.messages), stream: body.stream === true, store: body.store ?? false };
  if (body.max_completion_tokens ?? body.max_tokens) outgoing.max_output_tokens = body.max_completion_tokens ?? body.max_tokens;
  for (const field of ['temperature', 'top_p', 'metadata', 'tools', 'tool_choice', 'parallel_tool_calls', 'user']) if (body[field] !== undefined) outgoing[field] = body[field];
  return outgoing;
}
function normalizeResponsesRequest(body, model, prompt) {
  const outgoing = { ...body, model, input: normalizeResponseInput(body.input), store: body.store ?? false };
  delete outgoing.messages; delete outgoing.system;
  if (!outgoing.input.length && Array.isArray(body.messages)) outgoing.input = messagesToInput(body.messages);
  if (!outgoing.input.some((item) => item?.role === 'developer')) outgoing.input.unshift({ role: 'developer', content: inputContent(prompt) });
  return outgoing;
}
async function loadRuntime(requestedModel) {
  const [keyConfig, settings] = await Promise.all([fs.readFile(keyPath, 'utf8').then(JSON.parse), fs.readFile(settingsPath, 'utf8').then(JSON.parse)]);
  const baseUrl = String(fileConfig.upstream_host ?? keyConfig.base_url ?? '').replace(/\/$/, '');
  const upstreamKey = fileConfig.upstream_key ?? fileConfig.api_key ?? keyConfig.api_key;
  const signingSecret = fileConfig.signing_secret ?? keyConfig.signing_secret;
  const models = Object.values(settings.models ?? {});
  const normalized = requestedModel.includes('/') ? requestedModel : `monkeycode-ultra/${requestedModel}`;
  const modelConfig = models.find((entry) => entry.model === requestedModel || entry.model === normalized);
  if (!baseUrl || !upstreamKey || !signingSecret) throw new Error('gateway configuration is missing upstream_host, upstream_key, or signing_secret');
  if (!modelConfig) { const error = new Error(`model is not configured: ${requestedModel}`); error.statusCode = 404; error.type = 'invalid_request_error'; error.code = 'model_not_found'; throw error; }
  return { baseUrl, apiKey: modelConfig.api_key || upstreamKey, model: modelConfig.model, signingSecret, modelIds: models.map((entry) => entry.model) };
}
async function requestUpstream(outgoing, runtime) {
  const prompt = developerPromptFromBody(outgoing);
  const signature = crypto.createHmac('sha256', runtime.signingSecret).update(prompt, 'utf8').digest('hex');
  return fetch(`${runtime.baseUrl}/responses`, { method: 'POST', headers: { Authorization: `Bearer ${runtime.apiKey}`, 'X-OhMyAgent-Signature': `v1=${signature}`, 'Content-Type': 'application/json', Accept: outgoing.stream ? 'text/event-stream' : 'application/json' }, body: JSON.stringify(outgoing) });
}
async function readUpstreamError(upstream) {
  const text = (await upstream.text()).slice(0, 4000);
  try { const parsed = JSON.parse(text); return parsed.error ?? { message: text }; } catch { return { message: text || `upstream HTTP ${upstream.status}` }; }
}
function writeSse(res, data, event) {
  if (event) res.write(`event: ${event}\n`);
  res.write(`data: ${typeof data === 'string' ? data : JSON.stringify(data)}\n\n`);
}
async function parseSse(body, onEvent) {
  const decoder = new TextDecoder(); let buffer = '';
  for await (const chunk of body) {
    buffer += decoder.decode(chunk, { stream: true }).replace(/\r\n/g, '\n');
    let boundary;
    while ((boundary = buffer.indexOf('\n\n')) >= 0) {
      const block = buffer.slice(0, boundary); buffer = buffer.slice(boundary + 2);
      let event = ''; const data = [];
      for (const line of block.split('\n')) { if (line.startsWith('event:')) event = line.slice(6).trim(); else if (line.startsWith('data:')) data.push(line.slice(5).trimStart()); }
      if (data.length) await onEvent({ event, data: data.join('\n') });
    }
  }
  buffer += decoder.decode();
  if (buffer.trim()) { const data = buffer.split('\n').filter((line) => line.startsWith('data:')).map((line) => line.slice(5).trimStart()).join('\n'); if (data) await onEvent({ event: '', data }); }
}
function beginSse(req, res) {
  res.writeHead(200, { 'content-type': 'text/event-stream; charset=utf-8', 'cache-control': 'no-cache, no-transform', connection: 'keep-alive', 'x-accel-buffering': 'no', ...corsHeaders(req) });
  res.flushHeaders?.();
}
async function streamChat(req, res, upstream, requestedModel) {
  beginSse(req, res);
  const id = `chatcmpl-${crypto.randomUUID()}`; const created = Math.floor(Date.now() / 1000);
  const chunk = (delta, finishReason = null, usage) => ({ id, object: 'chat.completion.chunk', created, model: requestedModel, choices: [{ index: 0, delta, finish_reason: finishReason }], ...(usage ? { usage } : {}) });
  writeSse(res, chunk({ role: 'assistant', content: '' }));
  let completed;
  await parseSse(upstream.body, async ({ data }) => {
    if (data === '[DONE]') return;
    let event; try { event = JSON.parse(data); } catch { return; }
    if (event.type === 'response.output_text.delta' && typeof event.delta === 'string') writeSse(res, chunk({ content: event.delta }));
    else if (event.type === 'response.completed') completed = event.response;
    else if (event.type === 'response.failed' || event.type === 'error') writeSse(res, { error: event.error ?? errorBody('upstream stream failed', 'upstream_error').error });
  });
  writeSse(res, chunk({}, responseFinishReason(completed), normalizedUsage(completed?.usage)));
  writeSse(res, '[DONE]'); res.end();
}
async function streamResponses(req, res, upstream) {
  beginSse(req, res);
  const contentType = upstream.headers.get('content-type') ?? '';
  if (contentType.includes('text/event-stream')) {
    for await (const chunk of upstream.body) res.write(chunk);
    res.end(); return;
  }
  const data = await upstream.json(); const sequence = (type, extra = {}) => ({ type, sequence_number: sequence.number++, ...extra }); sequence.number = 0;
  writeSse(res, sequence('response.created', { response: { ...data, status: 'in_progress', output: [] } }));
  const text = responseOutputText(data);
  if (text) writeSse(res, sequence('response.output_text.delta', { output_index: 0, content_index: 0, delta: text }));
  writeSse(res, sequence('response.completed', { response: data })); res.end();
}
async function handleChat(req, res) {
  let body; try { body = await readJson(req); } catch (error) { sendError(req, res, error.statusCode ?? 400, error.message); return; }
  const requestedModel = typeof body.model === 'string' && body.model ? body.model : 'gpt-6-astra';
  let runtime, outgoing;
  try { runtime = await loadRuntime(requestedModel); outgoing = normalizeChatRequest(body, runtime.model); }
  catch (error) { sendError(req, res, error.statusCode ?? 500, error.message, error.type ?? 'gateway_configuration_error', error.code); return; }
  let upstream; try { upstream = await requestUpstream(outgoing, runtime); } catch (error) { sendError(req, res, 502, `upstream request failed: ${error.message}`, 'api_error', 'upstream_unavailable'); return; }
  if (!upstream.ok) { const error = await readUpstreamError(upstream); sendError(req, res, upstream.status, error.message, error.type ?? 'upstream_error', error.code, error.param); return; }
  if (body.stream === true) { await streamChat(req, res, upstream, requestedModel); return; }
  let data; try { data = await upstream.json(); } catch (error) { sendError(req, res, 502, `invalid upstream response: ${error.message}`, 'api_error', 'invalid_upstream_response'); return; }
  sendJson(req, res, 200, { id: data.id || `chatcmpl-${crypto.randomUUID()}`, object: 'chat.completion', created: Math.floor(Date.now() / 1000), model: requestedModel, choices: [{ index: 0, message: { role: 'assistant', content: responseOutputText(data) }, finish_reason: responseFinishReason(data) }], usage: normalizedUsage(data.usage) });
}
async function handleResponses(req, res) {
  let body; try { body = await readJson(req); } catch (error) { sendError(req, res, error.statusCode ?? 400, error.message); return; }
  const requestedModel = typeof body.model === 'string' && body.model ? body.model : 'gpt-6-astra';
  let runtime; try { runtime = await loadRuntime(requestedModel); } catch (error) { sendError(req, res, error.statusCode ?? 500, error.message, error.type ?? 'gateway_configuration_error', error.code); return; }
  const outgoing = normalizeResponsesRequest(body, runtime.model, developerPromptFromBody(body));
  let upstream; try { upstream = await requestUpstream(outgoing, runtime); } catch (error) { sendError(req, res, 502, `upstream request failed: ${error.message}`, 'api_error', 'upstream_unavailable'); return; }
  if (!upstream.ok) { const error = await readUpstreamError(upstream); sendError(req, res, upstream.status, error.message, error.type ?? 'upstream_error', error.code, error.param); return; }
  if (body.stream === true) { await streamResponses(req, res, upstream); return; }
  let data; try { data = await upstream.json(); } catch (error) { sendError(req, res, 502, `invalid upstream response: ${error.message}`, 'api_error', 'invalid_upstream_response'); return; }
  sendJson(req, res, 200, data);
}

const listener = async (req, res) => {
  try {
    const requestUrl = new URL(req.url || '/', `http://${req.headers.host || 'localhost'}`);
    const pathname = requestUrl.pathname.replace(/\/+$/, '') || '/';
    if (!ipAllowed(req)) { sendError(req, res, 403, 'client IP is not allowed', 'permission_error', 'ip_not_allowed'); return; }
    if (req.headers.origin && !originAllowed(req.headers.origin)) { sendError(req, res, 403, 'request origin is not allowed', 'permission_error', 'origin_not_allowed'); return; }
    if (req.method === 'OPTIONS') { res.writeHead(204, { ...corsHeaders(req), 'access-control-allow-methods': 'GET, POST, OPTIONS', 'access-control-allow-headers': 'Authorization, Content-Type', 'access-control-max-age': '86400' }); res.end(); return; }
    if (req.method === 'GET' && (pathname === '/' || pathname === '/v1')) { sendJson(req, res, 200, { object: 'gateway', name: 'monkeycode-direct-gateway', status: 'ok', endpoints: ['/health', '/v1/models', '/v1/responses', '/v1/chat/completions'] }); return; }
    if (req.method === 'GET' && (pathname === '/health' || pathname === '/v1/health')) { sendJson(req, res, 200, { ok: true, mode: 'direct-signed-gateway', auth_required: authRequired, tls: Boolean(tlsCertPath) }); return; }
    if (!authorized(req)) { sendError(req, res, 401, 'invalid API key', 'authentication_error', 'invalid_api_key', null); return; }
    if (req.method === 'GET' && (pathname === '/models' || pathname === '/v1/models')) {
      const runtime = await loadRuntime('gpt-6-astra'); const ids = [...new Set(runtime.modelIds.flatMap((id) => [id, id.replace(/^monkeycode-(?:basic|pro|ultra)\//, '')]))];
      sendJson(req, res, 200, { object: 'list', data: ids.map((id) => ({ id, object: 'model', created: 0, owned_by: 'monkeycode' })) }); return;
    }
    if (req.method === 'POST' && (pathname === '/responses' || pathname === '/v1/responses')) { await handleResponses(req, res); return; }
    if (req.method === 'POST' && (pathname === '/chat/completions' || pathname === '/v1/chat/completions')) { await handleChat(req, res); return; }
    sendError(req, res, 404, `unknown endpoint: ${pathname}`, 'invalid_request_error', 'not_found');
  } catch (error) { if (!res.headersSent) sendError(req, res, 500, error.message, 'api_error', 'internal_error'); else res.destroy(error); }
};

let server;
if (Boolean(tlsCertPath) !== Boolean(tlsKeyPath)) throw new Error('both tls_cert and tls_key are required for HTTPS');
if (tlsCertPath) server = https.createServer({ cert: await fs.readFile(tlsCertPath), key: await fs.readFile(tlsKeyPath) }, listener);
else server = http.createServer(listener);
server.requestTimeout = Number(configured('request-timeout-ms', 'DIRECT_GATEWAY_REQUEST_TIMEOUT_MS', fileConfig.request_timeout_ms ?? 600000));
server.listen(port, host, () => {
  console.log(`direct-gateway-server listening on ${tlsCertPath ? 'https' : 'http'}://${host}:${port}`);
  console.log('mode=direct-signed-gateway');
});
function shutdown() { server.close(() => process.exit(0)); }
process.on('SIGINT', shutdown); process.on('SIGTERM', shutdown);
