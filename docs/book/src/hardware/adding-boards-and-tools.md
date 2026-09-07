# Adding Boards and Tools: ZeroClaw Hardware Guide

This guide explains how to add new hardware boards and custom tools to ZeroClaw.

## Quick Start: Add a Board via CLI

<div class="os-tabs-src">

#### sh

```sh
# Add a board
zeroclaw peripheral add nucleo-f401re /dev/ttyACM0
zeroclaw peripheral add arduino-uno /dev/cu.usbmodem12345
zeroclaw peripheral add rpi-gpio native   # for Raspberry Pi GPIO (Linux)

# Restart daemon to apply
zeroclaw daemon --host 127.0.0.1 --port 42617
```

</div>

## Supported Boards

Boards are identified by USB VID/PID. The canonical registry:

{{#include ../_snippets/hardware-boards.md}}

The `board` value in `[[peripherals.boards]]` is matched against these registry
names (and a few transport-specific aliases such as `arduino-uno-q` / `rpi-gpio`
handled in `peripherals/mod.rs`).

## Manual Config

Boards are configured under `peripherals` and `peripherals.boards`. See the [Config reference](../reference/config.md) for the full field index, including `datasheet_dir` (RAG source).

## Adding a Datasheet (RAG)

Place `.md` or `.txt` files in `docs/datasheets/` (or your `datasheet_dir`). Name files by board: `nucleo-f401re.md`, `arduino-uno.md`. Files are extracted, chunked, and retrieved into the agent's context for board-specific questions. PDF datasheet parsing is no longer supported as of #8519; convert PDFs to `.md` or `.txt` before indexing.

### Pin Aliases (Recommended)

Add a `## Pin Aliases` section so the agent can map "red led" → pin 13:

```text
# My Board

## Pin Aliases

| alias       | pin |
|-------------|-----|
| red_led     | 13  |
| builtin_led | 13  |
| user_led    | 5   |
```

Or use key-value format:

```text
## Pin Aliases
red_led: 13
builtin_led: 13
```

## Adding a New Board Type

1. **Create a datasheet**: `docs/datasheets/my-board.md` with pin aliases and GPIO info.
2. **Add to config**: `zeroclaw peripheral add my-board /dev/ttyUSB0`
3. **Implement a peripheral** (optional): For custom protocols, implement the `Peripheral` trait in `crates/zeroclaw-hardware/src/peripherals/` and register in `create_peripheral_tools`.

See [`docs/hardware/hardware-peripherals-design.md`](../hardware/hardware-peripherals-design.md) for the full design.

## Adding a Custom Tool

1. Implement the `Tool` trait in `crates/zeroclaw-tools/src/`.
2. Register in `create_peripheral_tools` (for hardware tools) or the agent tool registry.
3. Add a tool description to the agent's `tool_descs` in `crates/zeroclaw-runtime/src/agent/loop_.rs`.

## Subprocess tool lifecycle

The manifest loader in `zeroclaw-hardware` runs a `tool.toml` binary through
`SubprocessTool`. It sends one JSON argument line and expects one JSON
`ToolResult` line on stdout. Both protocol envelopes are limited to 1 MiB,
including the newline. Oversized requests are refused before the child starts;
oversized, malformed or empty responses fail explicitly.

The active turn's parent deadline covers admission, stdin writes, response reads,
exit and cleanup. An expired parent prevents spawning and returns the existing
typed deadline error. Without an earlier parent deadline, exchange work has a
10-second ceiling. Natural exit gets at most 5 seconds within that exchange;
error cleanup gets at most 5 additional seconds for kill/reap. Cancellation drops
the owned kill-on-drop child and all pipe readers. No reader or cleanup task is
detached from the invocation.

Stderr is drained concurrently with stdin/stdout work, retaining at most a
512-byte diagnostic prefix. A received `ToolResult` needs a successful process
exit before success is reported. A later exit failure or local timeout changes
success to false while retaining the received text and structured output in the
runner's return value. This source-supplied output is not a trusted effect receipt.
The generic runtime failure/history projection and durable effect-receipt
preservation remain separate unfinished work.

Synthetic process tests cover blocked stdin, stderr backpressure, oversized
responses, parent expiry after one effect, direct-child reaping and exit failure.
The runner does not contain detached descendants or reverse external effects.
A timeout, killed child or malformed response is never proof of non-delivery;
no automatic retry is performed. Loader path checks and extension attribution
continue to apply.

## CLI Reference

See the [generated CLI reference](../reference/cli.md) for `zeroclaw peripheral` and `zeroclaw hardware` subcommands.

## Troubleshooting

- **Serial port not found**: On macOS use `/dev/cu.usbmodem*`; on Linux use `/dev/ttyACM0` or `/dev/ttyUSB0`.
- **Build with hardware**: `cargo build --features hardware`
- **Probe-rs for Nucleo**: `cargo build --features hardware,probe`
