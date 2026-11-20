#!/bin/sh
# Builds hvac-server on the target host and (re)starts its systemd service.
# usage: deploy/deploy.sh [ssh-host]   (default tuzfal)
set -e
host=${1:-tuzfal}
cd "$(dirname "$0")/.."
tar czf - --exclude=target --exclude=node_modules --exclude=src-tauri/gen \
  Cargo.toml Cargo.lock core server src-tauri ui deploy |
  ssh "$host" 'mkdir -p ~/hvac-control/src && tar xzf - -C ~/hvac-control/src'
ssh "$host" 'set -e
  cd ~/hvac-control/src
  ~/.cargo/bin/cargo build --release -p hvac-server
  install target/release/hvac-server ~/hvac-control/hvac-server.new
  mv ~/hvac-control/hvac-server.new ~/hvac-control/hvac-server
  sudo install -m 644 deploy/hvac-control.service /etc/systemd/system/
  sudo systemctl daemon-reload
  sudo systemctl enable hvac-control
  sudo systemctl restart hvac-control'
