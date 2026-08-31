//! Optional Libretro DAT and thumbnail integration for the ROMX GUI.

use crate::{normalize_crc32, payload_sha256, validate_png_bytes, RomxError};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::Path;
use std::time::Duration;

const DAT_LIMIT: u64 = 64 * 1024 * 1024;
const THUMBNAIL_LIMIT: u64 = 32 * 1024 * 1024;
const THUMBNAIL_BASE: &str = "https://thumbnails.libretro.com";
const CACHE_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibretroLookup {
    pub record: Value,
    pub method: String,
}

#[derive(Debug, Clone)]
enum Node {
    Atom(String),
    List(Vec<Node>),
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}

fn cache_key(url: &str) -> String {
    hex(&payload_sha256(url.as_bytes()))
}

fn cache_bytes(path: &Path, allow_stale: bool, limit: u64) -> Option<Vec<u8>> {
    let bytes = fs::read(path).ok()?;
    if bytes.len() as u64 > limit {
        return None;
    }
    if allow_stale
        || fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .and_then(|modified| modified.elapsed().map_err(std::io::Error::other))
            .is_ok_and(|age| age <= CACHE_MAX_AGE)
    {
        Some(bytes)
    } else {
        None
    }
}

fn fetch_bytes(url: &str, cache_dir: Option<&Path>, limit: u64) -> Result<Vec<u8>, RomxError> {
    let cache_path = cache_dir.map(|directory| directory.join(cache_key(url)));
    let stale_cache = cache_path
        .as_deref()
        .and_then(|path| cache_bytes(path, true, limit));
    if let Some(path) = &cache_path {
        if let Some(bytes) = cache_bytes(path, false, limit) {
            return Ok(bytes);
        }
    }
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(20))
        .timeout_read(Duration::from_secs(20))
        .build();
    let response = agent.get(url).set("User-Agent", "romx-core/0.2.0").call();
    let response = match response {
        Ok(response) => response,
        Err(_error) if stale_cache.is_some() => return Ok(stale_cache.unwrap()),
        Err(error) => {
            return Err(RomxError::Invalid(format!(
                "libretro request failed: {error}"
            )))
        }
    };
    let mut reader = response.into_reader().take(limit.saturating_add(1));
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(RomxError::Invalid(
            "libretro response exceeds the size limit".into(),
        ));
    }
    if let Some(path) = cache_path {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        crate::write_atomic_stream(&path, true, |file| {
            use std::io::Write;
            file.write_all(&bytes)?;
            Ok(())
        })?;
    }
    Ok(bytes)
}

fn tokenize(text: &str) -> Result<Vec<String>, RomxError> {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index].is_ascii_whitespace() {
            index += 1;
            continue;
        }
        if matches!(bytes[index], b'(' | b')') {
            tokens.push((bytes[index] as char).to_string());
            index += 1;
            continue;
        }
        if bytes[index] == b'"' {
            index += 1;
            let mut value = String::new();
            let mut closed = false;
            while index < bytes.len() {
                let byte = bytes[index];
                index += 1;
                match byte {
                    b'"' => {
                        closed = true;
                        break;
                    }
                    b'\\' if index < bytes.len() => {
                        let escaped = bytes[index];
                        index += 1;
                        value.push(match escaped {
                            b'n' => '\n',
                            b'r' => '\r',
                            b't' => '\t',
                            b'"' => '"',
                            b'\\' => '\\',
                            other => other as char,
                        });
                    }
                    other => value.push(other as char),
                }
            }
            if !closed {
                return Err(RomxError::Invalid(
                    "unterminated libretro DAT string".into(),
                ));
            }
            tokens.push(value);
            continue;
        }
        let start = index;
        while index < bytes.len()
            && !bytes[index].is_ascii_whitespace()
            && !matches!(bytes[index], b'(' | b')')
        {
            index += 1;
        }
        tokens.push(String::from_utf8_lossy(&bytes[start..index]).into_owned());
    }
    Ok(tokens)
}

