//! Steam content formats: the package zip, depot manifests and CDN chunks.

use std::collections::HashMap;
use std::io::{Cursor, Read};
use std::path::Path;

use aes::cipher::block_padding::Pkcs7;
use aes::cipher::{BlockCipherDecrypt, BlockModeDecrypt, KeyInit, KeyIvInit};
use anyhow::{Context, Result, bail, ensure};
use sha1::{Digest, Sha1};

use crate::log;

/// Largest chunk accepted from a manifest. Steam uses chunks of at most 1 MiB.
const MAX_CHUNK: u32 = 32 << 20;
#[cfg(unix)]
const FLAG_EXECUTABLE: u32 = 32;
const FLAG_DIRECTORY: u32 = 64;
const FLAG_SYMLINK: u32 = 512;

pub type Key = [u8; 32];

#[derive(Clone)]
pub struct Chunk {
    pub sha: [u8; 20],
    pub offset: u64,
    pub size: u32,
}

#[derive(Clone)]
pub struct Entry {
    /// Relative path using `/` separators, already checked for safety.
    pub name: String,
    pub size: u64,
    pub flags: u32,
    /// Sorted by offset, covering the file exactly.
    pub chunks: Vec<Chunk>,
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.flags & FLAG_DIRECTORY != 0
    }
    #[cfg(unix)]
    pub fn is_executable(&self) -> bool {
        self.flags & FLAG_EXECUTABLE != 0
    }
}

#[derive(Clone)]
pub struct Depot {
    pub id: u32,
    pub manifest: u64,
    /// When the manifest was made (Unix time), if it says.
    pub created: Option<u64>,
    /// The `.lua` file's comment for the depot, such as "VC 2022 Redist".
    pub name: Option<String>,
    pub key: Key,
    pub entries: Vec<Entry>,
}

impl Depot {
    pub fn files(&self) -> usize {
        self.entries.iter().filter(|entry| !entry.is_dir()).count()
    }

    pub fn size(&self) -> u64 {
        self.entries.iter().map(|entry| entry.size).sum()
    }
}

#[derive(Clone, Default)]
pub struct Package {
    /// The app ID, from `addappid(<app>)` in the `.lua` file.
    pub app: Option<u32>,
    /// The app's name and build, from the `.lua` file's comments if it has them.
    pub app_name: Option<String>,
    pub build: Option<String>,
    pub depots: Vec<Depot>,
    pub warnings: Vec<String>,
}

impl Package {
    /// A copy holding only the depots that `keep` accepts.
    pub fn select(&self, keep: impl Fn(u32) -> bool) -> Package {
        Package {
            app: self.app,
            app_name: self.app_name.clone(),
            build: self.build.clone(),
            depots: self.depots.iter().filter(|depot| keep(depot.id)).cloned().collect(),
            warnings: self.warnings.clone(),
        }
    }
}

/// What a package's `.lua` file says.
#[derive(Default)]
pub struct Lua {
    pub app: Option<u32>,
    pub app_name: Option<String>,
    pub build: Option<String>,
    pub keys: HashMap<u32, Key>,
    /// Comments naming depots.
    pub names: HashMap<u32, String>,
}

/// Read a package zip holding a `.lua` file with depot keys and the
/// depots' `.manifest` files.
pub fn read_package(path: &Path) -> Result<Package> {
    log::write(format!("package: reading {path:?}"));
    let data = std::fs::read(path).context("could not read the package")?;
    log::write(format!("package: {} bytes", data.len()));
    let mut archive = zip::ZipArchive::new(Cursor::new(data)).context("the package is not a zip file")?;
    let mut lua = String::new();
    let mut manifests = Vec::new();
    for index in 0..archive.len() {
        let mut item = archive.by_index(index).context("the package zip is damaged")?;
        let name = item.name().context("the package zip is damaged")?.to_ascii_lowercase();
        log::write(format!("package: entry {name:?}, {} bytes", item.size()));
        let unreadable = || format!("could not read {name} in the package");
        if name.ends_with(".lua") {
            item.read_to_string(&mut lua).with_context(unreadable)?;
            lua.push('\n');
        } else if name.ends_with(".manifest") {
            let mut bytes = Vec::new();
            item.read_to_end(&mut bytes).with_context(unreadable)?;
            manifests.push((name, bytes));
        }
    }
    for line in lua.lines() {
        log::write(format!("lua| {}", log::redact_keys(line)));
    }
    let mut lua = parse_lua(&lua);
    let mut keyed: Vec<_> = lua.keys.keys().collect();
    keyed.sort();
    log::write(format!("lua: app {:?}, name {:?}, build {:?}, keys for depots {keyed:?}",
                       lua.app, lua.app_name, lua.build));
    let mut package = Package { app: lua.app, app_name: lua.app_name, build: lua.build, ..Package::default() };
    for (name, bytes) in manifests {
        log::write(format!("package: parsing {name}"));
        let depot = parse_manifest(&bytes, &lua.keys, &mut package.warnings)
            .with_context(|| format!("{name} in the package is not a valid manifest"))?;
        match depot {
            Some(depot) if package.depots.iter().any(|d| d.id == depot.id) => {
                bail!("the package has more than one manifest for depot {}", depot.id)
            }
            Some(depot) => package.depots.push(Depot { name: lua.names.remove(&depot.id), ..depot }),
            None => package.warnings.push(format!("Skipping {name}: no key for its depot")),
        }
    }
    for warning in &package.warnings {
        log::write(format!("package: warning: {warning}"));
    }
    ensure!(!package.depots.is_empty(), "the package has no manifests with a matching depot key");
    Ok(package)
}

