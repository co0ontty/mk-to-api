#!/usr/bin/env bash
  set -euo pipefail

  CFG="$HOME/Library/Application Support/com.chaitin.baizhi.monkeycode/config.json"
  KEYFILE="$HOME/Library/Application Support/com.chaitin.baizhi.monkeycode/monkeycode-ohmyagent-key.json"

  OPENAI_URL="$(jq -r '
    .models[]
    | select(.source == "baizhi" and .provider == "openai_responses")
    | .base_url
  ' "$CFG" | head -1)"

  ANTHROPIC_URL="$(jq -r '
    .models[]
    | select(.source == "baizhi" and .provider == "anthropic")
    | .base_url
  ' "$CFG" | head -1)"

  API_KEY="$(jq -r '
    .models[]
    | select(.source == "baizhi")
    | .api_key
  ' "$CFG" | head -1)"

  OHMY_URL="$(jq -r '.base_url' "$KEYFILE")"
  OHMY_KEY="$(jq -r '.api_key' "$KEYFILE")"

  echo "===== MonkeyCode 云模型配置 ====="
  echo
  echo "OpenAI Responses URL:"
  echo "$OPENAI_URL"
  echo
  echo "Anthropic URL:"
  echo "$ANTHROPIC_URL"
  echo
  echo "API Key:"
  echo "$API_KEY"
  echo
  echo "===== 可用模型 ====="
  jq -r '
    .models[]
    | select(.source == "baizhi")
    | "\(.provider)\t\(.model)"
  ' "$CFG" | sort -u
  echo
  echo "===== OhMyAgent 本地代理配置 ====="
  echo
  echo "OhMyAgent URL:"
  echo "$OHMY_URL"
  echo
  echo "OhMyAgent Key:"
  echo "$OHMY_KEY"
  echo
  echo "===== 连通性测试 ====="

  MODELS_HTTP_CODE="$(
    curl -sS \
      --connect-timeout 10 \
      --max-time 30 \
      -o /tmp/monkeycode-models-test.json \
      -w '%{http_code}' \
      -H "Authorization: Bearer $API_KEY" \
      -H "Accept: application/json" \
      "$OPENAI_URL/models"
  )"

  echo
  echo "GET $OPENAI_URL/models"
  echo "HTTP: $MODELS_HTTP_CODE"
  jq -c '{
    object: .object,
    model_count: (.data | length),
    error: (.error // null)
  }' /tmp/monkeycode-models-test.json 2>/dev/null || cat /tmp/monkeycode-models-test.json

  RESPONSES_HTTP_CODE="$(
    curl -sS \
      --connect-timeout 10 \
      --max-time 60 \
      -o /tmp/monkeycode-responses-test.json \
      -w '%{http_code}' \
      -H "Authorization: Bearer $API_KEY" \
      -H "Content-Type: application/json" \
      -H "Accept: application/json" \
      "$OPENAI_URL/responses" \
      -d '{
        "model": "gpt-5.5",
        "input": "Reply with OK only.",
        "max_output_tokens": 16,
        "store": false
      }'
  )"

  echo
  echo "POST $OPENAI_URL/responses"
  echo "HTTP: $RESPONSES_HTTP_CODE"
  jq -c '{
    id: .id,
    status: .status,
    model: .model,
    output_text: (
      [.output[]?.content[]? | select(.type == "output_text") | .text]
      | join("")
    ),
    error: (.error // null)
  }' /tmp/monkeycode-responses-test.json 2>/dev/null || cat /tmp/monkeycode-responses-test.json

  echo
  echo "===== 完成 ====="
