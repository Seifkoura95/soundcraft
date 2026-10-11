//! Single-file sessions for hosts without a file system (the browser). A bundle is a ZIP (stored,
//! not compressed) holding `<Name>/<Name>.scraft` and the audio it plays as 32-bit float WAVs in
//! `<Name>/Audio Files/`, so unzipping one gives a folder the desktop app opens as it is.
//!
//! [`open_bytes`] also takes a bare `.scraft` file (its audio is then reported missing) and
//! bundles whose entries were deflated by another zip tool.

use crate::io::sanitize_name;
use crate::{Engine, EngineError, Result};
use soundcraft_audio_io::{EncodeOptions, FileFormat};
use soundcraft_model::{SESSION_EXTENSION, Session, SourceAudio, TrackId, TrackKind};
use std::collections::BTreeMap;
use std::sync::Arc;

/// The bundle's file extension (after the session name).
pub const EXTENSION: &str = "scraft.zip";

/// Bundles over 4 GiB would need ZIP64, which this writer does not produce.
const MAX_ZIP: usize = u32::MAX as usize;

/// The session name a bundle or session file name stands for (`Song.scraft.zip` → `Song`).
pub fn stem(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let base = base.strip_suffix(".zip").unwrap_or(base);
    let base = base.strip_suffix(&format!(".{SESSION_EXTENSION}")).unwrap_or(base);
    let base = base.trim();
    if base.is_empty() { "Untitled".into() } else { base.to_string() }
}

/// The file name a bundle of the current session gets.
pub fn file_name(e: &Engine) -> String {
    format!("{}.{EXTENSION}", sanitize_name(&e.session().name))
}

/// Save the open session as a bundle called `name` (a file name; the session is renamed after its
/// stem, as a save to disk would). The document is marked clean.
pub fn save(e: &mut Engine, name: &str) -> Result<Vec<u8>> {
    let mut s = e.session().clone();
    s.name = stem(name);
    let bytes = write(&mut s)?;
    *e.session_mut() = s;
    e.mark_clean();
    Ok(bytes)
}

/// A bundle of `session` (consumed: its media paths are rewritten to the bundle's).
pub fn of(mut session: Session) -> Result<Vec<u8>> {
    write(&mut session)
}

/// `session` reduced to the tracks in `keep` (masters stay), for File › Export › Selected Tracks
/// as New Session.
pub fn tracks_only(session: &Session, keep: &[TrackId]) -> Session {
    let mut s = session.clone();
    s.tracks.retain(|t| keep.contains(&t.id) || t.kind == TrackKind::Master);
    s
}

fn write(s: &mut Session) -> Result<Vec<u8>> {
    let folder = sanitize_name(&s.name);
    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    let mut targets: BTreeMap<u64, String> = BTreeMap::new();
    for src in &s.sources {
        let Some(audio) = s.pool.get(src.id) else { continue };
        let target = format!("Audio Files/{}_{}.wav", sanitize_name(&src.name), src.id.0);
        let opts = EncodeOptions { format: FileFormat::Wav, bit_depth: soundcraft_audio_io::BitDepth::Float32, dither: false, bwf: None };
        let bytes = soundcraft_audio_io::encode(&audio.buffer, &opts).map_err(|err| EngineError::Io(err.to_string()))?;
        entries.push((format!("{folder}/{target}"), bytes));
        targets.insert(src.id.0, target);
    }
    for src in &mut s.sources {
        if let Some(t) = targets.get(&src.id.0) {
            src.path.clone_from(t);
            src.unsaved = false;
            src.format = FileFormat::Wav;
        }
    }
    let text = s.to_json().map_err(|err| EngineError::Io(err.to_string()))?;
    entries.insert(0, (format!("{folder}/{folder}.{SESSION_EXTENSION}"), text.into_bytes()));
    zip::write(&entries)
}

/// Open a bundle, or a bare `.scraft` file, from `bytes`. Audio the bundle lacks is reported
/// (`missing media: …`) rather than fatal, as when opening from disk.
pub fn open_bytes(e: &mut Engine, name: &str, bytes: &[u8]) -> Result<Vec<String>> {
    let (text, files, dir) = if zip::is_zip(bytes) {
        let files = zip::read(bytes)?;
        let session_entry = files
            .keys()
            .filter(|k| k.ends_with(&format!(".{SESSION_EXTENSION}")) && !k.rsplit('/').next().unwrap_or("").starts_with('.'))
            .min_by_key(|k| (k.matches('/').count(), k.len()))
            .cloned()
            .ok_or_else(|| EngineError::Io(format!("{name}: no .{SESSION_EXTENSION} file in the bundle")))?;
        let dir = session_entry.rsplit_once('/').map(|(d, _)| format!("{d}/")).unwrap_or_default();
        let text = files.get(&session_entry).map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default();
        (text, files, dir)
    } else {
        (String::from_utf8_lossy(bytes).into_owned(), BTreeMap::new(), String::new())
    };
    let mut s = Session::from_json(&text).map_err(|err| EngineError::Io(format!("{name}: {err}")))?;
    let mut missing = Vec::new();
    for src in s.sources.clone() {
        let rel = src.path.trim_start_matches("./");
        let key = format!("{dir}{rel}");
        let Some(data) = files.get(&key).or_else(|| files.get(rel)) else {
            missing.push(src.path.clone());
            continue;
        };
        let ext = rel.rsplit('.').next();
        match soundcraft_audio_io::decode(data, ext) {
            Ok((_, mut buf)) => {
                if buf.sample_rate != s.sample_rate.hz() && buf.sample_rate > 0 {
                    buf.channels = soundcraft_dsp::offline::resample(&buf.channels, buf.sample_rate, s.sample_rate.hz());
                    buf.sample_rate = s.sample_rate.hz();
                }
                s.pool.insert(src.id, Arc::new(SourceAudio::new(buf)));
            }
            Err(err) => missing.push(format!("{}: {err}", src.path)),
        }
    }
    if s.name.trim().is_empty() {
        s.name = stem(name);
    }
    e.replace_session(s);
    e.path = None;
    for m in &missing {
        e.message(format!("missing media: {m}"));
    }
    Ok(missing)
}

