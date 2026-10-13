# Suncord Release

## Highlights

- Brief summary of this release.

## Features

- New or improved capabilities users should notice.

## Bug Fixes

- Issues resolved in this build.

## Installation

### Windows

1. Download the `.msi` installer **or** the NSIS `-setup.exe` from this release.
2. Run the installer and follow the prompts.
3. Launch **Suncord** from the Start menu.

> Prefer MSI for standard installs; use the setup EXE if MSI is blocked in your environment.

### NixOS / Linux (Flake)

From a checkout of this repository:

```bash
# Development shell (gtk3, webkitgtk_4_1, pipewire, openssl, …)
nix develop

# Build and run the packaged binary
nix run
```

Or pin the flake input and run:

```bash
nix run github:Alloyage/Suncord#suncord
```

Ensure your system provides WebKitGTK 4.1 and PipeWire as linked by `flake.nix`.
