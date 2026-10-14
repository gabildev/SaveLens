#!/usr/bin/env bash
# Downloads the real-game save corpus used by `tests/real_saves.rs`.
#
# The files are real saves published as test fixtures / examples by other
# open-source projects. They are NOT redistributed in this repository: they are
# fetched from each upstream at a pinned commit so the corpus is reproducible
# and each file stays under its upstream license (see the SOURCES table below
# and tests/real-saves/README.md).
#
# Usage: scripts/fetch-real-saves.sh [DEST]   (default: tests/real-saves/corpus)
set -euo pipefail

DEST="${1:-$(cd "$(dirname "$0")/.." && pwd)/tests/real-saves/corpus}"
mkdir -p "$DEST"

# owner/repo  commit  upstream-path  local-path
SOURCES=$(cat <<'EOF'
trumank/uesave-rs 1917a22ab69e9a4045e613d9d7f39a49fe00bd46 uesave/drg-save-test.sav unreal/deep-rock-galactic.sav
trumank/uesave-rs 1917a22ab69e9a4045e613d9d7f39a49fe00bd46 uesave/examples/space-rig-decorator/PropPack.sav unreal/drg-proppack.sav
scottanderson/gvas ba44fe7201a6cde98c05043b7ca666985d5c8099 resources/test/Delegate.sav unreal/Delegate.sav
scottanderson/gvas ba44fe7201a6cde98c05043b7ca666985d5c8099 resources/test/Options.sav unreal/Options.sav
scottanderson/gvas ba44fe7201a6cde98c05043b7ca666985d5c8099 resources/test/Profile_0.sav unreal/Profile_0.sav
scottanderson/gvas ba44fe7201a6cde98c05043b7ca666985d5c8099 resources/test/SaveSlot_03.sav unreal/SaveSlot_03.sav
scottanderson/gvas ba44fe7201a6cde98c05043b7ca666985d5c8099 resources/test/Slot1.sav unreal/Slot1.sav
scottanderson/gvas ba44fe7201a6cde98c05043b7ca666985d5c8099 resources/test/Slot2.sav unreal/Slot2.sav
scottanderson/gvas ba44fe7201a6cde98c05043b7ca666985d5c8099 resources/test/Slot3.sav unreal/Slot3.sav
scottanderson/gvas ba44fe7201a6cde98c05043b7ca666985d5c8099 resources/test/assert_failed.sav unreal/assert_failed.sav
scottanderson/gvas ba44fe7201a6cde98c05043b7ca666985d5c8099 resources/test/component8.sav unreal/component8.sav
scottanderson/gvas ba44fe7201a6cde98c05043b7ca666985d5c8099 resources/test/enum_array.sav unreal/enum_array.sav
scottanderson/gvas ba44fe7201a6cde98c05043b7ca666985d5c8099 resources/test/features_01.bin unreal/features_01.bin
scottanderson/gvas ba44fe7201a6cde98c05043b7ca666985d5c8099 resources/test/package_version_524.sav unreal/package_version_524.sav
scottanderson/gvas ba44fe7201a6cde98c05043b7ca666985d5c8099 resources/test/package_version_525.sav unreal/package_version_525.sav
scottanderson/gvas ba44fe7201a6cde98c05043b7ca666985d5c8099 resources/test/palworld_zlib.sav unreal/palworld_zlib.sav
scottanderson/gvas ba44fe7201a6cde98c05043b7ca666985d5c8099 resources/test/palworld_zlib_twice.sav unreal/palworld_zlib_twice.sav
scottanderson/gvas ba44fe7201a6cde98c05043b7ca666985d5c8099 resources/test/regression_01.bin unreal/regression_01.bin
scottanderson/gvas ba44fe7201a6cde98c05043b7ca666985d5c8099 resources/test/ro_64bit_fav.sav unreal/ro_64bit_fav.sav
scottanderson/gvas ba44fe7201a6cde98c05043b7ca666985d5c8099 resources/test/string_table_entry.sav unreal/string_table_entry.sav
scottanderson/gvas ba44fe7201a6cde98c05043b7ca666985d5c8099 resources/test/tagcontainer.sav unreal/tagcontainer.sav
scottanderson/gvas ba44fe7201a6cde98c05043b7ca666985d5c8099 resources/test/text_property_noarray.bin unreal/text_property_noarray.bin
scottanderson/gvas ba44fe7201a6cde98c05043b7ca666985d5c8099 resources/test/transform.sav unreal/transform.sav
scottanderson/gvas ba44fe7201a6cde98c05043b7ca666985d5c8099 resources/test/vector2d.sav unreal/vector2d.sav
twoolie/NBT 69d707b4418edaaa39da4bf14066261600ed7dea tests/bigtest.nbt minecraft/bigtest.nbt
twoolie/NBT 69d707b4418edaaa39da4bf14066261600ed7dea tests/world_test/data/scoreboard.dat minecraft/scoreboard.dat
twoolie/NBT 69d707b4418edaaa39da4bf14066261600ed7dea tests/regiontest.mca minecraft/regiontest.mca
twoolie/NBT 69d707b4418edaaa39da4bf14066261600ed7dea tests/world_test/region/r.4.-4.mca minecraft/r.4.-4.mca
owengage/fastnbt 60c7ee0de70c7ce6bfffc553fe372f834c3b32e9 fastnbt/src/test/resources/chunk.nbt minecraft/chunk.nbt
owengage/fastnbt 60c7ee0de70c7ce6bfffc553fe372f834c3b32e9 fastnbt/src/test/resources/chunk1.14.nbt minecraft/chunk1.14.nbt
owengage/fastnbt 60c7ee0de70c7ce6bfffc553fe372f834c3b32e9 fastanvil/resources/21w44a-test1.nbt minecraft/21w44a-test1.nbt
owengage/fastnbt 60c7ee0de70c7ce6bfffc553fe372f834c3b32e9 fastanvil/resources/forge-1.20.1.nbt minecraft/forge-1.20.1.nbt
owengage/fastnbt 60c7ee0de70c7ce6bfffc553fe372f834c3b32e9 fastanvil/resources/1.19.4.mca minecraft/r.1.19.4.mca
Querz/NBT 9133f65fbf8bb5abb49372bb04c3f8305ed70d2c src/test/resources/r.0.0.mca minecraft/r.0.0.mca
rahmatnazali/spireslayer 5d1b396b71a6842d67412b96924b2c6d0f886a20 example/DEFECT.autosave slay-the-spire/DEFECT.autosave
rahmatnazali/spireslayer 5d1b396b71a6842d67412b96924b2c6d0f886a20 source/tests/test.autosave slay-the-spire/test.autosave
N0edL/R.E.P.O-Save-Editor 414137e0ddf116be278c9a583d69ca94a8cb7ea4 test_save_file.es3 repo/test_save_file.es3
fakeyw/EasySave3Tool 291296535df16775b82fd0cbf4146e8454ab3c61 Example.sfh strike-force-heroes/Example.sfh
iaalm/pysfs 771cfa47846f4927ccabfc982e1faa514c8ea05e persistent.sfs kerbal-space-program/persistent.sfs
nictuku/stardew-rocks b4743ab9ec373258dae9f761424b7b953902bcbd assets/saves/Dristan_1458278710 stardew-valley/Dristan_1458278710
nictuku/stardew-rocks b4743ab9ec373258dae9f761424b7b953902bcbd assets/saves/Rey_1458536597 stardew-valley/Rey_1458536597
steffo99/lihzahrd 093c777cf77b3c436dddc0afaabc86cbe5d89b84 tests/lihzahrd/terraria/player/Gwent.plr terraria/Gwent.plr
steffo99/lihzahrd 093c777cf77b3c436dddc0afaabc86cbe5d89b84 tests/lihzahrd/terraria/world/Mount_Holly.wld terraria/Mount_Holly.wld
sonejostudios/RenRPG 559fab71bdcb915ca9b8b7fc44acb02864096c72 game/saves/1-1-LT1.save renpy/renrpg-1-1-LT1.save
farukalim/karrynprison 401bf82d49fb0eff4e3b73f01deada9378f73bcc 1-1-LT1.save renpy/karryn-1-1-LT1.save
truongthang2211/RPGSaveEditor f56df5328cce5d563d8a395b85dc859a901112de src/formats/renpy/__fixtures__/1-1-LT1.save renpy/rpgsaveeditor-1-1-LT1.save
truongthang2211/RPGSaveEditor f56df5328cce5d563d8a395b85dc859a901112de src/formats/renpy/__fixtures__/p2.pickle renpy/p2.pickle
truongthang2211/RPGSaveEditor f56df5328cce5d563d8a395b85dc859a901112de src/formats/renpy/__fixtures__/p5.pickle renpy/p5.pickle
truongthang2211/RPGSaveEditor f56df5328cce5d563d8a395b85dc859a901112de src/formats/sugarcube/__fixtures__/Demo%20Story-20261014-120000.save sugarcube/demo-story.save
ClayAmore/ER-Save-Lib 664ff9e83b7022a6fdc5a8332e8b7b22dce34e0f test/ER0000.sl2 elden-ring/ER0000.sl2
EOF
)

ok=0
fail=0
while read -r repo commit path local; do
  [ -z "$repo" ] && continue
  out="$DEST/$local"
  if [ -s "$out" ]; then
    ok=$((ok + 1))
    continue
  fi
  mkdir -p "$(dirname "$out")"
  url="https://raw.githubusercontent.com/$repo/$commit/$path"
  if curl -sSfL --retry 3 --retry-delay 2 -o "$out.part" "$url"; then
    mv "$out.part" "$out"
    ok=$((ok + 1))
  else
    rm -f "$out.part"
    echo "warning: could not fetch $url" >&2
    fail=$((fail + 1))
  fi
done <<< "$SOURCES"

echo "real-save corpus: $ok file(s) in $DEST, $fail failure(s)"
[ "$fail" -eq 0 ]