/// A minimal ZIP container: stored entries out, stored or deflated entries in.
mod zip {
    use super::{EngineError, MAX_ZIP, Result};
    use std::collections::BTreeMap;

    const LOCAL: u32 = 0x0403_4b50;
    const CENTRAL: u32 = 0x0201_4b50;
    const END: u32 = 0x0605_4b50;
    /// 1980-01-01 00:00, the earliest MS-DOS date (bundles are reproducible).
    const DOS_DATE: u16 = 0x0021;

    fn u16le(out: &mut Vec<u8>, v: u16) {
        out.extend_from_slice(&v.to_le_bytes());
    }

    fn u32le(out: &mut Vec<u8>, v: u32) {
        out.extend_from_slice(&v.to_le_bytes());
    }

    fn len32(n: usize) -> Result<u32> {
        u32::try_from(n).map_err(|_| EngineError::Io("the bundle would exceed 4 GiB".into()))
    }

    pub fn write(entries: &[(String, Vec<u8>)]) -> Result<Vec<u8>> {
        let total: usize = entries.iter().map(|(n, d)| n.len() + d.len() + 80).sum();
        if total > MAX_ZIP {
            return Err(EngineError::Io("the bundle would exceed 4 GiB".into()));
        }
        let mut out = Vec::with_capacity(total);
        let mut central = Vec::new();
        for (name, data) in entries {
            let crc = crc32fast::hash(data);
            let (size, name_len) = (len32(data.len())?, u16::try_from(name.len()).map_err(|_| EngineError::Io("entry name too long".into()))?);
            let offset = len32(out.len())?;
            u32le(&mut out, LOCAL);
            u16le(&mut out, 20);
            u16le(&mut out, 0);
            u16le(&mut out, 0);
            u16le(&mut out, 0);
            u16le(&mut out, DOS_DATE);
            u32le(&mut out, crc);
            u32le(&mut out, size);
            u32le(&mut out, size);
            u16le(&mut out, name_len);
            u16le(&mut out, 0);
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(data);
            u32le(&mut central, CENTRAL);
            u16le(&mut central, 20);
            u16le(&mut central, 20);
            u16le(&mut central, 0);
            u16le(&mut central, 0);
            u16le(&mut central, 0);
            u16le(&mut central, DOS_DATE);
            u32le(&mut central, crc);
            u32le(&mut central, size);
            u32le(&mut central, size);
            u16le(&mut central, name_len);
            u16le(&mut central, 0);
            u16le(&mut central, 0);
            u16le(&mut central, 0);
            u16le(&mut central, 0);
            u32le(&mut central, 0);
            u32le(&mut central, offset);
            central.extend_from_slice(name.as_bytes());
        }
        let cd_offset = len32(out.len())?;
        let cd_size = len32(central.len())?;
        let count = u16::try_from(entries.len()).map_err(|_| EngineError::Io("too many files for one bundle".into()))?;
        out.extend_from_slice(&central);
        u32le(&mut out, END);
        u16le(&mut out, 0);
        u16le(&mut out, 0);
        u16le(&mut out, count);
        u16le(&mut out, count);
        u32le(&mut out, cd_size);
        u32le(&mut out, cd_offset);
        u16le(&mut out, 0);
        Ok(out)
    }

    pub fn is_zip(bytes: &[u8]) -> bool {
        rd32(bytes, 0) == Some(LOCAL)
    }

    fn rd16(b: &[u8], at: usize) -> Option<u16> {
        Some(u16::from_le_bytes([*b.get(at)?, *b.get(at + 1)?]))
    }

    fn rd32(b: &[u8], at: usize) -> Option<u32> {
        Some(u32::from_le_bytes([*b.get(at)?, *b.get(at + 1)?, *b.get(at + 2)?, *b.get(at + 3)?]))
    }

