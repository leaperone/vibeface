#!/usr/bin/env bash
set -euo pipefail
umask 077

usage() {
  cat <<'USAGE'
Usage: install-node-service.sh install --binary /absolute/vibeface --data-dir /absolute/data
       install-node-service.sh install-updater --binary /install-root/current/vibeface --data-dir /absolute/data
       install-node-service.sh start|stop|restart|status [--data-dir /absolute/data]
       install-node-service.sh uninstall-updater|uninstall-node|uninstall [--data-dir /absolute/data]

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
service_id='node'
while (($#)); do
  case "$1" in
    --binary|--data-dir|--service-id)
      (($# >= 2)) || { usage >&2; exit 2; }
      if [[ "$1" == '--binary' ]]; then binary="$2"; elif [[ "$1" == '--data-dir' ]]; then data_dir="$2"; else service_id="$2"; fi
      shift 2
      ;;
    *) usage >&2; exit 2 ;;
  esac
done
[[ "$service_id" =~ ^[a-z0-9][a-z0-9-]{0,40}$ ]] || { echo 'Invalid service identity.' >&2; exit 2; }
case "$action" in install|install-updater|start|stop|restart|status|verify|uninstall-updater|uninstall-node|uninstall) ;; *) usage >&2; exit 2 ;; esac

if [[ -n "$data_dir" ]]; then
  [[ "$data_dir" == /* && -f "$data_dir/config.json" && -f "$data_dir/identity.json" ]] || { echo 'An initialized absolute --data-dir is required.' >&2; exit 2; }
  case "$data_dir" in *$'\n'*|*$'\r'*) echo 'Paths must not contain line breaks.' >&2; exit 2 ;; esac
  data_dir="$(cd "$data_dir" && pwd -P)"
fi

if [[ -n "$binary" ]]; then
  [[ "$binary" == /* && -x "$binary" ]] || { echo 'An absolute executable --binary is required.' >&2; exit 2; }
  case "$binary$data_dir" in *$'\n'*|*$'\r'*) echo 'Paths must not contain line breaks.' >&2; exit 2 ;; esac
fi
if [[ "$action" == install || "$action" == install-updater ]]; then
  [[ -n "$binary" ]] || { echo 'An absolute executable --binary is required.' >&2; exit 2; }
  [[ "$data_dir" == /* && -f "$data_dir/config.json" && -f "$data_dir/identity.json" ]] || { echo 'An initialized absolute --data-dir is required.' >&2; exit 2; }
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
    if [[ "$service_id" == node ]]; then
      label='com.leaperone.vibeface.node'; update_label='com.leaperone.vibeface.update'
    else
      label="com.leaperone.vibeface.node.$service_id"; update_label="com.leaperone.vibeface.update.$service_id"
    fi
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
      if [[ -n "$binary" ]]; then
        local installed_binary
        installed_binary="$(plutil -extract ProgramArguments.0 raw -o - "$file")" || different_data
        [[ "$installed_binary" == "$binary" ]] || different_data
      fi
    }
    case "$action" in
      start|stop|restart) check_service_data "$service" 3 ;;
      uninstall-updater) check_service_data "$update_service" 5 ;;
      install|install-updater|status|verify|uninstall) check_service_data "$service" 3; check_service_data "$update_service" 5 ;;
      uninstall-node) check_service_data "$service" 3 ;;
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
      verify)
        installed=false; running=false
        if [[ -f "$service" ]]; then installed=true; launchctl print "$domain/$label" >/dev/null 2>&1 && running=true; fi
        printf '{"installed":%s,"running":%s}\n' "$installed" "$running"
        ;;
      uninstall|uninstall-updater|uninstall-node)
        if [[ "$action" != uninstall-node ]]; then
          launchctl bootout "$domain/$update_label" >/dev/null 2>&1 || true
          rm -f "$update_service"
        fi
        if [[ "$action" == uninstall || "$action" == uninstall-node ]]; then
          launchctl bootout "$domain/$label" >/dev/null 2>&1 || true
          rm -f "$service"
        fi
        printf 'Removed requested services; node data was kept.\n'
        ;;
    esac
    ;;
  Linux)
    service_dir="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
    if [[ "$service_id" == node ]]; then unit='vibeface'; else unit="vibeface-$service_id"; fi
    service="$service_dir/$unit.service"
    update_service="$service_dir/$unit-update.service"
    update_timer="$service_dir/$unit-update.timer"
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
      if [[ -n "$binary" ]]; then
        [[ "$start" == "ExecStart=\"$(systemd_exec_escape "$binary")\" "* ]] || different_data
      fi
    }
    case "$action" in
      start|stop|restart|verify) check_service_data "$service" "$unit.service" serve ;;
      uninstall-updater) check_service_data "$update_service" "$unit-update.service" 'update apply --automatic' ;;
      uninstall-node) check_service_data "$service" "$unit.service" serve ;;
      install|install-updater|status|uninstall)
        check_service_data "$service" "$unit.service" serve
        check_service_data "$update_service" "$unit-update.service" 'update apply --automatic'
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
Unit=$unit-update.service

[Install]
WantedBy=timers.target
UNIT
          chmod 600 "$temp"
          mv "$temp" "$update_timer"
          systemctl --user daemon-reload
          systemctl --user enable --now "$unit-update.timer"
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
          systemctl --user enable --now "$unit.service"
          printf 'Installed %s\n' "$service"
        fi
        ;;
      start|stop|restart)
        [[ -f "$service" ]] || { echo 'Installed node service is required.' >&2; exit 1; }
        systemctl --user "$action" "$unit.service"
        ;;
      status)
        status_code=0
        systemctl --user --no-pager status "$unit.service" || status_code=$?
        if ! systemctl --user --no-pager status "$unit-update.timer"; then
          if [[ -f "$update_timer" ]]; then
            printf 'Updater is installed but its timer is not active.\n'
          else
            printf 'Updater is not installed.\n'
          fi
        fi
        systemctl --user --no-pager status "$unit-update.service" || true
        exit "$status_code"
        ;;
      verify)
        installed=false; running=false
        if [[ -f "$service" ]]; then installed=true; systemctl --user is-active --quiet "$unit.service" && running=true; fi
        printf '{"installed":%s,"running":%s}\n' "$installed" "$running"
        ;;
      uninstall|uninstall-updater|uninstall-node)
        if [[ "$action" != uninstall-node ]]; then
          systemctl --user disable --now "$unit-update.timer" >/dev/null 2>&1 || true
          systemctl --user stop "$unit-update.service" >/dev/null 2>&1 || true
          rm -f "$update_service" "$update_timer"
        fi
        if [[ "$action" == uninstall || "$action" == uninstall-node ]]; then
          systemctl --user disable --now "$unit.service" >/dev/null 2>&1 || true
          rm -f "$service"
        fi
        systemctl --user daemon-reload
        printf 'Removed requested services; node data was kept.\n'
        ;;
    esac
    ;;
  *) echo 'Supported service platforms are macOS and Linux with systemd.' >&2; exit 1 ;;
esac
