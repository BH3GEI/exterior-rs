#!/bin/sh
# exterior on Linux: run in a terminal (./run-linux.sh). Prints the web interface's address; Ctrl-C stops exterior.
# PORT (default 3000) or EXTERIOR_ADDR (host:port) change the address; EXTERIOR_VAR the data folder
# (default ${XDG_DATA_HOME:-~/.local/share}/exterior). To keep it running in the background, see exterior.service.
cd "$(dirname "$0")" || exit 1
chmod +x ./exterior 2>/dev/null
# open the page in the desktop's browser when there is one
if [ -n "${DISPLAY:-}${WAYLAND_DISPLAY:-}" ] && command -v xdg-open >/dev/null 2>&1; then
  ( sleep 2; xdg-open "http://127.0.0.1:${PORT:-3000}/" >/dev/null 2>&1 ) &
fi
exec ./exterior
