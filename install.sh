#!/bin/sh
# Build and install screenrec to ~/.cargo/bin, and bind the launcher to '-' in
# GNOME (keeps the shortcut you already picked; change it in the launcher's ⚙ settings).
set -e
cargo install --path "$(dirname "$0")"
"$HOME/.cargo/bin/screenrec" install
