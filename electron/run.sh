#!/bin/bash
# Start OxiTide in the Electron shell (the backend starts itself).
cd "$(dirname "$0")" && exec ./node_modules/electron/dist/electron . "$@"
