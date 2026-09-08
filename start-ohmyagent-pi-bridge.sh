#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
BRIDGE_SCRIPT="$SCRIPT_DIR/ohmyagent-pi-bridge.mjs"
PID_FILE="${OHMYAGENT_BRIDGE_PID_FILE:-$HOME/.ohmyagent-pi-bridge.pid}"
LOG_FILE="${OHMYAGENT_BRIDGE_LOG_FILE:-$HOME/Library/Logs/ohmyagent-pi-bridge.log}"
HOST="${OHMYAGENT_BRIDGE_HOST:-127.0.0.1}"
PORT="${OHMYAGENT_BRIDGE_PORT:-8765}"
LOCAL_KEY="${OHMYAGENT_BRIDGE_KEY:-local-ohmyagent-bridge}"

mkdir -p "$(dirname -- "$LOG_FILE")"

pid_is_bridge() {
  local pid="$1"
  kill -0 "$pid" 2>/dev/null || return 1
  ps -p "$pid" -o command= 2>/dev/null | grep -Fq "$BRIDGE_SCRIPT"
}

running_pid() {
  if [ -f "$PID_FILE" ]; then
    local pid
    pid="$(cat "$PID_FILE")"
    if pid_is_bridge "$pid"; then
      echo "$pid"
      return 0
    fi
  fi
  return 1
}

health_check() {
  curl -fsS --connect-timeout 2 --max-time 5 \
    -H "Authorization: Bearer $LOCAL_KEY" \
    "http://$HOST:$PORT/health" >/dev/null
}

start_bridge() {
  if pid="$(running_pid)"; then
    if health_check; then
      echo "ohmyagent-pi-bridge already running (PID $pid)"
      return 0
    fi
    echo "bridge process exists but health check failed" >&2
    return 1
  fi

  if health_check; then
    echo "port $PORT is already serving an OhMyAgent bridge"
    exit 0
  fi

  nohup env \
    OHMYAGENT_BRIDGE_HOST="$HOST" \
    OHMYAGENT_BRIDGE_PORT="$PORT" \
    OHMYAGENT_BRIDGE_KEY="$LOCAL_KEY" \
    OHMYAGENT_BRIDGE_CWD="${OHMYAGENT_BRIDGE_CWD:-$PWD}" \
    node "$BRIDGE_SCRIPT" >>"$LOG_FILE" 2>&1 </dev/null &
  pid=$!
  echo "$pid" >"$PID_FILE"

  for _ in {1..20}; do
    if health_check; then
      echo "ohmyagent-pi-bridge started (PID $pid)"
      echo "endpoint: http://$HOST:$PORT/v1"
      echo "log: $LOG_FILE"
      return 0
    fi
    sleep 0.25
  done

  echo "failed to start ohmyagent-pi-bridge; see $LOG_FILE" >&2
  cat "$LOG_FILE" >&2 || true
  exit 1
}

stop_bridge() {
  if ! pid="$(running_pid)"; then
    echo "ohmyagent-pi-bridge is not running"
    rm -f "$PID_FILE"
    return 0
  fi

  kill "$pid"
  for _ in {1..20}; do
    if ! kill -0 "$pid" 2>/dev/null; then
      rm -f "$PID_FILE"
      echo "ohmyagent-pi-bridge stopped"
      return 0
    fi
    sleep 0.25
  done

  echo "bridge did not stop after 5 seconds; PID $pid remains" >&2
  return 1
}

status_bridge() {
  if pid="$(running_pid)"; then
    if health_check; then
      echo "ohmyagent-pi-bridge is running (PID $pid)"
      curl -fsS -H "Authorization: Bearer $LOCAL_KEY" "http://$HOST:$PORT/health"
      echo
      return 0
    fi
    echo "ohmyagent-pi-bridge process exists but is unhealthy (PID $pid)" >&2
    return 1
  fi
  echo "ohmyagent-pi-bridge is not running"
  return 1
}

case "${1:-start}" in
  start) start_bridge ;;
  stop) stop_bridge ;;
  restart) stop_bridge || true; start_bridge ;;
  status) status_bridge ;;
  *)
    echo "Usage: $0 {start|stop|restart|status}" >&2
    exit 2
    ;;
esac