fn parse_node(tokens: &[String], index: &mut usize) -> Result<Node, RomxError> {
    let token = tokens
        .get(*index)
        .ok_or_else(|| RomxError::Invalid("unexpected end of libretro DAT".into()))?;
    if token != "(" {
        *index += 1;
        return Ok(Node::Atom(token.clone()));
    }
    *index += 1;
    let mut values = Vec::new();
    while *index < tokens.len() && tokens[*index] != ")" {
        values.push(parse_node(tokens, index)?);
    }
    if *index >= tokens.len() {
        return Err(RomxError::Invalid("unterminated libretro DAT form".into()));
    }
    *index += 1;
    Ok(Node::List(values))
}

fn parse_forms(text: &str) -> Result<Vec<Node>, RomxError> {
    let tokens = tokenize(text)?;
    let mut forms = Vec::new();
    let mut index = 0;
    while index < tokens.len() {
        // Libretro DAT files use both `(game (...))` and `game (...)` at the
        // top level. The Python reference accepts both spellings; combine the
        // leading atom with its following list before parsing the form.
        if tokens[index] != "(" {
            if index + 1 < tokens.len() && tokens[index + 1] == "(" {
                let name = tokens[index].clone();
                index += 1;
                let Node::List(mut values) = parse_node(&tokens, &mut index)? else {
                    return Err(RomxError::Invalid(
                        "invalid libretro DAT top-level form".into(),
                    ));
                };
                values.insert(0, Node::Atom(name));
                forms.push(Node::List(values));
            } else {
                index += 1;
            }
        } else {
            forms.push(parse_node(&tokens, &mut index)?);
        }
    }
    Ok(forms)
}

fn flatten_form(node: &Node) -> Option<Vec<Node>> {
    let Node::List(values) = node else {
        return None;
    };
    if values.len() == 2 {
        if let Node::List(children) = &values[1] {
            if children.iter().all(|child| matches!(child, Node::List(_))) {
                let mut flattened = Vec::with_capacity(children.len() + 1);
                flattened.push(values[0].clone());
                flattened.extend(children.iter().cloned());
                return Some(flattened);
            }
        }
    }
    Some(values.clone())
}

fn node_key(node: &Node) -> Option<&str> {
    match node {
        Node::List(values) => values.first().and_then(|value| match value {
            Node::Atom(value) => Some(value.as_str()),
            Node::List(_) => None,
        }),
        Node::Atom(_) => None,
    }
}

fn form_fields(node: &Node) -> BTreeMap<String, Node> {
    let Some(form) = flatten_form(node) else {
        return BTreeMap::new();
    };
    let mut result = BTreeMap::new();
    let mut index = match form.first() {
        Some(Node::Atom(name))
            if matches!(
                name.to_ascii_lowercase().as_str(),
                "game" | "rom" | "clrmamepro"
            ) =>
        {
            1
        }
        _ => 0,
    };
    while index < form.len() {
        if let Node::List(values) = &form[index] {
            if let Some(key) = node_key(&form[index]) {
                let value = if values.len() == 2 {
                    values[1].clone()
                } else {
                    Node::List(values[1..].to_vec())
                };
                result.insert(key.to_ascii_lowercase(), value);
            }
            index += 1;
        } else if index + 1 < form.len() {
            if let (Node::Atom(key), value) = (&form[index], &form[index + 1]) {
                result.insert(key.to_ascii_lowercase(), value.clone());
            }
            index += 2;
        } else {
            index += 1;
        }
    }
    result
}

fn node_text(node: &Node) -> Option<&str> {
    match node {
        Node::Atom(value) => Some(value.as_str()),
        Node::List(_) => None,
    }
}

