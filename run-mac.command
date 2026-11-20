#!/bin/sh
# exterior on macOS: double-click (or run in Terminal). Opens the web interface; close this window to stop exterior.
# PORT (default 3000) or EXTERIOR_ADDR (host:port) change the address; EXTERIOR_VAR the data folder.
cd "$(dirname "$0")" || exit 1
# a downloaded program is quarantined by macOS; this one is started on purpose
xattr -d com.apple.quarantine ./exterior 2>/dev/null
chmod +x ./exterior
( sleep 2; open "http://127.0.0.1:${PORT:-3000}/" ) &
exec ./exterior
