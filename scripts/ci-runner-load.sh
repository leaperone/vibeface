#!/usr/bin/env bash
# Samples a macOS runner's load every 10 seconds until the job ends, so a slow simulator can be
# told apart from a starved host. Prints only counts and process names.
set -u
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