fn libretro_records(text: &str) -> Result<Vec<Map<String, Value>>, RomxError> {
    let mut records = Vec::new();
    for form in parse_forms(text)? {
        let Some(flattened) = flatten_form(&form) else {
            continue;
        };
        if flattened
            .first()
            .and_then(node_text)
            .is_none_or(|value| !value.eq_ignore_ascii_case("game"))
        {
            continue;
        }
        let fields = form_fields(&form);
        let mut record = Map::new();
        for key in [
            "name",
            "description",
            "developer",
            "publisher",
            "genre",
            "region",
            "serial",
        ] {
            if let Some(value) = fields.get(key).and_then(node_text) {
                record.insert(key.into(), Value::String(value.to_owned()));
            }
        }
        if let Some(rom) = fields.get("rom") {
            let rom_fields = form_fields(rom);
            for key in ["serial", "crc", "md5", "sha1"] {
                if let Some(value) = rom_fields.get(key).and_then(node_text) {
                    record.insert(key.into(), Value::String(value.to_owned()));
                }
            }
        }
        records.push(record);
    }
    Ok(records)
}

fn canonical_serial(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_uppercase)
        .collect()
}

fn canonical_name(value: &str) -> String {
    let mut result = String::new();
    let mut pending_space = false;
    for character in value.chars().flat_map(char::to_lowercase) {
        if character.is_alphanumeric() {
            if pending_space && !result.is_empty() {
                result.push(' ');
            }
            pending_space = false;
            result.push(character);
        } else {
            pending_space = true;
        }
    }
    result
}

fn record_matches(
    mode: Option<&str>,
    object: Option<&Map<String, Value>>,
    record: &Map<String, Value>,
) -> Option<&'static str> {
    if let Some(mode) = mode {
        let matched = match mode {
            "serial" => match (
                object
                    .and_then(|value| value.get("serial"))
                    .and_then(Value::as_str),
                record.get("serial").and_then(Value::as_str),
            ) {
                (Some(wanted), Some(actual)) if !wanted.is_empty() => {
                    canonical_serial(wanted) == canonical_serial(actual)
                }
                _ => false,
            },
            "crc32" => match (
                object
                    .and_then(|value| value.get("crc32"))
                    .and_then(Value::as_str),
                record.get("crc").and_then(Value::as_str),
            ) {
                (Some(wanted), Some(actual)) => normalize_crc32(wanted)
                    .ok()
                    .is_some_and(|wanted| wanted.eq_ignore_ascii_case(actual)),
                _ => false,
            },
            _ => false,
        };
        if matched {
            return Some(match mode {
                "serial" => "serial",
                "crc32" => "crc32",
                _ => "name",
            });
        }
    }
    let wanted = object
        .and_then(|value| value.get("name"))
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())?;
    let actual = record.get("name").and_then(Value::as_str)?;
    (canonical_name(wanted) == canonical_name(actual)).then_some("name")
}

/// Return every DAT record matching the supplied identity, falling back to a
/// normalized game name when the platform identity is unavailable or has no
/// match. The caller can present all candidates and let the user choose which
/// metadata fields to write.
pub fn libretro_lookup_candidates(
    platform: &str,
    metadata: &Value,
    dat_url: Option<&str>,
    cache_dir: Option<&Path>,
    payload_format: Option<&str>,
) -> Result<Vec<LibretroLookup>, RomxError> {
    let Some(url) = dat_url.or_else(|| libretro_dat_url(platform, payload_format)) else {
        return Ok(Vec::new());
    };
    let bytes = fetch_bytes(url, cache_dir, DAT_LIMIT)?;
    let text = String::from_utf8(bytes)
        .map_err(|_| RomxError::Invalid("libretro DAT is not UTF-8".into()))?;
    let records = libretro_records(&text)?;
    let mode = libretro_match_mode(platform, payload_format);
    let object = metadata.as_object();
    let identity_available = mode.is_some_and(|mode| {
        object
            .and_then(|value| value.get(mode))
            .and_then(Value::as_str)
            .is_some_and(|value| !value.trim().is_empty())
    });
    let mut identity_matches = Vec::new();
    let mut name_matches = Vec::new();
    for record in records {
        let Some(matched_by) = record_matches(mode, object, &record) else {
            continue;
        };
        let result = LibretroLookup {
            record: Value::Object(record),
            method: matched_by.into(),
        };
        if identity_available && result.method != "name" {
            identity_matches.push(result);
        } else if !identity_available || identity_matches.is_empty() {
            name_matches.push(result);
        }
    }
    if identity_matches.is_empty() {
        Ok(name_matches)
    } else {
        Ok(identity_matches)
    }
}

