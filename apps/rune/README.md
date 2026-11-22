# T3 Code Rune

The native client pairs with a T3 Code environment that supports orchestration protocol 2. It opens projects and threads, displays conversations, and sends prompts. The environment owns agent execution and workspace state.

## Connect to an environment

From the repository root, run the native client.

```sh
cd apps/rune
cargo run --locked
```

Enter the environment address and a fresh pairing code. Run `t3 pair` on the host and use the value on the `Token` line. To prefill the address, run `cargo run --locked -- --url <address>`.

To start a thread, select a project and choose **New thread**. The thread uses the project folder and the configured default model. Set a project or environment default model in T3 Code settings before starting a thread.

The bundled Inter variable font uses the optical size of T3 Code's Inter web font and is distributed under the [SIL Open Font License](assets/fonts/Inter-LICENSE.txt).

Arguments after a second `--` go to Rune's tooling. This command captures the connection screen.

```sh
cargo run --locked -- -- --snap rune.png
```

## Open a database with the built-in host

Launch the desktop with a separate T3 Code home for a new or copied profile.

```sh
cargo run --locked -- --desktop --base-dir /path/to/t3-home
```

The desktop reads `userdata/statev2.sqlite` inside that directory and initializes an empty database if needed. It also accepts `T3CODE_HOME`. With no override, `--desktop` opens the standard `.t3` directory in the user's home. Use `--local-database` to open a database at a specific path.

```sh
cargo run --locked -- --local-database /path/to/statev2.sqlite
```

The built-in host upgrades databases from migration 30 onward through migration 105. If the profile has a V1 `state.sqlite` but no `statev2.sqlite`, it opens the V1 file read-only and upgrades a consistent copy. Rune restores legacy thread previews at startup and loads the rest of a conversation when you open it.

Threads configured for Codex can send text prompts through the built-in host. Install the Codex CLI or configure a Codex instance in the database directory's `settings.json`. The built-in host launches the configured Codex app server over standard input and output. For a new thread without a configured default model, it can discover the recommended model from the local Codex catalog.

The appearance setting supports System, Light, and Dark. Pass `--electron-profile /path/to/electron-profile` to read the saved Electron appearance when the native profile has no appearance preference. The source profile is opened read-only.

## Build a Linux package

Install the development libraries and graphics driver from [Rune's Linux setup](https://github.com/anomalyco/rune/blob/dd672fc8257a1bfb55d2e8595f585fc411e709df/crates/rune/README.md#linux). The Debian packaging script accepts an x86-64 or ARM64 Linux executable. Run these commands from `apps/rune`.

```sh
cargo build --release --locked
bash packaging/linux/build-deb.sh "${CARGO_TARGET_DIR:-target}/release/t3-rune" 0.1.0 /tmp/t3code_0.1.0.deb
```

The desktop entry starts `/usr/bin/t3code --desktop`.

## Browse a database backup

Create a consistent SQLite backup of `userdata/statev2.sqlite`, then run the history server from `apps/rune`.

```sh
cargo run --locked -- serve --database /path/to/snapshot.sqlite
```

The history server prints a loopback address and a single-use pairing code. Connect to it with the Rune client to browse projects, threads, and committed text messages. Restart the history server to issue a fresh code. Start or continue agent work by connecting to a running T3 Code environment.
