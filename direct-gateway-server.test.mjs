#!/usr/bin/env node

import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import fs from 'node:fs/promises';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';

const temporaryDir = await fs.mkdtemp(path.join(os.tmpdir(), 'direct-gateway-test-'));
const upstreamPort = 18181;
const gatewayPort = 18182;
const model = 'monkeycode-ultra/gpt-test';
let lastRequest;
const upstream = http.createServer(async (req, res) => {
  const chunks = [];
  for await (const chunk of req) chunks.push(chunk);
  lastRequest = JSON.parse(Buffer.concat(chunks));
  if (lastRequest.stream) {
    res.writeHead(200, { 'content-type': 'text/event-stream' });
    res.write(`event: response.output_text.delta\ndata: ${JSON.stringify({ type: 'response.output_text.delta', delta: 'hel' })}\n\n`);
    res.write(`event: response.output_text.delta\ndata: ${JSON.stringify({ type: 'response.output_text.delta', delta: 'lo' })}\n\n`);
    res.end(`event: response.completed\ndata: ${JSON.stringify({ type: 'response.completed', response: { id: 'resp_test', status: 'completed', model, output: [], usage: { input_tokens: 2, output_tokens: 1, total_tokens: 3 } } })}\n\n`);
  } else {
    res.writeHead(200, { 'content-type': 'application/json' });
    res.end(JSON.stringify({ id: 'resp_test', object: 'response', status: 'completed', model, output: [{ type: 'message', content: [{ type: 'output_text', text: 'hello' }] }], usage: { input_tokens: 2, output_tokens: 1, total_tokens: 3 } }));
  }
});
await new Promise((resolve) => upstream.listen(upstreamPort, '127.0.0.1', resolve));
await fs.writeFile(path.join(temporaryDir, 'key.json'), JSON.stringify({ base_url: `http://127.0.0.1:${upstreamPort}/v1`, api_key: 'upstream', signing_secret: 'secret' }));
await fs.writeFile(path.join(temporaryDir, 'settings.json'), JSON.stringify({ models: { test: { base_url: `http://127.0.0.1:${upstreamPort}/v1`, model } } }));
await fs.writeFile(path.join(temporaryDir, 'gateway.json'), JSON.stringify({ host: '127.0.0.1', port: gatewayPort, key: 'local', key_file: path.join(temporaryDir, 'key.json'), settings: path.join(temporaryDir, 'settings.json') }));
const child = spawn(process.execPath, [path.resolve('direct-gateway-server.mjs'), '--config', path.join(temporaryDir, 'gateway.json')], { stdio: ['ignore', 'pipe', 'inherit'] });
try {
  await new Promise((resolve, reject) => { const timer = setTimeout(() => reject(new Error('gateway did not start')), 3000); child.stdout.on('data', (chunk) => { if (chunk.toString().includes('listening')) { clearTimeout(timer); resolve(); } }); child.once('exit', (code) => reject(new Error(`gateway exited ${code}`))); });
  const base = `http://127.0.0.1:${gatewayPort}`;
  const info = await fetch(`${base}/v1`); assert.equal(info.status, 200); assert.equal((await info.json()).status, 'ok');
  const unauthorized = await fetch(`${base}/v1/models`); assert.equal(unauthorized.status, 401); assert.equal((await unauthorized.json()).error.code, 'invalid_api_key');
  const headers = { authorization: 'Bearer local', 'content-type': 'application/json' };
  const chat = await fetch(`${base}/v1/chat/completions`, { method: 'POST', headers, body: JSON.stringify({ model: 'gpt-test', messages: [{ role: 'system', content: 'be brief' }, { role: 'user', content: 'hi' }] }) });
  assert.equal(chat.status, 200); assert.equal((await chat.json()).choices[0].message.content, 'hello'); assert.equal(lastRequest.model, model); assert.equal(lastRequest.input[0].role, 'developer');
  const chatStream = await fetch(`${base}/v1/chat/completions`, { method: 'POST', headers, body: JSON.stringify({ model: 'gpt-test', stream: true, messages: [{ role: 'user', content: 'hi' }] }) });
  const chatText = await chatStream.text(); assert.match(chatText, /"content":"hel"/); assert.match(chatText, /"finish_reason":"stop"/); assert.match(chatText, /data: \[DONE\]/);
  const responseStream = await fetch(`${base}/v1/responses`, { method: 'POST', headers, body: JSON.stringify({ model: 'gpt-test', stream: true, input: 'hi' }) });
  const responseText = await responseStream.text(); assert.match(responseText, /response\.output_text\.delta/); assert.match(responseText, /response\.completed/);
  console.log('direct gateway integration tests passed');
} finally {
  child.kill('SIGTERM'); upstream.close(); await fs.rm(temporaryDir, { recursive: true, force: true });
}
