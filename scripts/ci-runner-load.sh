#!/usr/bin/env bash
# Samples hosted runner load every 10 seconds. Prints only resource counters and process names.
set -u
if [[ "$(uname -s)" == Linux ]]; then
  cgroup_path="$(awk -F: '$1 == "0" { print $3 }' /proc/self/cgroup)"
  cgroup_root="/sys/fs/cgroup$cgroup_path"
  printf 'cpus=%s cgroup=%s\n' "$(getconf _NPROCESSORS_ONLN)" "$cgroup_path"
  while :; do
    awk -v at="$(date -u +%H:%M:%S)" '
      /^(MemTotal|MemAvailable|SwapTotal|SwapFree):/ { value[$1] = $2 }
      END { printf "%s total_mib=%d available_mib=%d swap_total_mib=%d swap_free_mib=%d", at,
        value["MemTotal:"] / 1024, value["MemAvailable:"] / 1024,
        value["SwapTotal:"] / 1024, value["SwapFree:"] / 1024 }' /proc/meminfo
    ps -Ao rss=,comm= | sort -rn | head -5 | awk '{ rss = $1; $1 = ""; printf " |%s rss_mib=%d", $0, rss / 1024 }'
    echo
    if [[ -r /proc/pressure/memory ]]; then
      awk '{ printf "memory_psi_%s %s %s\n", $1, $2, $5 }' /proc/pressure/memory
    fi
    if [[ -r "$cgroup_root/memory.events" ]]; then
      awk '{ printf "cgroup_memory_%s=%s\n", $1, $2 }' "$cgroup_root/memory.events"
    fi
    sleep 10
  done
fi
printf 'cpus=%s memory_mib=%s\n' "$(sysctl -n hw.ncpu)" "$(( $(sysctl -n hw.memsize) / 1048576 ))"
page=$(sysctl -n hw.pagesize)
while :; do
  vm_stat | awk -v page="$page" -v at="$(date -u +%H:%M:%S)" -v load="$(sysctl -n vm.loadavg | awk '{print $2}')" '
    /Pages free/ { free = $3 }
    /Pages occupied by compressor/ { compressed = $5 }
    /^Swapouts/ { swapouts = $2 }
    END { printf "%s load=%s free_mib=%d compressed_mib=%d swapouts=%d", at, load, free * page / 1048576, compressed * page / 1048576, swapouts }'
  ps -Ao pcpu=,ucomm= | sort -rn | head -3 | awk '{ cpu = $1; $1 = ""; printf " |%s %s%%", $0, cpu }'
  echo
  sleep 10
done