    /// Every file in the archive, by name (directories are skipped).
    pub fn read(bytes: &[u8]) -> Result<BTreeMap<String, Vec<u8>>> {
        let bad = |what: &str| EngineError::Io(format!("not a usable bundle: {what}"));
        // The end record is in the last 64 KiB (it may carry a comment).
        let floor = bytes.len().saturating_sub(22 + 0xFFFF);
        let end = (floor..=bytes.len().saturating_sub(22)).rev().find(|&i| rd32(bytes, i) == Some(END)).ok_or_else(|| bad("no end record"))?;
        let count = rd16(bytes, end + 10).ok_or_else(|| bad("truncated end record"))? as usize;
        let mut pos = rd32(bytes, end + 16).ok_or_else(|| bad("truncated end record"))? as usize;
        let mut files = BTreeMap::new();
        for _ in 0..count {
            if rd32(bytes, pos) != Some(CENTRAL) {
                return Err(bad("bad central directory"));
            }
            let method = rd16(bytes, pos + 10).ok_or_else(|| bad("truncated"))?;
            let csize = rd32(bytes, pos + 20).ok_or_else(|| bad("truncated"))? as usize;
            let usize_ = rd32(bytes, pos + 24).ok_or_else(|| bad("truncated"))? as usize;
            let name_len = rd16(bytes, pos + 28).ok_or_else(|| bad("truncated"))? as usize;
            let extra_len = rd16(bytes, pos + 30).ok_or_else(|| bad("truncated"))? as usize;
            let comment_len = rd16(bytes, pos + 32).ok_or_else(|| bad("truncated"))? as usize;
            let local = rd32(bytes, pos + 42).ok_or_else(|| bad("truncated"))? as usize;
            let name = bytes.get(pos + 46..pos + 46 + name_len).ok_or_else(|| bad("truncated name"))?;
            let name = String::from_utf8_lossy(name).into_owned();
            pos += 46 + name_len + extra_len + comment_len;
            if name.ends_with('/') {
                continue;
            }
            if rd32(bytes, local) != Some(LOCAL) {
                return Err(bad("bad local header"));
            }
            let ln = rd16(bytes, local + 26).ok_or_else(|| bad("truncated"))? as usize;
            let le = rd16(bytes, local + 28).ok_or_else(|| bad("truncated"))? as usize;
            let start = local + 30 + ln + le;
            let data = bytes.get(start..start + csize).ok_or_else(|| bad("truncated entry"))?;
            let data = match method {
                0 => data.to_vec(),
                8 => miniz_oxide::inflate::decompress_to_vec_with_limit(data, usize_.max(1))
                    .map_err(|e| EngineError::Io(format!("{name}: could not inflate ({e:?})")))?,
                m => return Err(EngineError::Io(format!("{name}: compression method {m} is not supported"))),
            };
            files.insert(name, data);
        }
        Ok(files)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn zip_round_trip() {
        let entries = vec![("a/b.txt".to_string(), b"hello".to_vec()), ("a/c.bin".to_string(), vec![0u8; 1000])];
        let z = zip::write(&entries).unwrap();
        assert!(zip::is_zip(&z));
        let files = zip::read(&z).unwrap();
        assert_eq!(files.get("a/b.txt").unwrap(), b"hello");
        assert_eq!(files.get("a/c.bin").unwrap().len(), 1000);
    }

    #[test]
    fn bundle_round_trip_keeps_audio() {
        let mut e = crate::demo::demo_engine();
        let sources = e.session().sources.len();
        assert!(sources > 0);
        let bytes = save(&mut e, "My Song.scraft.zip").unwrap();
        assert_eq!(e.session().name, "My Song");
        assert!(!e.is_dirty());
        let mut f = Engine::default();
        let missing = open_bytes(&mut f, "My Song.scraft.zip", &bytes).unwrap();
        assert!(missing.is_empty(), "{missing:?}");
        assert_eq!(f.session().sources.len(), sources);
        assert!(f.session().sources.iter().all(|s| f.session().pool.get(s.id).is_some()));
        assert_eq!(f.session().name, "My Song");
    }

    #[test]
    fn bare_session_file_opens_with_missing_media() {
        let e = crate::demo::demo_engine();
        let text = e.session().to_json().unwrap();
        let mut f = Engine::default();
        let missing = open_bytes(&mut f, "Demo.scraft", text.as_bytes()).unwrap();
        assert_eq!(missing.len(), e.session().sources.len());
    }

    #[test]
    fn tracks_only_keeps_masters() {
        let e = crate::demo::demo_engine();
        let first = e.session().tracks.iter().find(|t| t.kind != TrackKind::Master).map(|t| t.id).unwrap();
        let s = tracks_only(e.session(), &[first]);
        assert!(s.tracks.iter().any(|t| t.id == first));
        assert!(s.tracks.iter().all(|t| t.id == first || t.kind == TrackKind::Master));
        let _ = json!(s.tracks.len());
    }

    #[test]
    fn stems() {
        assert_eq!(stem("Song.scraft.zip"), "Song");
        assert_eq!(stem("/x/y/Song.scraft"), "Song");
        assert_eq!(stem("Song"), "Song");
        assert_eq!(stem(".zip"), "Untitled");
    }
}
