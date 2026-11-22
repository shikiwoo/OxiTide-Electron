#!/bin/bash
# Stop the Electron shell (and with it the backend). Matches by executable, so
# it never hits shells whose command line merely mentions the path.
E="$(cd "$(dirname "$0")" && pwd)/node_modules/electron/dist/electron"
B="$(cd "$(dirname "$0")/.." && pwd)/target/release/oxitide-app"
for p in /proc/[0-9]*; do
  exe=$(readlink "$p/exe" 2>/dev/null) || continue
  [ "$exe" = "$E" ] && kill "${p#/proc/}" 2>/dev/null
done
sleep 2
for p in /proc/[0-9]*; do
  exe=$(readlink "$p/exe" 2>/dev/null) || continue
  [ "$exe" = "$B" ] && kill "${p#/proc/}" 2>/dev/null
done
exit 0
