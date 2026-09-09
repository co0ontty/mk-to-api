#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
SERVER_SCRIPT="$SCRIPT_DIR/direct-gateway-server.mjs"
PID_FILE="${DIRECT_GATEWAY_PID_FILE:-$HOME/.monkeycode-direct-gateway.pid}"
LOG_FILE="${DIRECT_GATEWAY_LOG_FILE:-$HOME/Library/Logs/monkeycode-direct-gateway.log}"
HOST="${DIRECT_GATEWAY_HOST:-127.0.0.1}"
PORT="${DIRECT_GATEWAY_PORT:-8765}"
LOCAL_KEY="${DIRECT_GATEWAY_KEY:-local-monkeycode-direct}"
LABEL="${DIRECT_GATEWAY_LAUNCH_LABEL:-com.monkeycode.direct-gateway}"
LAUNCH_DOMAIN="${DIRECT_GATEWAY_LAUNCH_DOMAIN:-gui/$(id -u)}"
PLIST_FILE="${DIRECT_GATEWAY_PLIST_FILE:-$HOME/Library/LaunchAgents/$LABEL.plist}"
SERVICE_TARGET="$LAUNCH_DOMAIN/$LABEL"
LAUNCHCTL="${LAUNCHCTL:-/bin/launchctl}"
PLUTIL="${PLUTIL:-/usr/bin/plutil}"
NODE_BIN="${DIRECT_GATEWAY_NODE:-}"

if [ -z "$NODE_BIN" ]; then
  NODE_BIN="$(command -v node || true)"
fi

mkdir -p "$(dirname -- "$LOG_FILE")" "$(dirname -- "$PLIST_FILE")"

xml_escape() {
  local value="$1"
  value="${value//&/&amp;}"
  value="${value//</&lt;}"
  value="${value//>/&gt;}"
  value="${value//\"/&quot;}"
  printf '%s' "$value"
}

service_loaded() {
  "$LAUNCHCTL" print "$SERVICE_TARGET" >/dev/null 2>&1
}

service_pid() {
  "$LAUNCHCTL" print "$SERVICE_TARGET" 2>/dev/null |
    awk '$1 == "pid" && $2 == "=" { print $3; exit }'
}

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

write_pid_file() {
  local pid
  pid="$(service_pid || true)"
  if [ -n "$pid" ]; then
    printf '%s\n' "$pid" >"$PID_FILE"
  fi
}

write_plist() {
  local escaped_node escaped_script escaped_host escaped_port escaped_key escaped_log
  escaped_node="$(xml_escape "$NODE_BIN")"
  escaped_script="$(xml_escape "$SERVER_SCRIPT")"
  escaped_host="$(xml_escape "$HOST")"
  escaped_port="$(xml_escape "$PORT")"
  escaped_key="$(xml_escape "$LOCAL_KEY")"
  escaped_log="$(xml_escape "$LOG_FILE")"

  cat >"$PLIST_FILE" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>$LABEL</string>
  <key>ProgramArguments</key>
  <array>
    <string>$escaped_node</string>
    <string>$escaped_script</string>
  </array>
  <key>WorkingDirectory</key>
  <string>$SCRIPT_DIR</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>DIRECT_GATEWAY_HOST</key>
    <string>$escaped_host</string>
    <key>DIRECT_GATEWAY_PORT</key>
    <string>$escaped_port</string>
    <key>DIRECT_GATEWAY_KEY</key>
    <string>$escaped_key</string>
  </dict>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>ThrottleInterval</key>
  <integer>5</integer>
  <key>StandardOutPath</key>
  <string>$escaped_log</string>
  <key>StandardErrorPath</key>
  <string>$escaped_log</string>
</dict>
</plist>
PLIST

  "$PLUTIL" -lint "$PLIST_FILE" >/dev/null
  chmod 600 "$PLIST_FILE"
}

show_running() {
  local pid
  pid="$(service_pid || true)"
  if [ -n "$pid" ]; then
    echo "direct gateway is running (PID $pid)"
  else
    echo "direct gateway is running"
  fi
  curl -fsS "http://$HOST:$PORT/health"
  echo
}

start_server() {
  if [ ! -x "$NODE_BIN" ]; then
    echo "node executable not found: ${NODE_BIN:-<empty>}" >&2
    return 1
  fi

  if service_loaded; then
    if health_check; then
      write_pid_file
      show_running
      return 0
    fi
    echo "direct gateway service is loaded but unhealthy; restarting" >&2
    "$LAUNCHCTL" kickstart -k "$SERVICE_TARGET"
  else
    if health_check; then
      echo "port $PORT is already serving an unmanaged direct gateway" >&2
      return 0
    fi
    write_plist
    "$LAUNCHCTL" bootstrap "$LAUNCH_DOMAIN" "$PLIST_FILE"
    "$LAUNCHCTL" kickstart -k "$SERVICE_TARGET"
  fi

  for _ in {1..20}; do
    if health_check; then
      write_pid_file
      show_running
      echo "endpoint: http://$HOST:$PORT/v1"
      echo "log: $LOG_FILE"
      return 0
    fi
    sleep 0.25
  done

  echo "failed to start direct gateway; see $LOG_FILE" >&2
  if [ -f "$LOG_FILE" ]; then
    tail -30 "$LOG_FILE" >&2 || true
  fi
  if service_loaded; then
    "$LAUNCHCTL" print "$SERVICE_TARGET" >&2 || true
  fi
  return 1
}

wait_until_stopped() {
  for _ in {1..20}; do
    if ! health_check; then
      return 0
    fi
    sleep 0.25
  done
  return 1
}

stop_server() {
  local stopped=0

  if service_loaded; then
    "$LAUNCHCTL" bootout "$SERVICE_TARGET"
    stopped=1
  fi

  if pid="$(running_pid)"; then
    kill "$pid" 2>/dev/null || true
    stopped=1
  fi

  rm -f "$PLIST_FILE" "$PID_FILE"

  if ! wait_until_stopped; then
    echo "direct gateway is still responding on port $PORT" >&2
    return 1
  fi

  if [ "$stopped" -eq 1 ]; then
    echo "direct gateway stopped"
  else
    echo "direct gateway is not running"
  fi
}

status_server() {
  if service_loaded; then
    if health_check; then
      write_pid_file
      show_running
      return 0
    fi
    echo "direct gateway service is loaded but unhealthy" >&2
    "$LAUNCHCTL" print "$SERVICE_TARGET" >&2 || true
    return 1
  fi

  if health_check; then
    echo "direct gateway responds on port $PORT but is not managed by this script" >&2
    curl -fsS "http://$HOST:$PORT/health"
    echo
    return 0
  fi

  echo "direct gateway is not running"
  return 1
}

show_help() {
  cat <<HELP
Usage: $0 {start|stop|restart|status|help}

Commands:
  start    Start the direct gateway as a background launchd service
  stop     Stop and unload the background service
  restart  Stop and start the service again
  status   Show service state and health response
  help     Show this help

Endpoint: http://$HOST:$PORT/v1
Log:      $LOG_FILE
HELP
}

case "${1:-start}" in
  start) start_server ;;
  stop) stop_server ;;
  restart) stop_server || true; start_server ;;
  status) status_server ;;
  help|-h|--help) show_help ;;
  *)
    show_help >&2
    exit 2
    ;;
esac
