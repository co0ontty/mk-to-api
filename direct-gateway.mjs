#!/usr/bin/env node

import crypto from 'node:crypto';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';

const args = process.argv.slice(2);
const flag = (name, fallback) => {
  const index = args.indexOf(name);
  return index >= 0 ? args[index + 1] ?? fallback : fallback;
};

const defaultConfigDir = path.join(os.homedir(), 'Library/Application Support/com.chaitin.baizhi.monkeycode');
const configDir = process.env.MONKEYCODE_CONFIG_DIR || defaultConfigDir;
const keyPath = process.env.MONKEYCODE_OHMYAGENT_KEY || path.join(configDir, 'monkeycode-ohmyagent-key.json');
const settingsPath = process.env.OHMYAGENT_SETTINGS || path.join(configDir, 'ohmyagent/settings.json');
const requestedModel = flag('--model', process.env.MODEL || 'gpt-6-astra');
const prompt = flag('--prompt', process.env.PROMPT || 'Reply with OK only.');
const systemPrompt = flag('--system', process.env.SYSTEM || 'You are a helpful assistant.');
const stream = args.includes('--stream');

function fail(message) {
  console.error(`error: ${message}`);
  process.exit(1);
}

async function readJson(filePath, label) {
  try {
    return JSON.parse(await fs.readFile(filePath, 'utf8'));
  } catch (error) {
    fail(`cannot read ${label}: ${filePath} (${error.message})`);
  }
}

const keyConfig = await readJson(keyPath, 'OhMyAgent key');
const settings = await readJson(settingsPath, 'OhMyAgent settings');
const models = Object.values(settings.models || {});
const proxyBaseUrl = String(keyConfig.base_url || '').replace(/\/$/, '');
const normalizedModel = requestedModel.includes('/') ? requestedModel : `monkeycode-ultra/${requestedModel}`;
const modelConfig = models.find((entry) => (
  entry.base_url === proxyBaseUrl &&
  (entry.model === requestedModel || entry.model === normalizedModel)
));

if (!proxyBaseUrl || !keyConfig.api_key || !keyConfig.signing_secret) {
  fail('OhMyAgent key is missing base_url, api_key, or signing_secret');
}
if (!modelConfig) {
  fail(`proxy model is not configured: ${requestedModel}`);
}

const model = modelConfig.model;
const apiKey = modelConfig.api_key || keyConfig.api_key;
const signature = crypto
  .createHmac('sha256', keyConfig.signing_secret)
  .update(systemPrompt, 'utf8')
  .digest('hex');
const body = {
  model,
  input: [
    { role: 'developer', content: [{ type: 'input_text', text: systemPrompt }] },
    { role: 'user', content: [{ type: 'input_text', text: prompt }] },
  ],
  max_output_tokens: 1024,
  store: false,
  stream,
};
const response = await fetch(`${proxyBaseUrl}/responses`, {
  method: 'POST',
  headers: {
    Authorization: `Bearer ${apiKey}`,
    'X-OhMyAgent-Signature': `v1=${signature}`,
    'Content-Type': 'application/json',
    Accept: stream ? 'text/event-stream' : 'application/json',
  },
  body: JSON.stringify(body),
});

if (!response.ok) {
  const detail = (await response.text()).slice(0, 1000);
  fail(`gateway HTTP ${response.status}: ${detail}`);
}

if (stream) {
  if (!response.body) fail('gateway returned no response body');
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  while (true) {
    const { value, done } = await reader.read();
    if (done) break;
    process.stdout.write(decoder.decode(value, { stream: true }));
  }
  process.stdout.write(decoder.decode());
} else {
  const data = await response.json();
  const text = data.output_text ?? (data.output || [])
    .flatMap((item) => item.content || [])
    .filter((item) => item.type === 'output_text')
    .map((item) => item.text)
    .join('');
  process.stdout.write(text ? `${text}\n` : `${JSON.stringify(data)}\n`);
}
