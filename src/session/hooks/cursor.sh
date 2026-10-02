#!/bin/sh
# Managed by radar setup; Cursor sends the actual conversation ID on stdin.
if [ -z "$RADAR_AGENT" ]; then
  printf '{}\n'
  exit 0
fi
exec radar session identify --provider cursor-agent --pid "$PPID"
