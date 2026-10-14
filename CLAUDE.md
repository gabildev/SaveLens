# SaveLens — Engineering Specification & Ground Rules

SaveLens is a semantic versioning engine (Git-like UX, **no Git underneath**), a differential
on-disk variable scanner ("Cheat Engine without touching RAM"), a live branch manager and a
disk-trainer for video-game save files on Linux (native + Steam Proton/Wine) and Windows.

This file is the contract for anyone (human or agent) working on the code base.

## Non-negotiable coding rules

* No placeholders, empty mocks, `todo!()`, `unimplemented!()`, half-finished functions or
  `// TODO` comments. If something is genuinely unsupported (e.g. a record type that never
  appears in save games), return a typed error that explains it — never silently ignore it.
* Library crates use `thiserror` error enums; binaries / front-ends use `anyhow`.
* No `unwrap()`/`expect()` on data coming from disk, user input or the network. They are only
  allowed on invariants that are provably upheld by the surrounding code (and in tests).
* Every codec and every envelope layer ships with integration tests proving the
  **lossless bit-for-bit round trip** and an **edit + re-sign/re-compress** test.
* `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings` must be
  green before a phase is considered done.

## Zero-Corruption Guarantee (security invariants)

1. **Total isolation of the game directory.** SaveLens never creates folders (`.savelens`),
   `.bak` files or temporary files inside a game's save directory. All state lives under
   `dirs::data_local_dir()/savelens` (overridable with `SAVELENS_HOME`). Atomic writes are
   staged in `<data>/staging` and moved with `rename(2)`; if the save directory lives on a
   different filesystem, the file is overwritten in place (`write + fsync`) and verified.
2. **Debounce & lock check.** The watcher only reads a file after the writer closed it
   (`IN_CLOSE_WRITE` / end of activity + configurable quiet period), after the size/mtime are
   stable, and after verifying that no process holds it open for writing
   (`/proc/*/fdinfo` on Linux, exclusive share-mode open on Windows).
3. **Immutable pre-mutation shadow backup.** Before *any* mutation of a file in a game
   directory (variable edit, checkout, cherry-pick, trainer injection, panic restore) the
   engine stores the current on-disk bytes in the CAS under the protected tag
   `safety/pre-mutation-<timestamp>` and re-reads the stored blob to verify its `blake3` hash.
   If verification fails the mutation is aborted.
4. **Safe edit gate.** Before structural edits are allowed on a freshly detected file, the
   engine runs `serialize(deserialize(bytes)) == bytes` in memory. If a single bit differs, only
   "Full backup/restore" and "Raw in-place edit with checksum recalculation" modes are offered.
5. **Panic rollback.** `Panic Restore` (TUI, GUI, overlay, CLI and global hotkey
   `Ctrl+Shift+F12`) reverts the most recently mutated file to its `safety/pre-mutation`
   bytes, bit for bit, and verifies the resulting hash.

## Workspace layout

| Crate | Responsibility |
|-------|----------------|
| `savelens-core` | `SaveAst` (`Node`, `NodePath`, `FormatEnvelope`), Merkle hashing (`blake3`), semantic diff, scanner engine (AST + raw bytes), cherry-pick engine. Pure logic, no I/O. |
| `savelens-codecs` | Reversible `EnvelopePipeline`: crypto (AES-CBC/ECB/CTR, XOR, ES3 PBKDF2+AES, `game_keys.toml`, entropy) → checksum hunter → compression (zlib/gzip/deflate/LZ4/zstd/base64, LZString, Palworld PlZ) → structural codecs (JSON + nested JSON, UE GVAS, .NET NRBF, Unity PlayerPrefs, SQLite, NBT, MessagePack, INI/CFG, KSP ConfigNode, XML, XML plist, BND4, Anvil, ZIP, Python pickle, raw binary). Containers open their entries through the full pipeline (`codecs/container.rs`). |
| `savelens-cas` | Content-addressable store: FastCDC chunking + zstd chunk files + SQLite metadata (games, branches, commits, tags, changelogs, cheat table, safety backups). Mutation protocol + panic restore. |
| `savelens-discovery` | Steam library/AppManifest parsing (native, Flatpak, Snap, Windows), Proton prefix mapping, embedded save-path rules, engine heuristics (Unity `app.info`, Unreal, Godot, …), live process file hunter. |
| `savelens-watcher` | `Engine` facade used by every front-end, background daemon (debounce, lock check, auto-commit, freeze-on-save trainer), atomic hot-swap, global hotkeys. |
| `savelens-tui` | `ratatui` + `crossterm` terminal UI (Games, Timeline, Scanner, AST explorer / cherry-pick). |
| `savelens-gui` | `eframe`/`egui` desktop app (sidebar, commit graph, Scanner Studio, tree diff & cherry-picker, integrity bar, panic button). |
| `savelens-overlay` | Translucent always-on-top mod menu: live cheat table, freeze-on-save rules, hotkeys. |
| `savelens-cli` | `savelens` binary (`clap`): `tui`, `gui`, `overlay`, `watch`, `discover`, `track`, `snapshot`, `log`, `diff`, `scan`, `edit`, `cherry-pick`, `checkout`, `branch`, `panic-restore`, … |

## Key design decisions

* **Document model.** Structural codecs parse into a codec-native document that retains every
  layout detail (whitespace, key order, encodings, property GUIDs, varint widths, …). The
  `SaveAst` is a *projection* of that document; edits are applied back with
  `Document::apply_ast`, then `Document::encode` re-serialises. Structural codecs never
  short-circuit to the original bytes: the round-trip gate always exercises the real writer.
* **Envelope layers** (crypto/checksum/compression/base64) are reversible transforms that
  record their exact parameters (IV, level, size prefix, header bytes, checksum rule). A layer
  re-emits its original outer bytes only when its inner payload is byte-identical, and reports
  whether re-encoding of modified data is bit-exact (`LayerReport::exact_reencode`).
* **Paths** use the syntax `Player.Inventory[3].Name`; keys containing `.`, `[`, `]`, `"` or
  whitespace are quoted: `Root["key.with.dots"]`.
* **SQLite is the IPC bus** between the daemon, the overlay and the UIs (WAL mode, busy timeout).
* The scanner works on named AST leaves, numeric strings and every `Bytes`/`RawRegion` node
  (aligned or unaligned, little/big endian).

## Commands

```bash
cargo check --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo run -p savelens-cli -- --help
```

Set `SAVELENS_HOME=/some/dir` to sandbox the data directory (tests always do this).

Real-game corpus: `scripts/fetch-real-saves.sh`, then `cargo test -p savelens-codecs --test real_saves`.
New real files go into the fetch script (pinned commit) and `tests/real-saves/manifest.toml`.
