#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: install-node-service.sh install --binary /absolute/vibeface-node --data-dir /absolute/data
       install-node-service.sh status
       install-node-service.sh uninstall

Installs a user-level launchd agent on macOS or a systemd user service on Linux.
Initialize the node before installing. Uninstall keeps all node data.
USAGE
}

action="${1:-}"
if [[ -z "$action" || "$action" == '--help' || "$action" == '-h' ]]; then
  usage
  exit 0
fi
shift
binary=''
data_dir=''
while (($#)); do
  case "$1" in
    --binary|--data-dir)
      (($# >= 2)) || { usage >&2; exit 2; }
      if [[ "$1" == '--binary' ]]; then binary="$2"; else data_dir="$2"; fi
      shift 2
      ;;
    *) usage >&2; exit 2 ;;
  esac
done
case "$action" in install|status|uninstall) ;; *) usage >&2; exit 2 ;; esac

if [[ "$action" == install ]]; then
  [[ "$binary" == /* && -x "$binary" ]] || { echo 'An absolute executable --binary is required.' >&2; exit 2; }
  [[ "$data_dir" == /* && -f "$data_dir/config.json" && -f "$data_dir/identity.json" ]] || { echo 'An initialized absolute --data-dir is required.' >&2; exit 2; }
  case "$binary$data_dir" in *$'\n'*|*$'\r'*) echo 'Paths must not contain line breaks.' >&2; exit 2 ;; esac
fi

xml_escape() {
  local value="$1"
  value="${value//&/\&amp;}"
  value="${value//</\&lt;}"
  value="${value//>/\&gt;}"
  value="${value//\"/\&quot;}"
  value="${value//\'/\&apos;}"
  printf '%s' "$value"
}

systemd_escape() {
  local value="$1"
  value="${value//\\/\\\\}"
  value="${value//\"/\\\"}"
  value="${value//%/%%}"
  printf '%s' "$value"
}

systemd_exec_escape() {
  local value
  value="$(systemd_escape "$1")"
  value="${value//\$/\$\$}"
  printf '%s' "$value"
}

case "$(uname -s)" in
  Darwin)
    label='com.leaperone.vibeface.node'
    domain="gui/$(id -u)"
    service_dir="$HOME/Library/LaunchAgents"
    service="$service_dir/$label.plist"
    case "$action" in
      install)
        mkdir -p "$service_dir"
        temp="$(mktemp "$service_dir/.vibeface.XXXXXX")"
        trap 'rm -f "$temp"' EXIT
        cat > "$temp" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>$label</string>
<key>ProgramArguments</key><array><string>$(xml_escape "$binary")</string><string>serve</string><string>--data-dir</string><string>$(xml_escape "$data_dir")</string></array>
<key>WorkingDirectory</key><string>$(xml_escape "$(dirname "$binary")")</string>
<key>RunAtLoad</key><true/>
<key>KeepAlive</key><true/>
<key>StandardOutPath</key><string>$(xml_escape "$data_dir/service.stdout.log")</string>
<key>StandardErrorPath</key><string>$(xml_escape "$data_dir/service.stderr.log")</string>
<key>Umask</key><integer>63</integer>
</dict></plist>
PLIST
        chmod 600 "$temp"
        plutil -lint "$temp" >/dev/null
        launchctl bootout "$domain/$label" >/dev/null 2>&1 || true
        mv "$temp" "$service"
        launchctl bootstrap "$domain" "$service"
        launchctl kickstart "$domain/$label"
        printf 'Installed %s\n' "$service"
        ;;
      status) launchctl print "$domain/$label" ;;
      uninstall)
        launchctl bootout "$domain/$label" >/dev/null 2>&1 || true
        rm -f "$service"
        printf 'Removed service; node data was kept.\n'
        ;;
    esac
    ;;
  Linux)
    service_dir="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
    service="$service_dir/vibeface-node.service"
    case "$action" in
      install)
        command -v systemctl >/dev/null || { echo 'systemd user services are unavailable on this host.' >&2; exit 1; }
        mkdir -p "$service_dir"
        temp="$(mktemp "$service_dir/.vibeface.XXXXXX")"
        trap 'rm -f "$temp"' EXIT
        cat > "$temp" <<UNIT
[Unit]
Description=Vibeface personal node
After=network-online.target

[Service]
Type=simple
ExecStart="$(systemd_exec_escape "$binary")" serve --data-dir "$(systemd_exec_escape "$data_dir")"
WorkingDirectory="$(systemd_escape "$(dirname "$binary")")"
Restart=on-failure
RestartSec=3
UMask=0077

[Install]
WantedBy=default.target
UNIT
        chmod 600 "$temp"
        mv "$temp" "$service"
        systemctl --user daemon-reload
        systemctl --user enable --now vibeface-node.service
        printf 'Installed %s\n' "$service"
        ;;
      status) systemctl --user status vibeface-node.service ;;
      uninstall)
        systemctl --user disable --now vibeface-node.service >/dev/null 2>&1 || true
        rm -f "$service"
        systemctl --user daemon-reload
        printf 'Removed service; node data was kept.\n'
        ;;
    esac
    ;;
  *) echo 'Supported service platforms are macOS and Linux with systemd.' >&2; exit 1 ;;
esac
