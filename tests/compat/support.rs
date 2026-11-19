//! Shared by the compatibility tests in `tests/compat.rs` and `crates/tickertape-cli/tests/compat_fixtures.rs`:
//! loads `tests/compat/fixtures/manifest.json` and unpacks fixtures into temporary directories (never in place).
//! The including crate must also include `sha256.rs` as `crate::sha256`.

use std::path::{Path, PathBuf};

use serde_json::Value;

/// The fixture directory in the repository.
pub fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .find(|p| p.join("tests/compat/fixtures/manifest.json").is_file())
        .expect("tests/compat/fixtures/manifest.json above CARGO_MANIFEST_DIR")
        .join("tests/compat/fixtures")
}

/// One manifest entry.
#[derive(Debug, Clone)]
pub struct Fixture {
    pub name: String,
    pub kind: String,
    pub json: Value,
}

impl Fixture {
    pub fn u64(&self, key: &str) -> u64 {
        self.json[key]
            .as_u64()
            .unwrap_or_else(|| panic!("{}: {key}", self.name))
    }

    pub fn str(&self, key: &str) -> &str {
        self.json[key]
            .as_str()
            .unwrap_or_else(|| panic!("{}: {key}", self.name))
    }

    pub fn checkpoint(&self, key: &str) -> u64 {
        self.json["checkpoint"][key]
            .as_u64()
            .unwrap_or_else(|| panic!("{}: checkpoint.{key}", self.name))
    }

    pub fn is_torn(&self) -> bool {
        self.kind == "torn"
    }

    /// Unpacks the fixture into a new temporary directory (the journal is its `journal` subdirectory) and checks
    /// every file's length and SHA-256 against the manifest, so the packaging can't have changed a byte.
    pub fn unpack(&self) -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let journal = tmp.path().join("journal");
        let blob = std::fs::read(fixtures_dir().join(self.str("pack"))).expect("read pack");
        let files = decode_pack(&blob).unwrap_or_else(|e| panic!("{}: {e}", self.name));
        let expected = self.json["files"].as_array().expect("files");
        assert_eq!(
            files.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
            expected
                .iter()
                .map(|f| f["name"].as_str().unwrap())
                .collect::<Vec<_>>(),
            "{}: file list",
            self.name
        );
        for ((name, data), want) in files.iter().zip(expected) {
            assert_eq!(
                data.len() as u64,
                want["len"].as_u64().unwrap(),
                "{}: {name} length",
                self.name
            );
            assert_eq!(
                crate::sha256::hex(data),
                want["sha256"].as_str().unwrap(),
                "{}: {name} digest",
                self.name
            );
            let path = journal.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, data).unwrap();
        }
        (tmp, journal)
    }
}

/// All fixtures in the manifest, sorted by name. Panics if the manifest is missing or empty.
pub fn fixtures() -> Vec<Fixture> {
    let text = std::fs::read_to_string(fixtures_dir().join("manifest.json")).expect("manifest");
    let manifest: Value = serde_json::from_str(&text).expect("manifest JSON");
    assert_eq!(manifest["format_version"], 1);
    let out: Vec<Fixture> = manifest["fixtures"]
        .as_array()
        .expect("fixtures")
        .iter()
        .map(|f| Fixture {
            name: f["name"].as_str().unwrap().to_string(),
            kind: f["kind"].as_str().unwrap().to_string(),
            json: f.clone(),
        })
        .collect();
    assert!(!out.is_empty(), "no fixtures in the manifest");
    out
}

/// Decodes a `TTFXPACK` v1 file (format in `tests/compat/pack.py`) into `(name, contents)` pairs.
pub fn decode_pack(blob: &[u8]) -> Result<Vec<(String, Vec<u8>)>, String> {
    struct Cursor<'a>(&'a [u8], usize);
    impl Cursor<'_> {
        fn take(&mut self, n: usize) -> Result<&[u8], String> {
            let end = self.1.checked_add(n).ok_or("overflow")?;
            let bytes = self.0.get(self.1..end).ok_or("truncated pack")?;
            self.1 = end;
            Ok(bytes)
        }
        fn u16(&mut self) -> Result<u16, String> {
            Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
        }
        fn u32(&mut self) -> Result<u32, String> {
            Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
        }
        fn u64(&mut self) -> Result<u64, String> {
            Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
        }
    }
    let mut c = Cursor(blob, 0);
    if c.take(8)? != b"TTFXPACK" {
        return Err("bad magic".into());
    }
    if c.u32()? != 1 {
        return Err("unsupported pack version".into());
    }
    let count = c.u32()?;
    let mut out: Vec<(String, Vec<u8>)> = Vec::new();
    for _ in 0..count {
        let name_len = c.u16()? as usize;
        let name = String::from_utf8(c.take(name_len)?.to_vec()).map_err(|e| e.to_string())?;
        if name.is_empty()
            || name.starts_with('/')
            || name
                .split('/')
                .any(|p| p == ".." || p == "." || p.is_empty())
            || out.iter().any(|(n, _)| *n == name)
        {
            return Err(format!("bad file name {name:?}"));
        }
        let size = usize::try_from(c.u64()?).map_err(|e| e.to_string())?;
        if size > 1 << 30 {
            return Err(format!("{name}: implausible size {size}"));
        }
        let runs = c.u32()?;
        let mut data = vec![0u8; size];
        let mut end = 0usize;
        for _ in 0..runs {
            let offset = usize::try_from(c.u64()?).map_err(|e| e.to_string())?;
            let len = c.u32()? as usize;
            if offset < end || offset.checked_add(len).is_none_or(|e| e > size) {
                return Err(format!("{name}: bad run at {offset}"));
            }
            data[offset..offset + len].copy_from_slice(c.take(len)?);
            end = offset + len;
        }
        out.push((name, data));
    }
    if c.1 != blob.len() {
        return Err("trailing bytes".into());
    }
    Ok(out)
}
