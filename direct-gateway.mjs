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
const configPath = flag('--config', process.env.MONKEYCODE_GATEWAY_CONFIG || path.join(defaultConfigDir, 'direct-gateway.json'));
let fileConfig = {};
try {
  fileConfig = JSON.parse(await fs.readFile(configPath, 'utf8'));
} catch (error) {
  if (error.code !== 'ENOENT') fail(`cannot read gateway config: ${configPath} (${error.message})`);
}
const config = { ...fileConfig };
const configValue = (name, fallback) => config[name.replaceAll('-', '_')] ?? config[name] ?? fallback;
const configured = (name, envName, fallback) => flag(`--${name}`, process.env[envName] ?? configValue(name, fallback));

const configDir = configured('config-dir', 'MONKEYCODE_CONFIG_DIR', defaultConfigDir);
const keyPath = configured('key-file', 'MONKEYCODE_OHMYAGENT_KEY', path.join(configDir, 'monkeycode-ohmyagent-key.json'));
const settingsPath = configured('settings', 'OHMYAGENT_SETTINGS', path.join(configDir, 'ohmyagent/settings.json'));
const requestedModel = configured('model', 'MODEL', 'gpt-6-astra');
const prompt = configured('prompt', 'PROMPT', 'Reply with OK only.');
const systemPrompt = configured('system', 'SYSTEM', 'You are a helpful assistant.');
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
const proxyBaseUrl = String(config.upstream_host ?? config.host ?? keyConfig.base_url ?? '').replace(/\/$/, '');
const configuredApiKey = config.upstream_key ?? config.api_key ?? config.key ?? keyConfig.api_key;
const signingSecret = config.signing_secret || keyConfig.signing_secret;
const normalizedModel = requestedModel.includes('/') ? requestedModel : `monkeycode-ultra/${requestedModel}`;
const modelConfig = models.find((entry) => (
  entry.base_url === proxyBaseUrl &&
  (entry.model === requestedModel || entry.model === normalizedModel)
));

if (!proxyBaseUrl || !configuredApiKey || !signingSecret) {
  fail('gateway config is missing host, key, or signing_secret');
}
if (!modelConfig) {
  fail(`proxy model is not configured: ${requestedModel}`);
}

const model = modelConfig.model;
const apiKey = modelConfig.api_key || configuredApiKey;
const signature = crypto
  .createHmac('sha256', signingSecret)
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