/// Read depot keys from lines like `addappid(2862861, 1, "<64 hex digits>")`
/// and the app from the first `addappid(2862860)`. Comments after those calls
/// name the app or depot, and a `-- Build: ...` comment gives the build; both
/// are optional.
pub fn parse_lua(text: &str) -> Lua {
    let mut lua = Lua::default();
    for line in text.lines() {
        let (code, comment) = line.split_once("--").unwrap_or((line, ""));
        let comment = comment.trim();
        if let Some(build) = comment.strip_prefix("Build:") {
            lua.build.get_or_insert_with(|| build.trim().to_string());
        }
        for call in code.split("addappid(").skip(1) {
            let args: Vec<&str> = call.split(')').next().unwrap_or("").split(',').map(str::trim).collect();
            let Ok(id) = args[0].parse::<u32>() else { continue };
            // Some generators label each depot with just "Depot <id>".
            let name = Some(comment.to_string()).filter(|c| !c.is_empty() && *c != format!("Depot {id}"));
            match args.as_slice() {
                [_] if lua.app.is_none() => (lua.app, lua.app_name) = (Some(id), name),
                [_, _, key, ..] => {
                    if let Some(key) = parse_key(key.trim_matches(['"', '\''])) {
                        lua.keys.insert(id, key);
                        lua.names.extend(name.map(|name| (id, name)));
                    }
                }
                _ => {}
            }
        }
    }
    lua
}

pub fn parse_key(hex: &str) -> Option<Key> {
    let hex = hex.as_bytes();
    if hex.len() != 64 {
        return None;
    }
    let digit = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let mut key = [0; 32];
    for (byte, pair) in key.iter_mut().zip(hex.chunks(2)) {
        *byte = digit(pair[0])? << 4 | digit(pair[1])?;
    }
    Some(key)
}

/// A protobuf field value: varints and fixed-width numbers become `Int`.
enum Value<'a> {
    Int(u64),
    Bytes(&'a [u8]),
}

impl Value<'_> {
    fn int(&self) -> u64 {
        match self {
            Value::Int(value) => *value,
            Value::Bytes(_) => 0,
        }
    }
    fn bytes(&self) -> &[u8] {
        match self {
            Value::Bytes(bytes) => bytes,
            Value::Int(_) => &[],
        }
    }
}

fn protobuf(mut data: &[u8]) -> Result<Vec<(u32, Value<'_>)>> {
    fn take<'a>(data: &mut &'a [u8], size: usize) -> Result<&'a [u8]> {
        ensure!(data.len() >= size, "truncated manifest");
        let (head, tail) = data.split_at(size);
        *data = tail;
        Ok(head)
    }
    fn varint(data: &mut &[u8]) -> Result<u64> {
        let mut value = 0;
        for shift in (0..64).step_by(7) {
            let byte = take(data, 1)?[0];
            value |= u64::from(byte & 0x7f) << shift;
            if byte < 0x80 {
                return Ok(value);
            }
        }
        bail!("invalid protobuf integer")
    }
    let mut fields = Vec::new();
    while !data.is_empty() {
        let tag = varint(&mut data)?;
        let number = u32::try_from(tag >> 3).ok().filter(|&n| n != 0).context("invalid protobuf field")?;
        let value = match tag & 7 {
            0 => Value::Int(varint(&mut data)?),
            1 => Value::Int(u64::from_le_bytes(take(&mut data, 8)?.try_into()?)),
            2 => {
                let size = usize::try_from(varint(&mut data)?)?;
                Value::Bytes(take(&mut data, size)?)
            }
            5 => Value::Int(u32::from_le_bytes(take(&mut data, 4)?.try_into()?).into()),
            wire => bail!("unsupported protobuf wire type {wire}"),
        };
        fields.push((number, value));
    }
    Ok(fields)
}