pub fn libretro_lookup_result(
    platform: &str,
    metadata: &Value,
    dat_url: Option<&str>,
    cache_dir: Option<&Path>,
    payload_format: Option<&str>,
) -> Result<Option<LibretroLookup>, RomxError> {
    Ok(
        libretro_lookup_candidates(platform, metadata, dat_url, cache_dir, payload_format)?
            .into_iter()
            .next(),
    )
}

/// Record-only compatibility helper matching the Python reference API.
pub fn libretro_lookup(
    platform: &str,
    metadata: &Value,
    dat_url: Option<&str>,
    cache_dir: Option<&Path>,
    payload_format: Option<&str>,
) -> Result<Option<Value>, RomxError> {
    Ok(
        libretro_lookup_result(platform, metadata, dat_url, cache_dir, payload_format)?
            .map(|result| result.record),
    )
}

fn safe_thumbnail_filename(name: &str) -> String {
    let value = name
        .chars()
        .map(|character| {
            if "&*/:<>?\\|\"".contains(character) {
                '_'
            } else {
                character
            }
        })
        .collect::<String>();
    let value = value.trim();
    if value.is_empty() {
        "untitled".into()
    } else {
        value.into()
    }
}

fn encode_path_segment(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::new();
    for byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(*byte as char);
        } else {
            encoded.push('%');
            encoded.push(HEX[(byte >> 4) as usize] as char);
            encoded.push(HEX[(byte & 0x0f) as usize] as char);
        }
    }
    encoded
}

pub fn download_libretro_thumbnail(
    platform: &str,
    name: &str,
    cover_set: &str,
    cache_dir: Option<&Path>,
) -> Result<Option<Vec<u8>>, RomxError> {
    let Some(playlist) = libretro_playlist_name(platform) else {
        return Ok(None);
    };
    if name.is_empty() {
        return Ok(None);
    }
    let mut candidates = vec![name.to_owned()];
    if let Some((prefix, _)) = name.split_once(" (") {
        candidates.push(prefix.to_owned());
    }
    for candidate in candidates {
        let filename = format!("{}.png", safe_thumbnail_filename(&candidate));
        let url = format!(
            "{}/{}/{}/{}",
            THUMBNAIL_BASE,
            encode_path_segment(playlist),
            encode_path_segment(cover_set),
            encode_path_segment(&filename)
        );
        let cache_path =
            cache_dir.map(|directory| directory.join(playlist).join(cover_set).join(&filename));
        if let Some(path) = &cache_path {
            if let Ok(bytes) = fs::read(path) {
                if validate_png_bytes(&bytes).is_ok() {
                    return Ok(Some(bytes));
                }
            }
        }
        let Ok(bytes) = fetch_bytes(&url, None, THUMBNAIL_LIMIT) else {
            continue;
        };
        if validate_png_bytes(&bytes).is_err() {
            continue;
        }
        if let Some(path) = cache_path {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            crate::write_atomic_stream(&path, true, |file| {
                use std::io::Write;
                file.write_all(&bytes)?;
                Ok(())
            })?;
        }
        return Ok(Some(bytes));
    }
    Ok(None)
}

