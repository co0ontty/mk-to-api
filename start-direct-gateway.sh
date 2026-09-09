#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
SERVER_SCRIPT="$SCRIPT_DIR/direct-gateway-server.mjs"
PID_FILE="${DIRECT_GATEWAY_PID_FILE:-$HOME/.monkeycode-direct-gateway.pid}"
LOG_FILE="${DIRECT_GATEWAY_LOG_FILE:-$HOME/Library/Logs/monkeycode-direct-gateway.log}"
HOST="${DIRECT_GATEWAY_HOST:-127.0.0.1}"
PORT="${DIRECT_GATEWAY_PORT:-8765}"
LOCAL_KEY="${DIRECT_GATEWAY_KEY:-local-monkeycode-direct}"

mkdir -p "$(dirname -- "$LOG_FILE")"

pid_is_server() {
  local pid="$1"
  kill -0 "$pid" 2>/dev/null || return 1
  ps -p "$pid" -o command= 2>/dev/null | grep -Fq "$SERVER_SCRIPT"
}

running_pid() {
  if [ -f "$PID_FILE" ]; then
    local pid
    pid="$(cat "$PID_FILE")"
    if pid_is_server "$pid"; then
      echo "$pid"
      return 0
    fi
  fi
  return 1
}

health_check() {
  curl -fsS --connect-timeout 2 --max-time 5 \
    "http://$HOST:$PORT/health" >/dev/null
}

start_server() {
  if pid="$(running_pid)"; then
    if health_check; then
      echo "direct gateway already running (PID $pid)"
      return 0
    fi
    echo "direct gateway process exists but health check failed" >&2
    return 1
  fi

  if health_check; then
    echo "port $PORT is already serving a direct gateway"
    exit 0
  fi

  nohup env \
    DIRECT_GATEWAY_HOST="$HOST" \
    DIRECT_GATEWAY_PORT="$PORT" \
    DIRECT_GATEWAY_KEY="$LOCAL_KEY" \
    node "$SERVER_SCRIPT" >>"$LOG_FILE" 2>&1 </dev/null &
  pid=$!
  echo "$pid" >"$PID_FILE"

  for _ in {1..20}; do
    if health_check; then
      echo "direct gateway started (PID $pid)"
      echo "endpoint: http://$HOST:$PORT/v1"
      echo "log: $LOG_FILE"
      return 0
    fi
    sleep 0.25
  done

  echo "failed to start direct gateway; see $LOG_FILE" >&2
  cat "$LOG_FILE" >&2 || true
  exit 1
}

stop_server() {
  if ! pid="$(running_pid)"; then
    echo "direct gateway is not running"
    rm -f "$PID_FILE"
    return 0
  fi

  kill "$pid"
  for _ in {1..20}; do
    if ! kill -0 "$pid" 2>/dev/null; then
      rm -f "$PID_FILE"
      echo "direct gateway stopped"
      return 0
    fi
    sleep 0.25
  done

  echo "direct gateway did not stop after 5 seconds; PID $pid remains" >&2
  return 1
}

status_server() {
  if pid="$(running_pid)"; then
    if health_check; then
      echo "direct gateway is running (PID $pid)"
      curl -fsS "http://$HOST:$PORT/health"
      echo
      return 0
    fi
    echo "direct gateway process exists but is unhealthy (PID $pid)" >&2
    return 1
  fi
  echo "direct gateway is not running"
  return 1
}

case "${1:-start}" in
  start) start_server ;;
  stop) stop_server ;;
  restart) stop_server || true; start_server ;;
  status) status_server ;;
  *)
    echo "Usage: $0 {start|stop|restart|status}" >&2
    exit 2
    ;;
esac
