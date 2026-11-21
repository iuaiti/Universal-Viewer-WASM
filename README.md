# Universal Viewer (Horizon WebAssembly Mod)

See inside **shulker boxes**, **bundles**, and **maps** without opening them — running sandboxed as `mod.wasm` on Horizon (`api = "0.2"`).

No native binary, native mixins, or native-code trust prompt required on the player's machine.

## Credits & Attribution

- **Original creator:** [Aiken1609](https://github.com/Aiken1609)
- **Original project:** [Aiken1609/uni-viewer](https://github.com/Aiken1609/uni-viewer)
- **WASM fork and maintainer:** iuaiti
- **Fork repository:** [iuaiti/Universal-Viewer-WASM](https://github.com/iuaiti/Universal-Viewer-WASM)

This project is a WebAssembly port of the original Universal Viewer for Horizon. It adapts the original functionality to Horizon's WASM sandbox and removes native-only dependencies.

## Features (WASM Edition)

- **Shulker Box & Bundle Contents:** Hold **Shift** while hovering to view the full vanilla-style slot grid tinted to the shulker or bundle's dye colour (16 dye colours + plain). Works on every server, including vanilla servers.
- **Ctrl-Lock Panel:** Hold (or toggle) **Ctrl** while the contents grid is open to lock it in place and inspect individual items inside, including names, enchantments, lore, and nested containers.
- **Coloured Placed Shulker Box UI:** Opening a placed shulker box tints its inventory screen to match its dye colour.
- **Map Previews:** Display map pictures in inventory slots and tooltips, including map metadata.

## WebAssembly Compatibility & Limitations

The original project uses a native `vanilla_map` mixin to intercept vanilla multiplayer `MapItemData` packets.

Since Horizon WebAssembly mods do not support `[[native.mixin]]`, this mixin has been removed in the WASM port.

The replacement uses an optional server-side payload system:

- `universal_viewer:map_request`
- `universal_viewer:map_data`

A built-in standard Minecraft map colour palette is used to render map pictures.

### Feature availability

| Feature | Singleplayer | Vanilla server | Horizon server with mod |
|---|---|---|---|
| Shulker preview | Yes | Yes | Yes |
| Bundle preview | Yes | Yes | Yes |
| Ctrl-lock panel | Yes | Yes | Yes |
| Coloured shulker UI | Yes | Yes | Yes |
| Map previews | Yes | Limited | Yes |

**Map preview limitations:**

- In singleplayer, map pictures can be accessed locally.
- On Horizon multiplayer servers with Universal Viewer installed, the optional payload system provides map data.
- On vanilla multiplayer servers without the server-side mod, map pictures are unavailable because the native packet interception mixin is not supported in WASM.
- Shulker boxes, bundles, and coloured shulker UI continue working normally on vanilla servers.

## Build (WebAssembly)

Requires Rust (`rustup`, Rust 1.95+) and `cargo-horizon-mod` for development and building.

Install the build tool:

```bash
cargo install --locked --git https://github.com/horizon-rust/horizon cargo-horizon-mod
```

Optionally update the SDK pin in `Cargo.toml` to the latest Horizon Nightly:

```bash
cargo horizon-mod update
```

Build the WASM module and package the `.hmod`:

```bash
cargo horizon-mod build --wasm
```

The build produces:

```text
mod.wasm
target/horizon-mod/universal_viewer.hmod
```

Do not edit the `[wasm]` table in `horizon-mod.toml` manually. The build command generates it automatically.

**Note:** Developers need Rust to compile the mod. Players do not need Rust, Cargo, or a native compiler to install or run it.

## Install

1. Copy `target/horizon-mod/universal_viewer.hmod` into your game's `horizon-mods/` directory.
2. Add `mod.universal_viewer:true` to `horizon-mods.txt` (beside `options.txt`).
3. Launch Horizon.
4. Configure the mod under **Options > Mods > Universal Viewer**.

## License

MIT. See [LICENSE](LICENSE).

This project retains attribution to the original Universal Viewer creator, Aiken1609, and its upstream repository.