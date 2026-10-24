//! Writes the browser bridge extension where browsers can load it from.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const SCRIPTS: [(&str, &str); 3] = [
    ("inject.js", include_str!("../extension/inject.js")),
    ("relay.js", include_str!("../extension/relay.js")),
    ("background.js", include_str!("../extension/background.js")),
];
const CHROME_MANIFEST: &str = include_str!("../extension/manifest.json");
const FIREFOX_MANIFEST: &str = include_str!("../extension/manifest.firefox.json");

pub struct Bridge {
    /// Unpacked extension for Chrome, Edge, Brave, Opera and other Chromium browsers.
    pub chrome_dir: PathBuf,
    /// Unpacked extension for Firefox-based browsers (temporary add-on).
    pub firefox_dir: PathBuf,
    /// The Firefox extension as an installable add-on file.
    pub firefox_xpi: PathBuf,
}

pub fn write(data_dir: &Path) -> io::Result<Bridge> {
    let bridge = Bridge {
        chrome_dir: data_dir.join("pocket-option-bridge"),
        firefox_dir: data_dir.join("pocket-option-bridge-firefox"),
        firefox_xpi: data_dir.join("pocket-option-bridge-firefox.xpi"),
    };
    for (dir, manifest) in [(&bridge.chrome_dir, CHROME_MANIFEST), (&bridge.firefox_dir, FIREFOX_MANIFEST)] {
        fs::create_dir_all(dir)?;
        fs::write(dir.join("manifest.json"), manifest)?;
        for (name, body) in SCRIPTS {
            fs::write(dir.join(name), body)?;
        }
    }
    let mut files = vec![("manifest.json", FIREFOX_MANIFEST.as_bytes())];
    files.extend(SCRIPTS.iter().map(|(n, b)| (*n, b.as_bytes())));
    fs::write(&bridge.firefox_xpi, zip(&files))?;
    Ok(bridge)
}

/// A minimal zip archive with uncompressed ("stored") entries, which is all an
/// .xpi needs.
fn zip(files: &[(&str, &[u8])]) -> Vec<u8> {
    const DOS_DATE_1980_01_01: u16 = 0x21;
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data) in files {
        let offset = out.len() as u32;
        let crc = crc32(data);
        let size = data.len() as u32;
        let header = |sig: u32, central: bool| {
            let mut h = Vec::new();
            h.extend(sig.to_le_bytes());
            if central {
                h.extend(20u16.to_le_bytes()); // made by
            }
            for v in [20u16, 0, 0, 0, DOS_DATE_1980_01_01] {
                h.extend(v.to_le_bytes()); // version needed, flags, stored, time, date
            }
            for v in [crc, size, size] {
                h.extend(v.to_le_bytes());
            }
            h.extend((name.len() as u16).to_le_bytes());
            h.extend(0u16.to_le_bytes()); // extra field length
            if central {
                // Comment length (2), disk (2), internal (2) and external (4) attributes.
                h.extend([0u8; 10]);
                h.extend(offset.to_le_bytes());
            }
            h.extend(name.as_bytes());
            h
        };
        out.extend(header(0x0403_4b50, false));
        out.extend_from_slice(data);
        central.extend(header(0x0201_4b50, true));
    }
    let central_offset = out.len() as u32;
    out.extend(&central);
    out.extend(0x0605_4b50u32.to_le_bytes());
    out.extend([0u8; 4]); // disk numbers
    out.extend((files.len() as u16).to_le_bytes());
    out.extend((files.len() as u16).to_le_bytes());
    out.extend((central.len() as u32).to_le_bytes());
    out.extend(central_offset.to_le_bytes());
    out.extend(0u16.to_le_bytes()); // comment length
    out
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_reference() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn writes_both_browsers() {
        let dir = std::env::temp_dir().join(format!("daytrade-bridge-{}", std::process::id()));
        let b = write(&dir).unwrap();
        assert!(fs::read_to_string(b.chrome_dir.join("manifest.json")).unwrap().contains("service_worker"));
        let firefox = fs::read_to_string(b.firefox_dir.join("manifest.json")).unwrap();
        assert!(firefox.contains("gecko"));
        // Firefox ignores match patterns that include a port.
        assert!(!firefox.contains("127.0.0.1:"), "{firefox}");
        let xpi = fs::read(&b.firefox_xpi).unwrap();
        assert_eq!(&xpi[..4], b"PK\x03\x04");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn zip_layout() {
        let files: [(&str, &[u8]); 2] = [("a.txt", b"hello"), ("dir/b.js", b"x")];
        let z = zip(&files);
        let le16 = |i: usize| u16::from_le_bytes([z[i], z[i + 1]]) as usize;
        let le32 = |i: usize| u32::from_le_bytes([z[i], z[i + 1], z[i + 2], z[i + 3]]) as usize;
        let eocd = z.len() - 22;
        assert_eq!(le32(eocd), 0x0605_4b50);
        assert_eq!(le16(eocd + 10), 2);
        let (size, mut at) = (le32(eocd + 12), le32(eocd + 16));
        assert_eq!(at + size, eocd);
        for (name, data) in files {
            assert_eq!(le32(at), 0x0201_4b50);
            assert_eq!(le32(at + 16), crc32(data) as usize);
            assert_eq!(&z[at + 46..at + 46 + name.len()], name.as_bytes());
            let local = le32(at + 42);
            assert_eq!(le32(local), 0x0403_4b50);
            let body = local + 30 + le16(local + 26);
            assert_eq!(&z[body..body + data.len()], data);
            at += 46 + le16(at + 28);
        }
    }
}