fn field<'a>(fields: &'a [(u32, Value<'a>)], number: u32) -> Option<&'a Value<'a>> {
    fields.iter().find(|(n, _)| *n == number).map(|(_, value)| value)
}

/// Parse a raw or ZIP-wrapped manifest. Returns `None` when `keys` has no
/// key for the manifest's depot. Symbolic links are skipped with a warning.
pub fn parse_manifest(data: &[u8], keys: &HashMap<u32, Key>, warnings: &mut Vec<String>) -> Result<Option<Depot>> {
    let zipped = data.starts_with(b"PK");
    let unzipped;
    let mut data = data;
    if zipped {
        unzipped = unzip(data, None)?;
        data = &unzipped;
    }
    let mut sections = HashMap::new();
    loop {
        ensure!(data.len() >= 4, "truncated manifest");
        let magic = u32::from_le_bytes(data[..4].try_into()?);
        if magic == 0x32C4_15AB {
            break; // End marker, no length follows.
        }
        ensure!(data.len() >= 8, "truncated manifest");
        let size = u32::from_le_bytes(data[4..8].try_into()?) as usize;
        ensure!(data.len() >= 8 + size, "truncated manifest");
        sections.insert(magic, &data[8..8 + size]);
        data = &data[8 + size..];
    }
    let (Some(payload), Some(metadata)) = (sections.get(&0x71F6_17D0), sections.get(&0x1F48_12BE)) else {
        bail!("manifest is missing its payload or metadata");
    };
    let metadata = protobuf(metadata)?;
    let id = field(&metadata, 1).map_or(0, Value::int);
    let depot = u32::try_from(id).ok().filter(|&id| id != 0).context("manifest has no valid depot ID")?;
    let manifest = field(&metadata, 2).map_or(0, Value::int);
    let created = field(&metadata, 3).map(Value::int).filter(|&time| time != 0);
    let encrypted_names = field(&metadata, 4).is_some_and(|v| v.int() != 0);
    log::write(format!("manifest: depot {depot}, manifest {manifest}, created {created:?}, \
                        encrypted names {encrypted_names}, zipped {zipped}"));
    let Some(&key) = keys.get(&depot) else { return Ok(None) };

    let mut entries = Vec::new();
    for (number, mapping) in protobuf(payload)? {
        if number != 1 {
            continue;
        }
        let fields = protobuf(mapping.bytes())?;
        let mut name = field(&fields, 1).map_or(&[][..], Value::bytes).to_vec();
        if encrypted_names {
            let encoded = std::str::from_utf8(&name).context("encrypted file name is not base64")?;
            name = decrypt(&base64(encoded)?, &key)?;
            while name.last() == Some(&0) {
                name.pop();
            }
        }
        let name = String::from_utf8(name).context("file name is not UTF-8")?.replace('\\', "/");
        let flags = field(&fields, 3).map_or(0, Value::int) as u32;
        if flags & FLAG_SYMLINK != 0 || field(&fields, 7).is_some_and(|v| !v.bytes().is_empty()) {
            warnings.push(format!("Skipping symbolic link {name:?}"));
            continue;
        }
        check_name(&name)?;
        let mut entry = Entry { name, size: field(&fields, 2).map_or(0, Value::int), flags, chunks: Vec::new() };
        for (number, chunk) in &fields {
            if *number == 6 {
                let chunk = protobuf(chunk.bytes())?;
                entry.chunks.push(Chunk {
                    sha: field(&chunk, 1).map_or(&[][..], Value::bytes).try_into().unwrap_or_default(),
                    offset: field(&chunk, 3).map_or(0, Value::int),
                    size: field(&chunk, 4).map_or(0, Value::int).try_into().unwrap_or(0),
                });
            }
        }
        entry.chunks.sort_by_key(|chunk| chunk.offset);
        if !entry.is_dir() {
            let mut offset = 0;
            for chunk in &entry.chunks {
                ensure!(chunk.offset == offset && (1..=MAX_CHUNK).contains(&chunk.size) && chunk.sha != [0; 20],
                        "invalid chunk layout for {:?}", entry.name);
                offset += u64::from(chunk.size);
            }
            ensure!(offset == entry.size, "chunks do not cover {:?}", entry.name);
        }
        entries.push(entry);
    }
    let chunks: usize = entries.iter().map(|entry| entry.chunks.len()).sum();
    let size: u64 = entries.iter().map(|entry| entry.size).sum();
    log::write(format!("manifest: depot {depot}: {} entries ({} folders), {chunks} chunks, {size} bytes",
                       entries.len(), entries.iter().filter(|entry| entry.is_dir()).count()));
    Ok(Some(Depot { id: depot, manifest, created, name: None, key, entries }))
}

/// Reject manifest paths that could escape the output directory.
pub fn check_name(name: &str) -> Result<()> {
    // Manifest names have `/` by now, so a backslash can only be an attempt
    // to climb out on Windows, for example from an edited list of installed files.
    let bad_part = |part: &str| part.trim_end_matches([' ', '.']).is_empty() || part.contains([':', '\0', '\\']);
    ensure!(!name.starts_with('/') && !name.split('/').any(bad_part), "unsafe path in manifest: {name:?}");
    Ok(())
}

fn base64(text: &str) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let (mut bits, mut count) = (0u32, 0);
    for c in text.trim_end_matches('=').bytes() {
        let value = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => bail!("invalid base64 in file name"),
        };
        bits = bits << 6 | u32::from(value);
        count += 6;
        if count >= 8 {
            count -= 8;
            out.push((bits >> count) as u8);
        }
    }
    Ok(out)
}

