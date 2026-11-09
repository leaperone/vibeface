<div align="center">

# Vibeface

**Chat with a bot. Run work on your own computers. Keep the results in one conversation.**

[English](README.md) · [简体中文](https://github.com/leaperone/vibeface/blob/main/README.zh-CN.md)

[![CI](https://github.com/leaperone/vibeface/actions/workflows/ci.yml/badge.svg)](https://github.com/leaperone/vibeface/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/leaperone/vibeface?include_prereleases&label=release)](https://github.com/leaperone/vibeface/releases)
[![Platforms](https://img.shields.io/badge/platform-macOS%20arm64%20%7C%20Windows%20x64%20%7C%20Linux%20x64%20%7C%20Linux%20arm64-lightgrey)](packages/docs/install.md)

</div>

> [!NOTE]
> Vibeface is in Nightly. Before the first stable release, protocol, data, and API changes do not preserve backward compatibility. Use the same version on all computers in your network.

## Contents

- [What you can do](#what-you-can-do)
- [Get started](#get-started)
- [Add another device](#add-another-device)
- [Public mesh](#public-mesh)
- [Privacy and permissions](#privacy-and-permissions)
- [Documentation](#documentation)
- [Build from source](#build-from-source)
- [Repository layout](#repository-layout)
- [Contribute](#contribute)
- [License](#license)

## What you can do

- **Ask in chat.** Give a bot a name and a job, then use its conversation to ask for work and review results.
- **Choose a computer.** Run tasks on your own Mac or Linux computer using Codex, Claude Code, pi, opencode, or Cursor.
- **Continue elsewhere.** Approved computers share conversations, task updates, and saved results. Open a synced conversation on another computer and choose to continue there.
- **Check from your phone.** Connect through a browser or the native iOS and Android apps.

Vibeface coordinates the work. You provide the Agent, its login and model access, and the project folder on each computer. Project files, dependencies, and logins do not move with the conversation.

## Get started

You need an Apple Silicon Mac, Linux x64, or Linux ARM64 with glibc, plus a supported Agent set up on that computer.

1. Install Nightly. The installer selects and verifies the complete package for your platform:

   ```sh
   curl -fsSL https://raw.githubusercontent.com/leaperone/vibeface/main/install.sh | bash -s -- --channel nightly --install-root "$HOME/.local/share/vibeface" --bin-dir "$HOME/.local/bin"
   export PATH="$HOME/.local/bin:$PATH"
   vibeface --version
   ```

2. Run the Node, the service that connects this computer to Vibeface:

   ```sh
   vibeface
   ```

3. Open `http://localhost:4790` in the browser on that computer.
4. Create your personal network. Download the recovery file and keep its password somewhere safe.
5. Choose an Agent in the computer's AI settings, complete its login, and check availability. A login from your usual terminal may not apply to Vibeface's private directory.
6. Create a bot and send a message. To work on a project, enable task execution and add the project folder in the AI settings.

The default data directory is `~/.vibeface`. The first run initializes only a missing or empty directory. See the [quick start](packages/docs/quick-start.md) for the full walkthrough.

Add `~/.local/bin` to your shell configuration to use `vibeface` in new terminals. For an existing installation, use `vibeface update` for updates and `vibeface service` to manage the background service. Do not run the installer again. See [installation and updates](packages/docs/install.md) for update configuration and other installation options.

With Node.js 22 or newer, you can also install the CLI from npm with `npm install -g @leaperone/vibeface@nightly`. npm manages updates for that installation.

## Add another device

Create a join link in **Add device** on a device that manages your network. Open it in another browser or the phone app. For another computer, run:

```sh
vibeface join '<join link>' --name '<device name>'
```

Compare the six-digit code on both devices before approving. Join the existing network instead of creating a new one. Read [first-time setup](packages/docs/onboarding.md) for joining and recovery.

## Public mesh

Every Node joins one public mesh by default: it announces a signed record with its device ID, direct addresses, home relay and the roles it offers, and keeps every other Node's record. The official public node is an ordinary Node outside every personal network and the default bootstrap node. It also runs an open relay and browser access, each with small limits. Its device ID, UDP address and relay are pinned in [`config/public-bootstrap.json`](config/public-bootstrap.json):

```text
vibeface://node/<issuer>?addr=8.217.72.118:4792&relay=http://node.vibeface.dev:3340
```

New personal Nodes join the mesh through it, and pick verified open relays from the mesh when no direct path works. `https://app.vibeface.dev` connects straight to its browser access address, `gateway_url` (`https://node.vibeface.dev:4792`), and reaches your Node through it by device ID; you can switch to your own Gateway in the connection settings, and LAN connections remain preferred. It forwards ciphertext and sees addresses, device IDs and traffic volume, not content. Use `--no-mesh` during first initialization to stay off the mesh, or `--bootstrap <node link>` to join through the first Node you know (`vibeface mesh share` prints a running Node's link). See [The mesh](packages/docs/node.md#全网) for configuration, visible metadata, and connection limits. Joining a personal network still requires an invitation and approval.

## Privacy and permissions

- You approve each device that joins your private network. Only add devices you trust.
- Connections are encrypted. A public node forwards encrypted traffic without becoming a member of your network.
- Configured credentials stay on the computer. A secret sent in chat becomes part of the shared conversation.
- Approved computers and your chosen model service process task content. Connection encryption does not encrypt files on disk.
- Agents can run commands and change files. Project folder settings are not an operating system sandbox.
- If a task result is unknown, check the original task before sending it again. Syncing history never starts old tasks.

Read the [privacy and security guide](packages/docs/privacy-security.md) before enabling access to your projects.

## Documentation

Detailed guides are currently in Chinese.

| Guide | Use it to |
| --- | --- |
| [Quick start](packages/docs/quick-start.md) | Install Vibeface and finish the first conversation. |
| [User guide](packages/docs/user-guide.md) | Manage bots, tasks, notifications, and computers. |
| [Product overview](packages/docs/overview.md) | Understand what Vibeface does and its limits. |
| [Agent support](packages/docs/bot-and-execution.md) | Set up coordination and task execution. |
| [History sync](packages/docs/history-sync.md) | See what syncs and how to continue on another computer. |
| [Installation and updates](packages/docs/install.md) | Manage packages, background services, and updates. |
| [Troubleshooting](packages/docs/troubleshooting.md) | Diagnose connection and task problems. |
| [Node reference](packages/docs/node.md) | Configure connections, Agents, and recovery. |
| [CI](packages/docs/ci.md) | Find the checks for a change. |

[File delivery](packages/docs/paid-delivery.md) is experimental and off by default. Paid delivery uses a local test chain, with no real assets.

## Build from source

You need Rust 1.91+, Node.js 22.12+, pnpm 11.9.0, and `wasm-bindgen-cli` 0.2.129.

```sh
rustup target add wasm32-unknown-unknown
bash scripts/build-web-crypto.sh
bash scripts/build-web.sh
bash scripts/package-node.sh
```

The platform package is written to `.artifacts/packages/`. See [source setup](packages/docs/quick-start.md#从源码准备当前版本) for running it and [release instructions](packages/docs/releasing.md) for packaging details.

## Repository layout

```text
.
├── apps/
│   ├── app/         # Web app
│   ├── cli/         # Command line
│   ├── core/        # Node service
│   ├── mobile/      # iOS and Android apps
│   └── www/         # Public website
├── crates/          # Shared Rust libraries
├── packages/
│   ├── client/      # Shared app client
│   ├── docs/        # User and developer guides
│   └── theme/       # Shared styles
├── contracts/       # Experimental file delivery contracts
├── scripts/         # Build, package, and verification tools
├── tests/           # Fixtures and end-to-end tests
└── install.sh       # Package installer
```

## Contribute

Open an [Issue](https://github.com/leaperone/vibeface/issues) with your platform, `vibeface --version`, and the relevant `vibeface status` output. Remove credentials and personal data before sharing logs.

For a pull request, start from `main`, use a separate branch, and run the checks that match your changes. See [CI](packages/docs/ci.md) for the required checks.

## License

This repository does not include a `LICENSE` file yet. Until a license is published, all rights are reserved.
