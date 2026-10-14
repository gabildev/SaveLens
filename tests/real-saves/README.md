# Real-game save corpus

SaveLens is tested against real save files from real games, not only against
synthetic fixtures. The files are published by other open-source projects
(save editors, parsers, format libraries) as test fixtures or examples.

**They are not redistributed in this repository.** `scripts/fetch-real-saves.sh`
downloads each file from its upstream repository at a pinned commit into
`tests/real-saves/corpus/` (git-ignored). Every file stays under the license of
the project it comes from.

```bash
scripts/fetch-real-saves.sh
cargo test -p savelens-codecs --test real_saves -- --nocapture
```

`manifest.toml` lists what every file must open as (codec, envelope layers),
whether its structural round trip must be bit-exact (Safe Edit Gate), and an
optional edit probe. For each file the harness checks that:

1. the pipeline detects the expected codec and layers,
2. the Safe Edit Gate result matches the manifest,
3. `encode()` of the unedited save returns the original bytes exactly,
4. the edit probe (if any) passes `encode_verified`, re-opens with the same
   codec, contains the new value and changes no other node.

Without the corpus the test is skipped; set `SAVELENS_REQUIRE_REAL_SAVES=1` to
make a missing corpus a failure (the CI `real-saves` job does this).

## Sources

| Game / engine | Format exercised | Upstream (pinned in the fetch script) |
|---|---|---|
| Deep Rock Galactic | UE4 GVAS | [trumank/uesave-rs](https://github.com/trumank/uesave-rs) |
| Palworld, assorted UE4/UE5 games | GVAS, Palworld `PlZ` zlib / zlib×2 wrapper, Text/Delegate/GameplayTag properties | [scottanderson/gvas](https://github.com/scottanderson/gvas) |
| Minecraft (Java) | gzip NBT, uncompressed chunk NBT, Anvil `.mca` regions | [twoolie/NBT](https://github.com/twoolie/NBT), [owengage/fastnbt](https://github.com/owengage/fastnbt), [Querz/NBT](https://github.com/Querz/NBT) |
| Slay the Spire | Base64 → XOR(`key`) → JSON | [rahmatnazali/spireslayer](https://github.com/rahmatnazali/spireslayer) |
| R.E.P.O. | Easy Save 3 (PBKDF2 + AES-128-CBC) → JSON | [N0edL/R.E.P.O-Save-Editor](https://github.com/N0edL/R.E.P.O-Save-Editor) |
| Strike Force Heroes | Easy Save 3 → ES3 JSON dialect with unquoted keys | [fakeyw/EasySave3Tool](https://github.com/fakeyw/EasySave3Tool) |
| Kerbal Space Program | ConfigNode `.sfs` | [iaalm/pysfs](https://github.com/iaalm/pysfs) |
| Stardew Valley | .NET `XmlSerializer` XML | [nictuku/stardew-rocks](https://github.com/nictuku/stardew-rocks) |
| Terraria | `.plr` AES-128-CBC (key/IV `h3y_gUyZ` UTF-16LE), `.wld` binary | [steffo99/lihzahrd](https://github.com/steffo99/lihzahrd) |
| Ren'Py | ZIP `.save` with pickled `log`, raw pickle protocols 2 and 5 | [sonejostudios/RenRPG](https://github.com/sonejostudios/RenRPG), [farukalim/karrynprison](https://github.com/farukalim/karrynprison), [truongthang2211/RPGSaveEditor](https://github.com/truongthang2211/RPGSaveEditor) |
| SugarCube (Twine) | LZString `compressToBase64` → JSON | [truongthang2211/RPGSaveEditor](https://github.com/truongthang2211/RPGSaveEditor) |
| Elden Ring | FromSoftware `BND4` `.sl2` container | [ClayAmore/ER-Save-Lib](https://github.com/ClayAmore/ER-Save-Lib) |
