//! Structural codecs.

pub mod container;
pub mod gvas;
pub mod ini;
pub mod json;
pub mod msgpack;
pub mod nbt;
pub mod nrbf;
pub mod playerprefs;
pub mod plist;
pub mod raw;
pub mod sqlite;
pub mod xml;

use std::sync::Arc;

use crate::document::StructuralCodec;

/// Every built-in structural codec, raw fallback last.
pub fn builtin() -> Vec<Arc<dyn StructuralCodec>> {
    vec![
        Arc::new(json::JsonCodec),
        Arc::new(gvas::GvasCodec),
        Arc::new(nrbf::NrbfCodec),
        Arc::new(playerprefs::PlayerPrefsCodec),
        Arc::new(sqlite::SqliteCodec),
        Arc::new(nbt::NbtCodec),
        Arc::new(msgpack::MsgPackCodec),
        Arc::new(ini::IniCodec),
        Arc::new(xml::XmlCodec),
        Arc::new(plist::PlistCodec),
        Arc::new(raw::RawCodec),
    ]
}
