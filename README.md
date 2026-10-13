<a id="readme-top"></a>

<p align="center">
  <img src="https://img.shields.io/badge/License-MIT-0078D4?style=for-the-badge&logo=github&logoColor=white" alt="License MIT" />
  <img src="https://img.shields.io/badge/Tauri-2.0-FFC107?style=for-the-badge&logo=tauri&logoColor=black" alt="Tauri 2.0" />
  <img src="https://img.shields.io/badge/Rust-Backend-000000?style=for-the-badge&logo=rust&logoColor=white" alt="Rust" />
  <img src="https://img.shields.io/badge/React-18-61DAFB?style=for-the-badge&logo=react&logoColor=black" alt="React 18" />
  <img src="https://img.shields.io/badge/NixOS-Supported-5277C3?style=for-the-badge&logo=nixos&logoColor=white" alt="NixOS Supported" />
</p>

<h1 align="center">Suncord</h1>
<h3 align="center">High-Performance Native Discord Client</h3>

<div align="center">
  <p align="center">
    Ultra-responsive, lightweight desktop client for Discord built with Tauri 2.0, Rust, and React.<br/>
    Designed for zero-latency UI rendering, instant channel navigation, and low overhead.
  </p>
  <a href="https://github.com/Alloyage/Suncord/releases"><strong>Download Latest Release</strong></a> (NOT YET ADDED)
</div>

<br/>

---

## Overview

Suncord offloads state caching, gateway protocol handling, and media processing to a native Rust backend while using a clean React UI projection. switching channels updates instantly from local memory without waiting on network Round-Trip Time.

> [!WARNING]
> Suncord is an unofficial client. Using custom clients may violate Discord's Terms of Service and could lead to account restrictions.

<br/>

<table>
  <tbody>
    <tr>
      <td><strong>Core Stack</strong></td>
      <td>Tauri 2.0, Rust, React, Tailwind CSS, Zustand</td>
    </tr>
    <tr>
      <td><strong>Platforms</strong></td>
      <td>Windows 10/11, Linux (NixOS Flake included)</td>
    </tr>
    <tr>
      <td><strong>Architecture</strong></td>
      <td>Rust Gateway & Cache, Native Audio Pipeline, WGC Capture</td>
    </tr>
  </tbody>
</table>

<br/>

---

## Downloads & Installation

### Option 1: Direct Download (Windows) (NOT YET IMPLEMENTED)

Download the pre-compiled installer from the [Releases](https://github.com/Alloyage/Suncord/releases) page:

- **Windows Installer (`.exe` / `.msi`)**: Run the setup wizard to install Suncord directly.

<br/>

### Option 2: NixOS / Linux

Run Suncord directly using the repository flake:

```bash
nix run github:Alloyage/Suncord
