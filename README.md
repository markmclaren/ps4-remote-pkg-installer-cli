# PS4 Remote PKG Installer CLI

A Windows command-line client for [flatz's PS4 Remote PKG Installer](https://github.com/flatz/ps4_remote_pkg_installer).

Looking for the original Java GUI? It is available in the [PS4 Remote PKG Installer repository](https://github.com/BenjaminFaal/ps4-remote-pkg-installer).

## Build

Install Rust from [rustup.rs](https://rustup.rs/), then build the client from the repository root:

```powershell
cargo build --release
```

## Usage

Pass the console host with `--console`:

```powershell
.\target\release\ps4-remote-pkg-installer-cli.exe --console 192.168.1.50 status
.\target\release\ps4-remote-pkg-installer-cli.exe --console 192.168.1.50 install .\game.pkg
.\target\release\ps4-remote-pkg-installer-cli.exe --console 192.168.1.50 install-url https://example.com/game.pkg
.\target\release\ps4-remote-pkg-installer-cli.exe --console 192.168.1.50 tasks
```

For local installs, the CLI serves the selected files over HTTP while the task is running. Allow inbound connections to the configured file-server port (default `8080`) in Windows Firewall, and use `--advertise-ip` if automatic interface selection does not choose an address reachable by the PS4. Task IDs are stored under `%APPDATA%\ps4-remote-pkg-installer-cli`
