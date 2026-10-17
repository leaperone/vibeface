#!/usr/bin/env bash
set -euo pipefail
umask 077

usage() {
  cat <<'USAGE'
Usage: install-node-service.sh install --binary /absolute/vibeface-node --data-dir /absolute/data
       install-node-service.sh install-updater --binary /install-root/current/vibeface-node --data-dir /absolute/data
       install-node-service.sh start|stop|restart|status [--data-dir /absolute/data]
       install-node-service.sh uninstall-updater|uninstall [--data-dir /absolute/data]

Installs user-level launchd agents on macOS or systemd user services on Linux.
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
case "$action" in install|install-updater|start|stop|restart|status|uninstall-updater|uninstall) ;; *) usage >&2; exit 2 ;; esac

if [[ -n "$data_dir" ]]; then
  [[ "$data_dir" == /* && -f "$data_dir/config.json" && -f "$data_dir/identity.json" ]] || { echo 'An initialized absolute --data-dir is required.' >&2; exit 2; }
  case "$data_dir" in *$'\n'*|*$'\r'*) echo 'Paths must not contain line breaks.' >&2; exit 2 ;; esac
  data_dir="$(cd "$data_dir" && pwd -P)"
fi

if [[ "$action" == install || "$action" == install-updater ]]; then
  [[ "$binary" == /* && -x "$binary" ]] || { echo 'An absolute executable --binary is required.' >&2; exit 2; }
  [[ "$data_dir" == /* && -f "$data_dir/config.json" && -f "$data_dir/identity.json" ]] || { echo 'An initialized absolute --data-dir is required.' >&2; exit 2; }
  case "$binary$data_dir" in *$'\n'*|*$'\r'*) echo 'Paths must not contain line breaks.' >&2; exit 2 ;; esac
  log_prefix='service'
  [[ "$action" != install-updater ]] || log_prefix='update'
  if [[ "$action" == install-updater || "$(uname -s)" == Darwin ]]; then
    touch "$data_dir/$log_prefix.stdout.log" "$data_dir/$log_prefix.stderr.log"
    chmod 600 "$data_dir/$log_prefix.stdout.log" "$data_dir/$log_prefix.stderr.log"
  fi
fi

xml_escape() {
  printf '%s' "$1" | sed -e 's/&/\&amp;/g' -e 's/</\&lt;/g' -e 's/>/\&gt;/g' -e 's/"/\&quot;/g' -e "s/'/\\&apos;/g"
}

systemd_escape() {
  local value="$1"
  value="${value//\\/\\\\}"
  value="${value//\"/\\\"}"
  value="${value//%/%%}"
  printf '%s' "$value"
}

systemd_path_escape() {
  # Path directives preserve shell quotes and backslashes literally.
  local value="$1"
  printf '%s' "${value//%/%%}"
}

systemd_exec_escape() {
  local value
  value="$(systemd_escape "$1")"
  value="${value//\$/\$\$}"
  printf '%s' "$value"
}

different_data() {
  echo 'Installed service uses a different data directory or unsupported arguments; no service was changed.' >&2
  exit 1
}

case "$(uname -s)" in
  Darwin)
    label='com.leaperone.vibeface.node'
    update_label='com.leaperone.vibeface.update'
    domain="gui/$(id -u)"
    service_dir="$HOME/Library/LaunchAgents"
    service="$service_dir/$label.plist"
    update_service="$service_dir/$update_label.plist"
    check_service_data() {
      local file="$1" index="$2" installed_data
      [[ -n "$data_dir" && -f "$file" ]] || return 0
      [[ "$(plutil -extract "ProgramArguments.$((index - 1))" raw -o - "$file")" == --data-dir ]] || different_data
      if plutil -extract "ProgramArguments.$((index + 1))" raw -o - "$file" >/dev/null 2>&1; then different_data; fi
      if [[ "$index" == 3 ]]; then
        [[ "$(plutil -extract ProgramArguments.1 raw -o - "$file")" == serve ]] || different_data
      else
        [[ "$(plutil -extract ProgramArguments.1 raw -o - "$file")" == update && "$(plutil -extract ProgramArguments.2 raw -o - "$file")" == apply && "$(plutil -extract ProgramArguments.3 raw -o - "$file")" == --automatic ]] || different_data
      fi
      installed_data="$(plutil -extract "ProgramArguments.$index" raw -o - "$file")" || different_data
      [[ -d "$installed_data" ]] || different_data
      [[ "$(cd "$installed_data" && pwd -P)" == "$data_dir" ]] || different_data
    }
    case "$action" in
      start|stop|restart) check_service_data "$service" 3 ;;
      uninstall-updater) check_service_data "$update_service" 5 ;;
      install|install-updater|status|uninstall) check_service_data "$service" 3; check_service_data "$update_service" 5 ;;
    esac
    case "$action" in
      install|install-updater)
        mkdir -p "$service_dir"
        temp="$(mktemp "$service_dir/.vibeface.XXXXXX")"
        trap 'rm -f "$temp"' EXIT
        if [[ "$action" == install-updater ]]; then
          label="$update_label"
          service="$update_service"
        fi
        cat > "$temp" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>$label</string>
<key>ProgramArguments</key><array><string>$(xml_escape "$binary")</string>
PLIST
        if [[ "$action" == install-updater ]]; then
          cat >> "$temp" <<PLIST
<string>update</string><string>apply</string><string>--automatic</string>
PLIST
        else
          cat >> "$temp" <<PLIST
<string>serve</string>
PLIST
        fi
        cat >> "$temp" <<PLIST
<string>--data-dir</string><string>$(xml_escape "$data_dir")</string></array>
<key>WorkingDirectory</key><string>$(xml_escape "$(dirname "$binary")")</string>
<key>RunAtLoad</key><true/>
PLIST
        if [[ "$action" == install-updater ]]; then
          cat >> "$temp" <<PLIST
<key>StartInterval</key><integer>3600</integer>
PLIST
        else
          cat >> "$temp" <<PLIST
<key>KeepAlive</key><true/>
PLIST
        fi
        cat >> "$temp" <<PLIST
<key>StandardOutPath</key><string>$(xml_escape "$data_dir/$log_prefix.stdout.log")</string>
<key>StandardErrorPath</key><string>$(xml_escape "$data_dir/$log_prefix.stderr.log")</string>
<key>Umask</key><integer>63</integer>
</dict></plist>
PLIST
        chmod 600 "$temp"
        plutil -lint "$temp" >/dev/null
        launchctl bootout "$domain/$label" >/dev/null 2>&1 || true
        mv "$temp" "$service"
        launchctl bootstrap "$domain" "$service"
        if [[ "$action" == install ]]; then launchctl kickstart "$domain/$label"; fi
        printf 'Installed %s\n' "$service"
        ;;
      start|stop|restart)
        [[ -f "$service" ]] || { echo 'Installed node service is required.' >&2; exit 1; }
        if [[ "$action" == stop || "$action" == restart ]]; then
          if launchctl print "$domain/$label" >/dev/null 2>&1; then
            launchctl bootout "$domain/$label"
          fi
        fi
        if [[ "$action" == start || "$action" == restart ]]; then
          if ! launchctl print "$domain/$label" >/dev/null 2>&1; then
            launchctl bootstrap "$domain" "$service"
          fi
          launchctl kickstart "$domain/$label"
        fi
        ;;
      status)
        status_code=0
        launchctl print "$domain/$label" || status_code=$?
        if ! launchctl print "$domain/$update_label"; then
          if [[ -f "$update_service" ]]; then
            printf 'Updater is installed but not loaded.\n'
          else
            printf 'Updater is not installed.\n'
          fi
        fi
        exit "$status_code"
        ;;
      uninstall|uninstall-updater)
        launchctl bootout "$domain/$update_label" >/dev/null 2>&1 || true
        rm -f "$update_service"
        if [[ "$action" == uninstall ]]; then
          launchctl bootout "$domain/$label" >/dev/null 2>&1 || true
          rm -f "$service"
        fi
        printf 'Removed requested services; node data was kept.\n'
        ;;
    esac
    ;;
  Linux)
    service_dir="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
    service="$service_dir/vibeface-node.service"
    update_service="$service_dir/vibeface-update.service"
    update_timer="$service_dir/vibeface-update.timer"
    check_service_data() {
      local file="$1" unit="$2" expected="$3" contents line start='' count=0
      [[ -n "$data_dir" && -f "$file" ]] || return 0
      contents="$(systemctl --user cat "$unit")" || different_data
      while IFS= read -r line; do
        case "$line" in
          ExecStart=) start=''; count=0 ;;
          ExecStart=*) start="$line"; count=$((count + 1)) ;;
        esac
      done <<< "$contents"
      [[ "$count" == 1 && "$start" == ExecStart=*" $expected --data-dir \"$(systemd_exec_escape "$data_dir")\"" ]] || different_data
    }
    case "$action" in
      start|stop|restart) check_service_data "$service" vibeface-node.service serve ;;
      uninstall-updater) check_service_data "$update_service" vibeface-update.service 'update apply --automatic' ;;
      install|install-updater|status|uninstall)
        check_service_data "$service" vibeface-node.service serve
        check_service_data "$update_service" vibeface-update.service 'update apply --automatic'
        ;;
    esac
    case "$action" in
      install|install-updater)
        command -v systemctl >/dev/null || { echo 'systemd user services are unavailable on this host.' >&2; exit 1; }
        mkdir -p "$service_dir"
        temp="$(mktemp "$service_dir/.vibeface.XXXXXX")"
        trap 'rm -f "$temp"' EXIT
        if [[ "$action" == install-updater ]]; then
          cat > "$temp" <<UNIT
[Unit]
Description=Update Vibeface personal node
After=network-online.target

[Service]
Type=oneshot
TimeoutStartSec=infinity
ExecStart="$(systemd_exec_escape "$binary")" update apply --automatic --data-dir "$(systemd_exec_escape "$data_dir")"
WorkingDirectory=$(systemd_path_escape "$(dirname "$binary")")
StandardOutput=append:$(systemd_path_escape "$data_dir/update.stdout.log")
StandardError=append:$(systemd_path_escape "$data_dir/update.stderr.log")
UMask=0077
UNIT
          chmod 600 "$temp"
          mv "$temp" "$update_service"
          temp="$(mktemp "$service_dir/.vibeface.XXXXXX")"
          cat > "$temp" <<UNIT
[Unit]
Description=Check for Vibeface updates hourly

[Timer]
OnBootSec=1h
OnUnitActiveSec=1h
Persistent=true
Unit=vibeface-update.service

[Install]
WantedBy=timers.target
UNIT
          chmod 600 "$temp"
          mv "$temp" "$update_timer"
          systemctl --user daemon-reload
          systemctl --user enable --now vibeface-update.timer
          printf 'Installed %s and %s\n' "$update_service" "$update_timer"
        else
          cat > "$temp" <<UNIT
[Unit]
Description=Vibeface personal node
After=network-online.target

[Service]
Type=simple
ExecStart="$(systemd_exec_escape "$binary")" serve --data-dir "$(systemd_exec_escape "$data_dir")"
WorkingDirectory=$(systemd_path_escape "$(dirname "$binary")")
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
        fi
        ;;
      start|stop|restart)
        [[ -f "$service" ]] || { echo 'Installed node service is required.' >&2; exit 1; }
        systemctl --user "$action" vibeface-node.service
        ;;
      status)
        status_code=0
        systemctl --user --no-pager status vibeface-node.service || status_code=$?
        if ! systemctl --user --no-pager status vibeface-update.timer; then
          if [[ -f "$update_timer" ]]; then
            printf 'Updater is installed but its timer is not active.\n'
          else
            printf 'Updater is not installed.\n'
          fi
        fi
        systemctl --user --no-pager status vibeface-update.service || true
        exit "$status_code"
        ;;
      uninstall|uninstall-updater)
        systemctl --user disable --now vibeface-update.timer >/dev/null 2>&1 || true
        systemctl --user stop vibeface-update.service >/dev/null 2>&1 || true
        rm -f "$update_service" "$update_timer"
        if [[ "$action" == uninstall ]]; then
          systemctl --user disable --now vibeface-node.service >/dev/null 2>&1 || true
          rm -f "$service"
        fi
        systemctl --user daemon-reload
        printf 'Removed requested services; node data was kept.\n'
        ;;
    esac
    ;;
  *) echo 'Supported service platforms are macOS and Linux with systemd.' >&2; exit 1 ;;
esac
