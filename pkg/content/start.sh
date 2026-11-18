#!/bin/sh
# Install the bundled packages (once per container), then run exterior. Its data lives in /lzcapp/var.
if ! command -v sshpass >/dev/null || ! command -v git >/dev/null; then
  apk add -q --no-network --no-cache /lzcapp/pkg/content/apk/*.apk || { echo "cannot install the bundled packages" >&2; exit 1; }
fi
export EXTERIOR_VAR=/lzcapp/var PORT=3000 HOME=/lzcapp/var
exec /lzcapp/pkg/content/exterior