pub fn libretro_playlist_name(platform: &str) -> Option<&'static str> {
    match platform {
        "gb" => Some("Nintendo - Game Boy"),
        "gbc" => Some("Nintendo - Game Boy Color"),
        "gba" => Some("Nintendo - Game Boy Advance"),
        "nes" => Some("Nintendo - NES"),
        "fds" => Some("Nintendo - Famicom Disk System"),
        "snes" => Some("Nintendo - Super Nintendo Entertainment System"),
        "nds" => Some("Nintendo - Nintendo DS"),
        "n64" => Some("Nintendo - Nintendo 64"),
        "psp" => Some("Sony - PlayStation Portable"),
        "playstation" | "ps1" => Some("Sony - PlayStation"),
        "ps2" => Some("Sony - PlayStation 2"),
        "genesis" => Some("Sega - Mega Drive - Genesis"),
        "genesis32x" | "32x" => Some("Sega - 32X"),
        "sms" => Some("Sega - Master System - Mark III"),
        "gg" | "gamegear" => Some("Sega - Game Gear"),
        "pce" => Some("NEC - PC Engine - TurboGrafx 16"),
        "pcecd" => Some("NEC - PC Engine CD - TurboGrafx-CD"),
        "segacd" => Some("Sega - Sega CD - Mega CD"),
        "saturn" => Some("Sega - Saturn"),
        "dreamcast" => Some("Sega - Dreamcast"),
        "gamecube" => Some("Nintendo - GameCube"),
        "wii" => Some("Nintendo - Wii"),
        "3ds" => Some("Nintendo - 3DS"),
        _ => None,
    }
}

pub fn libretro_match_mode(platform: &str, payload_format: Option<&str>) -> Option<&'static str> {
    if platform == "psp" && matches!(payload_format, Some("elf" | "prx")) {
        return None;
    }
    if platform == "wii" && payload_format == Some("wad") {
        return Some("crc32");
    }
    match platform {
        "gb" | "gbc" | "gba" | "nes" | "fds" | "snes" | "nds" | "n64" | "genesis"
        | "genesis32x" | "32x" | "sms" | "gg" | "gamegear" | "pce" | "pcecd" | "3ds" => {
            Some("crc32")
        }
        "psp" | "playstation" | "ps1" | "segacd" | "saturn" | "dreamcast" | "gamecube" | "wii"
        | "ps2" => Some("serial"),
        _ => None,
    }
}

