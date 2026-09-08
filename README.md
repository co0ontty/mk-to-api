# OhMyAgent Pi Bridge

This local bridge adapts the OpenAI Responses API used by `pi` to the
MonkeyCode-bundled `ohmyagent` CLI. It listens on `127.0.0.1` by default and
does not expose the MonkeyCode credentials over the network.

## Start

```bash
node ohmyagent-pi-bridge.mjs
```

可通过 `--cwd` 指定 pi 当前项目目录；未指定时使用启动目录：

```bash
node ohmyagent-pi-bridge.mjs --cwd /path/to/project
```

The default port is `8765`. Override it with `OHMYAGENT_BRIDGE_PORT`.

## Use with pi

The matching provider is `monkeycode-bridge` in `~/.pi/agent/models.json`:

```bash
pi --provider monkeycode-bridge --model gpt-6-astra \
  --thinking minimal --print "Reply with OK only."
```

The bridge translates `gpt-6-astra` into the internal
`monkeycode-ultra/gpt-6-astra` model and uses the App's OhMyAgent configuration
directory. Keep the bridge bound to `127.0.0.1` unless you intentionally add
network authentication and access controls.
