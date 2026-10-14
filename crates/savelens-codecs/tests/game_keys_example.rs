//! The documented repository-level `game_keys.toml` must stay valid.

use savelens_codecs::document::DetectContext;
use savelens_codecs::KeyDatabase;

#[test]
fn example_key_file_parses_and_selects_profiles() {
    let text = include_str!("../../../game_keys.toml");
    let db = KeyDatabase::from_toml(text, "game_keys.toml").expect("example must be valid");
    assert_eq!(db.profiles().len(), 6);
    let ctx = DetectContext { file_name: Some("SaveFile.es3".into()), app_id: Some(1234560) };
    assert_eq!(db.candidates(&ctx)[0].name, "my-unity-game");
    assert_eq!(db.candidates(&DetectContext::for_file("world1.dat"))[0].name, "aes-ctr-fixed");
}