pub fn libretro_dat_url(platform: &str, payload_format: Option<&str>) -> Option<&'static str> {
    if platform == "wii" && payload_format == Some("wad") {
        return Some("https://raw.githubusercontent.com/libretro/libretro-database/master/metadat/no-intro/Nintendo%20-%20Wii%20(Digital).dat");
    }
    match platform {
        "psp" => Some("https://raw.githubusercontent.com/libretro/libretro-database/master/metadat/no-intro/Sony%20-%20PlayStation%20Portable.dat"),
        "playstation" | "ps1" => Some("https://raw.githubusercontent.com/libretro/libretro-database/master/metadat/redump/Sony%20-%20PlayStation.dat"),
        "ps2" => Some("https://raw.githubusercontent.com/libretro/libretro-database/master/metadat/redump/Sony%20-%20PlayStation%202.dat"),
        "pcecd" => Some("https://raw.githubusercontent.com/libretro/libretro-database/master/metadat/redump/NEC%20-%20PC%20Engine%20CD%20-%20TurboGrafx-CD.dat"),
        "segacd" => Some("https://raw.githubusercontent.com/libretro/libretro-database/master/metadat/redump/Sega%20-%20Mega-CD%20-%20Sega%20CD.dat"),
        "saturn" => Some("https://raw.githubusercontent.com/libretro/libretro-database/master/metadat/redump/Sega%20-%20Saturn.dat"),
        "dreamcast" => Some("https://raw.githubusercontent.com/libretro/libretro-database/master/metadat/redump/Sega%20-%20Dreamcast.dat"),
        "gamecube" => Some("https://raw.githubusercontent.com/libretro/libretro-database/master/metadat/redump/Nintendo%20-%20GameCube.dat"),
        "wii" => Some("https://raw.githubusercontent.com/libretro/libretro-database/master/metadat/redump/Nintendo%20-%20Wii.dat"),
        "nds" => Some("https://raw.githubusercontent.com/libretro/libretro-database/master/metadat/no-intro/Nintendo%20-%20Nintendo%20DS.dat"),
        "nes" => Some("https://raw.githubusercontent.com/libretro/libretro-database/master/metadat/no-intro/Nintendo%20-%20Nintendo%20Entertainment%20System.dat"),
        "gba" => Some("https://raw.githubusercontent.com/libretro/libretro-database/master/metadat/no-intro/Nintendo%20-%20Game%20Boy%20Advance.dat"),
        "gb" => Some("https://raw.githubusercontent.com/libretro/libretro-database/master/metadat/no-intro/Nintendo%20-%20Game%20Boy.dat"),
        "gbc" => Some("https://raw.githubusercontent.com/libretro/libretro-database/master/metadat/no-intro/Nintendo%20-%20Game%20Boy%20Color.dat"),
        "n64" => Some("https://raw.githubusercontent.com/libretro/libretro-database/master/metadat/no-intro/Nintendo%20-%20Nintendo%2064.dat"),
        "fds" => Some("https://raw.githubusercontent.com/libretro/libretro-database/master/metadat/no-intro/Nintendo%20-%20Family%20Computer%20Disk%20System.dat"),
        "snes" => Some("https://raw.githubusercontent.com/libretro/libretro-database/master/metadat/no-intro/Nintendo%20-%20Super%20Nintendo%20Entertainment%20System.dat"),
        "genesis" => Some("https://raw.githubusercontent.com/libretro/libretro-database/master/metadat/no-intro/Sega%20-%20Mega%20Drive%20-%20Genesis.dat"),
        "genesis32x" | "32x" => Some("https://raw.githubusercontent.com/libretro/libretro-database/master/metadat/no-intro/Sega%20-%2032X.dat"),
        "sms" => Some("https://raw.githubusercontent.com/libretro/libretro-database/master/metadat/no-intro/Sega%20-%20Master%20System%20-%20Mark%20III.dat"),
        "gg" | "gamegear" => Some("https://raw.githubusercontent.com/libretro/libretro-database/master/metadat/no-intro/Sega%20-%20Game%20Gear.dat"),
        "pce" => Some("https://raw.githubusercontent.com/libretro/libretro-database/master/metadat/no-intro/NEC%20-%20PC%20Engine%20-%20TurboGrafx%2016.dat"),
        "3ds" => Some("https://raw.githubusercontent.com/libretro/libretro-database/master/metadat/no-intro/Nintendo%20-%20Nintendo%203DS.dat"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{cache_key, libretro_lookup_candidates, libretro_lookup_result, libretro_records};
    use serde_json::json;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn parses_both_libretro_dat_form_styles() {
        let text = r#"
            clrmamepro ( name "fixture" )
            game (
                name "Fixture Game"
                description "Fixture Game (World)"
                rom ( name "fixture.gba" crc "deadbeef" )
            )
            (game
                (name "Parenthesized Game")
                (rom (name "parent.gba" crc "0123abcd"))
            )
        "#;
        let records = libretro_records(text).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["name"], "Fixture Game");
        assert_eq!(records[0]["crc"], "deadbeef");
        assert_eq!(records[1]["name"], "Parenthesized Game");
        assert_eq!(records[1]["crc"], "0123abcd");
    }

    #[test]
    fn cached_dat_lookup_matches_crc_identity() {
        let cache = tempdir().unwrap();
        let url = "https://example.invalid/fixture.dat";
        let dat = br#"game ( name "Fixture Game" rom ( name "fixture.gba" crc "deadbeef" ) )"#;
        fs::write(cache.path().join(cache_key(url)), dat).unwrap();
        let result = libretro_lookup_result(
            "gba",
            &json!({"crc32": "deadbeef"}),
            Some(url),
            Some(cache.path()),
            Some("gba"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(result.method, "crc32");
        assert_eq!(result.record["name"], "Fixture Game");
    }

    #[test]
    fn cached_dat_lookup_falls_back_to_name() {
        let cache = tempdir().unwrap();
        let url = "https://example.invalid/name.dat";
        let dat = br#"game ( name "Fixture Game" rom ( name "fixture.gba" crc "deadbeef" ) )"#;
        fs::write(cache.path().join(cache_key(url)), dat).unwrap();
        let results = libretro_lookup_candidates(
            "gba",
            &json!({"name": "fixture-game"}),
            Some(url),
            Some(cache.path()),
            Some("gba"),
        )
        .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].method, "name");
        assert_eq!(results[0].record["name"], "Fixture Game");
    }
}
