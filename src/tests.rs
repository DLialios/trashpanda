//! Offline tests: formats, path safety, and full downloads against local servers.

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Cursor, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use aes::cipher::block_padding::Pkcs7;
use aes::cipher::{BlockCipherEncrypt, BlockModeEncrypt, KeyInit, KeyIvInit};
use sha1::{Digest, Sha1};

use crate::depot::{self, Key};
use crate::download::{self, Meter, Progress, Rates};

const KEY: Key = [7; 32];
const IV: [u8; 16] = [9; 16];

fn varint(mut value: u64) -> Vec<u8> {
    let mut out = Vec::new();
    while value >= 0x80 {
        out.push(value as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
    out
}

fn int(number: u32, value: u64) -> Vec<u8> {
    [varint(u64::from(number) << 3), varint(value)].concat()
}

fn bytes(number: u32, value: &[u8]) -> Vec<u8> {
    [varint(u64::from(number) << 3 | 2), varint(value.len() as u64), value.to_vec()].concat()
}

fn encrypt(data: &[u8]) -> Vec<u8> {
    let mut iv = IV;
    aes::Aes256::new(&KEY.into()).encrypt_block((&mut iv).into());
    let body = cbc::Encryptor::<aes::Aes256>::new(&KEY.into(), &IV.into()).encrypt_padded_vec::<Pkcs7>(data);
    [&iv[..], &body].concat()
}

fn zipped(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for (name, data) in files {
        writer.start_file(*name, options).unwrap();
        writer.write_all(data).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

fn compressed(data: &[u8], kind: &str) -> Vec<u8> {
    let crc = crc32fast::hash(data).to_le_bytes();
    let size = (data.len() as u32).to_le_bytes();
    match kind {
        "zstd" => {
            let frame = ruzstd::encoding::compress_to_vec(data, ruzstd::encoding::CompressionLevel::Fastest);
            [&b"VSZa"[..], &crc, &frame, &crc, &size, &[0; 4], b"zsv"].concat()
        }
        "lzma" => {
            let mut alone = Vec::new();
            lzma_rs::lzma_compress(&mut &data[..], &mut alone).unwrap();
            // Steam drops the 8-byte size from the standard LZMA-alone header.
            [&b"VZa"[..], &crc, &alone[..5], &alone[13..], &crc, &size, b"zv"].concat()
        }
        _ => zipped(&[("data", data)]),
    }
}

fn manifest(mappings: &[Vec<u8>], encrypted: bool) -> Vec<u8> {
    let payload: Vec<u8> = mappings.iter().flat_map(|m| bytes(1, m)).collect();
    let metadata = [int(1, 123), int(2, 9876543210123456789), int(3, 1_794_069_781), int(4, encrypted.into())].concat();
    let section = |magic: u32, data: &[u8]| [&magic.to_le_bytes()[..], &(data.len() as u32).to_le_bytes(), data].concat();
    [section(0x71F617D0, &payload), section(0x1F4812BE, &metadata), section(0x1B81B817, &[]),
     0x32C415ABu32.to_le_bytes().to_vec()].concat()
}

/// A three-chunk file, one chunk per compression format.
struct Fixture {
    data: Vec<u8>,
    parts: Vec<Vec<u8>>,
    chunks: Vec<Vec<u8>>,
    responses: HashMap<String, Vec<u8>>,
}

fn fixture() -> Fixture {
    let parts: Vec<Vec<u8>> = [("new compression\n", 500), ("older compression\n", 300), ("zip compression\n", 400)]
        .iter().map(|(text, count)| text.repeat(*count).into_bytes()).collect();
    let (mut chunks, mut responses, mut offset) = (Vec::new(), HashMap::new(), 0);
    for (part, kind) in parts.iter().zip(["zstd", "lzma", "zip"]) {
        let sha = Sha1::digest(part);
        chunks.push([bytes(1, &sha), int(3, offset), int(4, part.len() as u64)].concat());
        responses.insert(hex(&sha), encrypt(&compressed(part, kind)));
        offset += part.len() as u64;
    }
    chunks.reverse(); // Steam sorts chunks by SHA-1, not offset.
    Fixture { data: parts.concat(), parts, chunks, responses }
}

impl Fixture {
    fn mapping(&self, name: &[u8]) -> Vec<u8> {
        let mut mapping = [bytes(1, name), int(2, self.data.len() as u64), int(3, 32)].concat();
        for chunk in &self.chunks {
            mapping.extend(bytes(6, chunk));
        }
        mapping
    }
}

fn hex(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect()
}

/// A scratch directory removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("rin-dl-test-{}-{name}", std::process::id()));
        _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        TempDir(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        _ = fs::remove_dir_all(&self.0);
    }
}

/// A local HTTP server serving `responses` by chunk SHA-1, or 503 for everything.
struct Server {
    url: String,
    requests: Arc<Mutex<Vec<String>>>,
}

fn serve(responses: &HashMap<String, Vec<u8>>, broken: bool) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (responses, log) = (Arc::new(responses.clone()), requests.clone());
    thread::spawn(move || {
        for stream in listener.incoming() {
            let (mut stream, responses, log) = (stream.unwrap(), responses.clone(), log.clone());
            thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                reader.read_line(&mut request).unwrap();
                let mut header = String::new();
                while reader.read_line(&mut header).unwrap() > 2 {
                    header.clear();
                }
                let path = request.split_whitespace().nth(1).unwrap_or("").to_string();
                let body = responses.get(path.rsplit('/').next().unwrap()).filter(|_| !broken);
                let status = if body.is_some() { "200 OK" } else { "503 Unavailable" };
                let body = body.map_or(&[][..], Vec::as_slice);
                log.lock().unwrap().push(path);
                write!(stream, "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
                stream.write_all(body).unwrap();
            });
        }
    });
    Server { url, requests }
}

fn lua(keys: &[(u32, Key)]) -> String {
    let mut text = String::from("-- comment addappid(1, 1, \"not a key\")\naddappid(2862860) -- the app\n");
    for (depot, key) in keys {
        text += &format!("addappid({depot}, 1, \"{}\") -- Depot {depot}\nsetManifestid({depot}, \"1\", 0)\n", hex(key));
    }
    text
}

#[test]
fn reads_raw_zipped_and_encrypted_manifests() {
    let fixture = fixture();
    let keys = HashMap::from([(123, KEY)]);
    for encrypted in [false, true] {
        for wrap in [false, true] {
            let name = if encrypted {
                let encoded = encrypt(b"folder\\game.bin\0");
                base64(&encoded).into_bytes()
            } else {
                b"folder\\game.bin".to_vec()
            };
            let mut data = manifest(&[fixture.mapping(&name)], encrypted);
            if wrap {
                data = zipped(&[("z", &data)]);
            }
            let depot = depot::parse_manifest(&data, &keys, &mut Vec::new()).unwrap().unwrap();
            assert_eq!((depot.id, depot.manifest), (123, 9876543210123456789));
            assert_eq!(depot.entries[0].name, "folder/game.bin");
            let offsets: Vec<u64> = depot.entries[0].chunks.iter().map(|c| c.offset).collect();
            assert_eq!(offsets, [0, 8000, 8000 + 5400]);
        }
    }
    assert!(depot::parse_manifest(&manifest(&[], false), &HashMap::new(), &mut Vec::new()).unwrap().is_none());
}

fn base64(data: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for group in data.chunks(3) {
        let n = group.iter().enumerate().fold(0u32, |n, (i, &b)| n | u32::from(b) << (16 - 8 * i));
        for i in 0..=group.len() {
            out.push(TABLE[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    while !out.len().is_multiple_of(4) {
        out.push('=');
    }
    out
}

#[test]
fn rejects_broken_manifests_and_skips_symlinks() {
    let fixture = fixture();
    let keys = HashMap::from([(123, KEY)]);
    let valid = manifest(&[fixture.mapping(b"game.bin")], false);
    let mut missing = fixture.mapping(b"game.bin");
    missing.truncate(missing.len() - fixture.chunks[0].len() - 2); // Drop the last chunk.
    let doubled = [fixture.mapping(b"game.bin"), fixture.chunks.iter().flat_map(|c| bytes(6, c)).collect()].concat();
    let wrong_size = [bytes(1, b"game.bin"), int(2, 1), bytes(6, &fixture.chunks[0])].concat();
    for blob in [vec![], valid[..valid.len() - 1].to_vec(), valid[..10].to_vec(),
                 manifest(&[missing], false), manifest(&[doubled], false), manifest(&[wrong_size], false),
                 manifest(&[fixture.mapping(b"../outside")], false)] {
        assert!(depot::parse_manifest(&blob, &keys, &mut Vec::new()).is_err());
    }
    let link = [bytes(1, b"link"), int(3, 512), bytes(7, b"target")].concat();
    let mut warnings = Vec::new();
    let depot = depot::parse_manifest(&manifest(&[link], false), &keys, &mut warnings).unwrap().unwrap();
    assert!(depot.entries.is_empty());
    assert_eq!(warnings.len(), 1);
}

#[test]
fn decodes_each_chunk_format_and_checks_it() {
    let data = fixture().data;
    for kind in ["zstd", "lzma", "zip"] {
        let encoded = compressed(&data, kind);
        assert_eq!(depot::decompress(&encoded, data.len() as u32).unwrap(), data, "{kind}");
        assert!(depot::decompress(&encoded, data.len() as u32 - 1).is_err(), "{kind}");
        if kind != "zip" {
            let mut bad = encoded.clone();
            let at = bad.len() - if kind == "zstd" { 15 } else { 10 };
            bad[at] ^= 1; // Corrupt the trailing CRC.
            assert!(depot::decompress(&bad, data.len() as u32).is_err(), "{kind}");
        }
    }
    let encrypted = encrypt(&compressed(&data, "zstd"));
    assert!(depot::decrypt(&encrypted, &[0; 32]).is_err());
    let chunk = depot::Chunk { sha: Sha1::digest(&data).into(), offset: 0, size: data.len() as u32 };
    assert_eq!(depot::decode_chunk(&encrypted, &KEY, &chunk).unwrap(), data);
    let wrong = depot::Chunk { sha: [1; 20], ..chunk };
    assert!(depot::decode_chunk(&encrypted, &KEY, &wrong).unwrap_err().to_string().contains("SHA-1"));
}

#[test]
fn reads_keys_and_names_from_lua() {
    let text = lua(&[(2862861, KEY), (228989, [0xab; 32])])
        + "-- Build: 26508517 (public branch)\naddappid(9) -- DLC\n"
        + &format!("addappid(5, 1, '{}') -- VC 2022 Redist\n", hex(&[1; 32]));
    let lua = depot::parse_lua(&text);
    assert_eq!(lua.keys.len(), 3);
    assert_eq!((lua.keys[&2862861], lua.keys[&228989], lua.keys[&5]), (KEY, [0xab; 32], [1; 32]));
    assert_eq!((lua.app, lua.app_name.as_deref()), (Some(2862860), Some("the app")));
    assert_eq!(lua.build.as_deref(), Some("26508517 (public branch)"));
    // "Depot <id>" says nothing the ID does not.
    assert_eq!(lua.names, HashMap::from([(5, "VC 2022 Redist".to_string())]));
    let bad = depot::parse_lua("addappid(5, 1, \"abc\")");
    assert!(bad.keys.is_empty() && bad.app.is_none());
}

#[test]
fn formats_dates() {
    for (time, date) in [(0, "1970-01-01"), (951_782_400, "2000-02-29"), (1_794_069_781, "2026-11-07")] {
        assert_eq!(download::date(time), date);
    }
}

#[test]
fn rejects_unsafe_paths() {
    for name in ["../outside", "/outside", "C:/outside", "x/../../outside", "", ".", "x\0y", "a//b", "a/. ./b"] {
        assert!(depot::check_name(name).is_err(), "{name:?}");
    }
    depot::check_name("folder/game.bin").unwrap();
}

#[test]
fn filters_content_servers() {
    let json = r#"{"response": {"servers": [
        {"type": "SteamCache", "host": "restricted.test", "https_support": "mandatory", "allowed_app_ids": [1]},
        {"type": "CDN", "host": "proxy.test", "https_support": "optional", "use_as_proxy": true},
        {"type": "CDN", "host": "plain.test", "https_support": "unavailable"},
        {"type": "SteamCache", "host": "a.test", "vhost": "cache.test", "https_support": "mandatory"},
        {"type": "CDN", "host": "b.test", "https_support": "optional", "allowed_app_ids": []}]}}"#;
    assert_eq!(download::parse_servers(json).unwrap(), ["https://cache.test", "https://b.test"]);
}

#[test]
fn meter_reports_network_speed_and_time_left() {
    const MIB: u64 = 1 << 20;
    let progress = Progress::default();
    progress.total.store(100 * MIB, Relaxed);
    let mut meter = Meter::default();
    let start = Instant::now();
    assert_eq!(meter.update(&progress, start), Rates { speed: None, left: None });

    // 40 MiB found on disk and 20 MiB downloaded (10 MiB over the wire) in 2s:
    // the 40 MiB still to go take 4s at the 10 MiB/s download rate.
    progress.done.store(60 * MIB, Relaxed);
    progress.fetched.store(20 * MIB, Relaxed);
    progress.received.store(10 * MIB, Relaxed);
    let rates = meter.update(&progress, start + Duration::from_secs(2));
    assert_eq!((rates.speed_text(), rates.left_text()), ("5.0 MiB/s".into(), "4s".into()));

    // Ten seconds without data: the old samples age out and the estimate is unknown.
    let rates = meter.update(&progress, start + Duration::from_secs(12));
    assert_eq!((rates.speed_text(), rates.left_text()), ("0 B/s".into(), "--".into()));

    progress.done.store(100 * MIB, Relaxed);
    assert_eq!(meter.update(&progress, start + Duration::from_secs(13)).left, Some(0.0));
    assert_eq!((download::duration(375), download::duration(3725)), ("6m 15s".into(), "1h 02m".into()));
}

#[test]
fn downloads_resumes_and_repairs() {
    let fixture = fixture();
    let temp = TempDir::new("download");
    let manifest = manifest(&[
        fixture.mapping(b"folder\\game.bin"),
        [bytes(1, b"empty-directory"), int(3, 64)].concat(),
        [bytes(1, b"empty-file"), int(2, 0)].concat(),
    ], false);
    let package_path = temp.0.join("package.zip");
    fs::write(&package_path, zipped(&[("123.lua", lua(&[(123, KEY)]).as_bytes()), ("123_1.manifest", &manifest),
                                      ("99_1.manifest", &manifest_for_depot(99))])).unwrap();
    let mut package = depot::read_package(&package_path).unwrap();
    assert_eq!(package.warnings.len(), 1, "the manifest without a key is skipped");
    assert_eq!(download::describe(&package), [
        "App 2862860: the app",
        "Depot 123 (manifest 9876543210123456789, 2026-11-07): 2 files, 19.3 KiB",
    ]);
    // A second depot may list the same folder, but not the same file.
    let shared = depot::parse_manifest(&manifest_for_depot(98), &HashMap::from([(98, KEY)]), &mut Vec::new());
    let mut shared = shared.unwrap().unwrap();
    shared.entries.push(depot::Entry { name: "empty-directory".into(), size: 0, flags: 64, chunks: Vec::new() });
    package.depots.push(shared);

    let (broken, good) = (serve(&fixture.responses, true), serve(&fixture.responses, false));
    let servers = [broken.url.clone(), good.url.clone()];
    let agent = download::agent(2);
    let output = temp.0.join("out");
    fs::create_dir(&output).unwrap();
    let run = || download::download(&package, &output, 2, &agent, &servers, None, &Progress::default()).unwrap();
    let target = output.join("folder/game.bin");
    let requests = || good.requests.lock().unwrap().len();

    let summary = run();
    assert_eq!((summary.files, summary.fetched, summary.reused), (2, 3, 0));
    assert_eq!(fs::read(&target).unwrap(), fixture.data);
    assert!(output.join("empty-directory").is_dir());
    assert_eq!(fs::read(output.join("empty-file")).unwrap(), b"");
    assert!(!broken.requests.lock().unwrap().is_empty());
    #[cfg(unix)]
    assert_eq!(std::os::unix::fs::PermissionsExt::mode(&fs::metadata(&target).unwrap().permissions()) & 0o777, 0o755);

    // A complete download is verified from disk without any requests.
    let before = requests();
    assert_eq!(run().reused, 3);
    assert_eq!(requests(), before);

    // Damage inside one chunk: only that chunk is fetched again.
    let mut damaged = fixture.data.clone();
    damaged[fixture.parts[0].len() + 10] ^= 1;
    fs::write(&target, &damaged).unwrap();
    let progress = Progress::default();
    let summary = download::download(&package, &output, 2, &agent, &servers, None, &progress).unwrap();
    assert_eq!((summary.fetched, summary.reused), (1, 2));
    assert_eq!(fs::read(&target).unwrap(), fixture.data);
    // Speed counts network bytes; time left counts content that had to be fetched.
    assert_eq!(progress.done.load(Relaxed), fixture.data.len() as u64);
    assert_eq!(progress.fetched.load(Relaxed), fixture.parts[1].len() as u64);
    let response = &fixture.responses[&hex(&Sha1::digest(&fixture.parts[1]))];
    assert_eq!(progress.received.load(Relaxed), response.len() as u64);
    let last = progress.lines(0).pop().unwrap();
    assert!(last.contains("on average; 2 of 3 chunks were already on disk"), "{last}");

    // An interrupted file keeps its complete chunks; extra bytes are cut off.
    fs::write(&target, &fixture.data[..fixture.parts[0].len() + 100]).unwrap();
    assert_eq!(run().fetched, 2);
    assert_eq!(fs::read(&target).unwrap(), fixture.data);
    fs::write(&target, [&fixture.data[..], b"junk"].concat()).unwrap();
    assert_eq!(run().fetched, 0);
    assert_eq!(fs::read(&target).unwrap(), fixture.data);

    package.depots[1].entries.push(depot::Entry { name: "empty-file".into(), size: 0, flags: 0, chunks: Vec::new() });
    let error = download::download(&package, &output, 2, &agent, &servers, None, &Progress::default()).unwrap_err();
    assert!(error.to_string().contains("is in both depot 123 and depot 98"), "{error}");
    // Leaving one of the two depots out avoids the clash.
    let only = package.select(|id| id == 123);
    assert_eq!(only.depots.len(), 1);
    download::download(&only, &output, 2, &agent, &servers, None, &Progress::default()).unwrap();
}

fn manifest_for_depot(depot: u64) -> Vec<u8> {
    let mut data = manifest(&[], false);
    let metadata = int(1, 123);
    let at = data.windows(metadata.len()).position(|w| w == metadata.as_slice()).unwrap();
    data.splice(at..at + metadata.len(), int(1, depot)); // IDs below 128 keep the length.
    data
}

#[test]
fn failed_chunks_are_reported_and_retried_on_the_next_run() {
    let fixture = fixture();
    let temp = TempDir::new("failures");
    let package = depot::Package {
        depots: vec![depot::parse_manifest(&manifest(&[fixture.mapping(b"game.bin")], false),
                                           &HashMap::from([(123, KEY)]), &mut Vec::new()).unwrap().unwrap()],
        ..Default::default()
    };
    let mut partial = fixture.responses.clone();
    partial.remove(&hex(&Sha1::digest(&fixture.parts[1])));
    let incomplete = serve(&partial, false);
    let agent = download::agent(1);
    let progress = Progress::default();
    let error = download::download(&package, &temp.0, 1, &agent, &[incomplete.url], None, &progress).unwrap_err();
    assert!(error.to_string().contains("1 chunks could not be downloaded"), "{error}");
    assert_eq!(progress.done.load(std::sync::atomic::Ordering::Relaxed), 8000 + 6400);

    // Pausing before anything happens leaves the run unfinished but not failed.
    let good = serve(&fixture.responses, false);
    let servers = [good.url.clone()];
    let paused = Progress::default();
    paused.stop.store(true, Relaxed);
    let summary = download::download(&package, &temp.0, 1, &agent, &servers, None, &paused).unwrap();
    assert!(summary.paused && summary.fetched == 0);
    assert!(paused.lines(0).pop().unwrap().starts_with("Paused"));

    let summary = download::download(&package, &temp.0, 1, &agent, &servers, None, &Progress::default()).unwrap();
    assert_eq!((summary.fetched, summary.reused, summary.paused), (1, 2, false));
    assert_eq!(fs::read(temp.0.join("game.bin")).unwrap(), fixture.data);
}

#[cfg(unix)]
#[test]
fn refuses_to_write_through_symlinks() {
    let fixture = fixture();
    let temp = TempDir::new("symlink");
    let outside = temp.0.join("outside");
    let output = temp.0.join("out");
    fs::create_dir_all(&outside).unwrap();
    fs::create_dir_all(&output).unwrap();
    std::os::unix::fs::symlink(&outside, output.join("link")).unwrap();
    let package = depot::Package {
        depots: vec![depot::parse_manifest(&manifest(&[fixture.mapping(b"link/game.bin")], false),
                                           &HashMap::from([(123, KEY)]), &mut Vec::new()).unwrap().unwrap()],
        ..Default::default()
    };
    let servers = [serve(&fixture.responses, false).url];
    let result = download::download(&package, &output, 1, &download::agent(1), &servers, None, &Progress::default());
    assert!(result.unwrap_err().to_string().contains("symbolic link"));
    assert!(fs::read_dir(&outside).unwrap().next().is_none());
}


#[test]
fn checks_what_is_already_downloaded_and_reuses_the_result() {
    use std::io::{Seek, SeekFrom};
    let fixture = fixture();
    let temp = TempDir::new("check");
    let package = depot::Package {
        depots: vec![depot::parse_manifest(&manifest(&[fixture.mapping(b"game.bin")], false),
                                           &HashMap::from([(123, KEY)]), &mut Vec::new()).unwrap().unwrap()],
        ..Default::default()
    };
    let check = |previous: Option<&download::OnDisk>| {
        let progress = Progress::default();
        let on_disk = download::check(&package, &temp.0, previous, &progress).unwrap().unwrap();
        assert_eq!(progress.total.load(Relaxed), fixture.data.len() as u64);
        (on_disk, progress.done.load(Relaxed))
    };

    // Nothing downloaded yet, and checking creates nothing.
    assert_eq!(check(None).1, 0);
    assert!(fs::read_dir(&temp.0).unwrap().next().is_none());

    // An interrupted download: the first chunk and part of the second.
    let target = temp.0.join("game.bin");
    let first = fixture.parts[0].len() as u64;
    fs::write(&target, &fixture.data[..first as usize + 100]).unwrap();
    let (on_disk, done) = check(None);
    assert_eq!(done, first);

    // A file that looks unchanged (same length and time) is not read again,
    // so damage hidden that way goes unnoticed until a fresh check.
    let mut file = fs::OpenOptions::new().write(true).open(&target).unwrap();
    let modified = file.metadata().unwrap().modified().unwrap();
    file.seek(SeekFrom::Start(10)).unwrap();
    file.write_all(b"!").unwrap();
    file.set_modified(modified).unwrap();
    assert_eq!(check(Some(&on_disk)).1, first);
    assert_eq!(check(None).1, 0);
    // A record is only trusted for the depot it was made for: another depot
    // may have a different file at the same path.
    let mut other = package.clone();
    other.depots[0].id = 99;
    let progress = Progress::default();
    download::check(&other, &temp.0, Some(&on_disk), &progress).unwrap();
    assert_eq!(progress.done.load(Relaxed), 0);
    // A changed file is read again.
    file.seek(SeekFrom::Start(10)).unwrap();
    file.write_all(&fixture.data[10..11]).unwrap();
    file.set_modified(modified + Duration::from_secs(1)).unwrap();
    drop(file);
    let (on_disk, done) = check(Some(&on_disk));
    assert_eq!(done, first);

    // The download takes over the check, and leaves a record of its own.
    let server = serve(&fixture.responses, false);
    let agent = download::agent(1);
    let summary = download::download(&package, &temp.0, 1, &agent, std::slice::from_ref(&server.url), Some(&on_disk),
                                     &Progress::default()).unwrap();
    assert_eq!((summary.fetched, summary.reused), (2, 1));
    assert_eq!(fs::read(&target).unwrap(), fixture.data);
    assert_eq!(check(summary.on_disk.as_ref()).1, fixture.data.len() as u64);
}