fn unzip(data: &[u8], limit: Option<u32>) -> Result<Vec<u8>> {
    let mut archive = zip::ZipArchive::new(Cursor::new(data))?;
    let files: Vec<usize> = (0..archive.len()).filter(|&i| archive.by_index(i).is_ok_and(|f| !f.is_dir())).collect();
    ensure!(files.len() == 1, "expected exactly one file in ZIP container");
    let mut out = Vec::new();
    let limit = limit.map_or(u64::MAX, |limit| u64::from(limit) + 1);
    archive.by_index(files[0])?.take(limit).read_to_end(&mut out)?;
    Ok(out)
}

/// Steam's chunk and file-name encryption: an ECB-encrypted IV, then AES-256-CBC.
pub fn decrypt(data: &[u8], key: &Key) -> Result<Vec<u8>> {
    ensure!(data.len() >= 32 && data.len().is_multiple_of(16), "invalid encrypted data length");
    let mut iv: [u8; 16] = data[..16].try_into()?;
    aes::Aes256::new(key.into()).decrypt_block((&mut iv).into());
    cbc::Decryptor::<aes::Aes256>::new(key.into(), &iv.into())
        .decrypt_padded_vec::<Pkcs7>(&data[16..])
        .map_err(|_| anyhow::anyhow!("decryption failed (wrong depot key?)"))
}

/// Unpack a decrypted chunk: `VSZa` zstd, `VZa` LZMA, or a ZIP container.
pub fn decompress(data: &[u8], size: u32) -> Result<Vec<u8>> {
    let u32_at = |at: usize| u32::from_le_bytes(data[at..at + 4].try_into().unwrap());
    let n = data.len();
    let mut plain = Vec::with_capacity(size as usize);
    let (crc, declared) = if data.starts_with(b"VSZa") && n >= 23 && data.ends_with(b"zsv") {
        ensure!(u32_at(4) == u32_at(n - 15), "zstd container checksums disagree");
        let decoder = ruzstd::decoding::StreamingDecoder::new(&data[8..n - 15])
            .map_err(|e| anyhow::anyhow!("zstd: {e}"))?;
        decoder.take(u64::from(size) + 1).read_to_end(&mut plain)?;
        (u32_at(n - 15), u32_at(n - 11))
    } else if data.starts_with(b"VZa") && n >= 22 && data.ends_with(b"zv") {
        let declared = u32_at(n - 6);
        ensure!(declared == size, "chunk size mismatch");
        let options = lzma_rs::decompress::Options {
            unpacked_size: lzma_rs::decompress::UnpackedSize::UseProvided(Some(declared.into())),
            memlimit: Some(128 << 20),
            allow_incomplete: false,
        };
        lzma_rs::lzma_decompress_with_options(&mut &data[7..n - 10], &mut plain, &options)
            .map_err(|e| anyhow::anyhow!("lzma: {e}"))?;
        (u32_at(n - 10), declared)
    } else if data.starts_with(b"PK") {
        plain = unzip(data, Some(size))?;
        (crc32fast::hash(&plain), size)
    } else {
        bail!("unknown chunk compression format");
    };
    ensure!(plain.len() == size as usize && declared == size && crc32fast::hash(&plain) == crc,
            "chunk size or checksum mismatch");
    Ok(plain)
}

/// Turn a downloaded chunk into its verified plain content.
pub fn decode_chunk(data: &[u8], key: &Key, chunk: &Chunk) -> Result<Vec<u8>> {
    let plain = decompress(&decrypt(data, key)?, chunk.size)?;
    ensure!(Sha1::digest(&plain)[..] == chunk.sha, "chunk SHA-1 mismatch");
    Ok(plain)
}
