//! Offline tests: formats, path safety, and full downloads against local servers.

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Cursor, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use aes::cipher::block_padding::Pkcs7;
use aes::cipher::{BlockCipherEncrypt, BlockModeEncrypt, KeyInit, KeyIvInit};
use sha1::{Digest, Sha1};

use crate::depot::{self, Key};
use crate::download::{self, Meter, Progress, Rates};
use crate::{installed, update};

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
        let path = std::env::temp_dir().join(format!("trashpanda-test-{}-{name}", std::process::id()));
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

/// A local HTTP server, and the paths requested from it.
struct Server {
    url: String,
    requests: Arc<Mutex<Vec<String>>>,
}

/// What a test server answers a request with.
enum Reply {
    Body(&'static str, Vec<u8>),
    Redirect(String),
}

/// A server serving `responses` by chunk SHA-1, or 503 for everything.
fn serve(responses: &HashMap<String, Vec<u8>>, broken: bool) -> Server {
    let responses = responses.clone();
    serve_with(move |path| match responses.get(path.rsplit('/').next().unwrap()).filter(|_| !broken) {
        Some(body) => Reply::Body("200 OK", body.clone()),
        None => Reply::Body("503 Unavailable", Vec::new()),
    })
}

/// A server answering each request path with `reply`.
fn serve_with(reply: impl Fn(&str) -> Reply + Send + Sync + 'static) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (reply, log) = (Arc::new(reply), requests.clone());
    thread::spawn(move || {
        for stream in listener.incoming() {
            let (mut stream, reply, log) = (stream.unwrap(), reply.clone(), log.clone());
            thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                reader.read_line(&mut request).unwrap();
                let mut header = String::new();
                while reader.read_line(&mut header).unwrap() > 2 {
                    header.clear();
                }
                let path = request.split_whitespace().nth(1).unwrap_or("").to_string();
                let (status, location, body) = match reply(&path) {
                    Reply::Body(status, body) => (status, String::new(), body),
                    Reply::Redirect(to) => ("302 Found", format!("Location: {to}\r\n"), Vec::new()),
                };
                log.lock().unwrap().push(path);
                write!(stream, "HTTP/1.1 {status}\r\n{location}Content-Length: {}\r\nConnection: close\r\n\r\n",
                       body.len()).unwrap();
                stream.write_all(&body).unwrap();
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
fn gives_people_short_errors() {
    use anyhow::Context;
    let missing = fs::read(std::env::temp_dir().join("trashpanda-test-missing")).context("could not read the package");
    assert_eq!(crate::log::brief(&missing.unwrap_err()), "Could not read the package: it does not exist");
    let refused = anyhow::Error::from(ureq::Error::StatusCode(404)).context("giving up after 20 chunks failed");
    assert_eq!(crate::log::brief(&refused), "Giving up after 20 chunks failed: the server answered with error 404");
    let plain = anyhow::anyhow!("the package is not a zip file");
    assert_eq!(crate::log::brief(&plain), "The package is not a zip file");
}

#[test]
fn keeps_the_log_small() {
    let mut log = crate::log::Log::default();
    for n in 0..100 {
        log.push(n.to_string(), 40);
    }
    // Past 40 lines, ten at a time are dropped after the first four.
    let lines = log.lines();
    assert_eq!(lines[..4], ["0", "1", "2", "3"]);
    assert_eq!(lines[4].trim(), "(60 lines dropped here to keep the log small)");
    assert_eq!(lines[5..], (64..100).map(|n| n.to_string()).collect::<Vec<_>>());
}

#[test]
fn keeps_keys_out_of_the_log() {
    let key = "4ee20016788a1b4d0a2fca7538249558728be0b90e5c064c412a68dfa2c541e7";
    let line = format!("addappid(2862861, 1, \"{key}\") -- Depot 2862861");
    assert_eq!(crate::log::redact_keys(&line), "addappid(2862861, 1, \"<key>\") -- Depot 2862861");
    let manifest = "setManifestid(2862861, \"6952845047153854805\", 32501641497)";
    assert_eq!(crate::log::redact_keys(manifest), manifest);
    assert_eq!(crate::log::clip("née", 2), "n");
}

#[test]
fn formats_dates() {
    for (time, date) in [(0, "1970-01-01"), (951_782_400, "2000-02-29"), (1_794_069_781, "2026-11-07")] {
        assert_eq!(download::date(time), date);
    }
    assert_eq!(download::datetime(1_794_069_781), "2026-11-07 16:43:01 UTC");
}

#[test]
fn rejects_unsafe_paths() {
    for name in ["../outside", "/outside", "C:/outside", "x/../../outside", "", ".", "x\0y", "a//b", "a/. ./b",
                 "x\\..\\..\\outside"] {
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
    let logged = crate::log::lines().join("\n");
    assert!(logged.contains("lua| addappid(123, 1, \"<key>\") -- Depot 123"), "{logged}");
    assert!(!logged.contains(&hex(&KEY)), "the log must not hold depot keys");
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
    assert!(error.to_string().contains("depots 123 and 98 both contain \"empty-file\""), "{error}");
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

/// A server that takes every connection and request and never answers, like
/// a content server that has hung.
fn silent() -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let log = requests.clone();
    thread::spawn(move || {
        let mut open = Vec::new();
        for stream in listener.incoming() {
            log.lock().unwrap().push(String::new());
            open.push(stream.unwrap()); // Held open, never answered.
        }
    });
    Server { url, requests }
}

#[test]
fn a_server_that_never_answers_holds_nothing_up() {
    let fixture = fixture();
    let temp = TempDir::new("silent");
    let package = depot::Package {
        depots: vec![depot::parse_manifest(&manifest(&[fixture.mapping(b"game.bin")], false),
                                           &HashMap::from([(123, KEY)]), &mut Vec::new()).unwrap().unwrap()],
        ..Default::default()
    };
    let (silent, good) = (silent(), serve(&fixture.responses, false));
    let agent = download::agent(1);

    // The download gives up on it and moves on to the next server.
    let started = Instant::now();
    let servers = [silent.url.clone(), good.url.clone()];
    let summary = download::download(&package, &temp.0, 1, &agent, &servers, None, &Progress::default()).unwrap();
    assert_eq!(summary.fetched, 3);
    assert_eq!(silent.requests.lock().unwrap().len(), 1);
    assert!(started.elapsed() < Duration::from_secs(5), "took {:?}", started.elapsed());

    // Pausing while a request hangs does not wait minutes for it.
    fs::remove_file(temp.0.join("game.bin")).unwrap();
    let (progress, servers) = (Progress::default(), [silent.url.clone()]);
    let started = Instant::now();
    let summary = thread::scope(|scope| {
        let run = scope.spawn(|| download::download(&package, &temp.0, 1, &agent, &servers, None, &progress));
        thread::sleep(Duration::from_millis(100));
        progress.stop.store(true, Relaxed);
        run.join().unwrap().unwrap()
    });
    assert!(summary.paused && summary.fetched == 0);
    assert!(started.elapsed() < Duration::from_secs(5), "took {:?}", started.elapsed());
}

/// A depot of `files`, each made of `parts`, one chunk per part, adding what
/// a server answers for those chunks to `responses`.
fn depot_of(id: u32, files: &[(&str, &[&[u8]])], responses: &mut HashMap<String, Vec<u8>>) -> depot::Depot {
    let mappings: Vec<Vec<u8>> = files.iter().map(|(name, parts)| {
        let size: usize = parts.iter().map(|part| part.len()).sum();
        let mut mapping = [bytes(1, name.as_bytes()), int(2, size as u64), int(3, 0)].concat();
        let mut offset = 0;
        for part in *parts {
            let sha = Sha1::digest(part);
            mapping.extend(bytes(6, &[bytes(1, &sha), int(3, offset), int(4, part.len() as u64)].concat()));
            responses.insert(hex(&sha), encrypt(&compressed(part, "zstd")));
            offset += part.len() as u64;
        }
        mapping
    }).collect();
    let keys = HashMap::from([(123, KEY)]);
    let mut depot = depot::parse_manifest(&manifest(&mappings, false), &keys, &mut Vec::new()).unwrap().unwrap();
    depot.id = id;
    depot
}

/// How many files a download said it removed.
fn removed(progress: &Progress) -> usize {
    let said = progress.lines(0);
    said.iter().find_map(|line| line.strip_prefix("Removed ")?.split(' ').next()?.parse().ok()).unwrap_or(0)
}

fn listed(output: &Path) -> Vec<(u32, Vec<String>)> {
    let installed = installed::Installed::load(output).unwrap();
    installed.depots.into_iter().map(|(id, files)| (id, files.into_iter().collect())).collect()
}

#[test]
fn updates_in_place_and_removes_what_the_new_version_dropped() {
    let temp = TempDir::new("game-update");
    let out = &temp.0;
    let part = |text: &str, count: usize| text.repeat(count).into_bytes();
    let (a, b, b2, c, f, g) = (part("aaaa\n", 900), part("bbbb\n", 700), part("BBBB\n", 700), part("cccc\n", 500),
                               part("ffff\n", 400), part("gggg\n", 450));
    let (new, readme, stale, gone) = (part("new\n", 200), part("read me\n", 50), part("old\n", 300), part("gone\n", 100));
    let (fr, voice, subtitles) = (part("bonjour\n", 80), part("voix\n", 600), part("sous-titres\n", 90));
    let mut responses = HashMap::new();
    let game1 = depot_of(123, &[("game.bin", &[&a, &b, &c]), ("shrunk.bin", &[&f, &g]), ("Readme.txt", &[&readme]),
                                ("old/stale.bin", &[&stale]), ("gone/away.bin", &[&gone])], &mut responses);
    let french1 = depot_of(124, &[("lang/fr.txt", &[&fr]), ("lang/fr-voice.bin", &[&voice])], &mut responses);
    // The next version changes one chunk, shortens a file, renames one by
    // case, drops two, adds one, and moves fr.txt from the French depot.
    let game2 = depot_of(123, &[("game.bin", &[&a, &b2, &c]), ("shrunk.bin", &[&f]), ("readme.txt", &[&readme]),
                                ("new.bin", &[&new]), ("lang/fr.txt", &[&fr])], &mut responses);
    let french2 = depot_of(124, &[("lang/fr-subtitles.txt", &[&subtitles])], &mut responses);
    // And then fr.txt moves back.
    let game3 = depot_of(123, &[("game.bin", &[&a, &b2, &c]), ("shrunk.bin", &[&f]), ("readme.txt", &[&readme]),
                                ("new.bin", &[&new])], &mut responses);
    let french3 = depot_of(124, &[("lang/fr.txt", &[&fr]), ("lang/fr-subtitles.txt", &[&subtitles])], &mut responses);
    let server = serve(&responses, false);
    let (agent, servers) = (download::agent(2), [server.url.clone()]);
    let run = |depots: Vec<depot::Depot>| {
        let (package, progress) = (depot::Package { depots, ..Default::default() }, Progress::default());
        let summary = download::download(&package, out, 2, &agent, &servers, None, &progress).unwrap();
        (summary.fetched, summary.reused, removed(&progress))
    };
    let read = |name: &str| fs::read(out.join(name)).unwrap();
    let exists = |name: &str| out.join(name).exists();

    // The player's own files, before and after the first download.
    fs::create_dir_all(out.join("saves")).unwrap();
    fs::write(out.join("saves/slot1.sav"), "mine").unwrap();
    assert_eq!(run(vec![game1, french1]), (10, 0, 0));
    fs::write(out.join("old/notes.txt"), "mine too").unwrap();
    let game1_files = ["Readme.txt", "game.bin", "gone/away.bin", "old/stale.bin", "shrunk.bin"];
    assert_eq!(listed(out), [(123, game1_files.map(String::from).to_vec()),
                             (124, vec!["lang/fr-voice.bin".into(), "lang/fr.txt".into()])]);

    // The new game depot alone, the French one left unticked: only the
    // changed and new chunks are fetched, and what it dropped is removed.
    assert_eq!(run(vec![game2.clone()]), (3, 4, 3));
    assert_eq!((read("game.bin"), read("shrunk.bin"), read("new.bin")), ([&a[..], &b2, &c].concat(), f.clone(), new));
    assert_eq!(read("readme.txt"), readme);
    assert!(!exists("Readme.txt") && !exists("old/stale.bin") && !exists("gone"), "the folder emptied goes too");
    assert_eq!((read("old/notes.txt"), read("saves/slot1.sav")), (b"mine too".to_vec(), b"mine".to_vec()));
    assert_eq!(read("lang/fr-voice.bin"), voice, "a depot not downloaded keeps its files");
    let game2_files = ["game.bin", "lang/fr.txt", "new.bin", "readme.txt", "shrunk.bin"].map(String::from).to_vec();
    assert_eq!(listed(out)[0], (123, game2_files.clone()));

    // Both: the French depot's dropped file goes, but not the one it gave the game.
    let (fetched, _, removed) = run(vec![game2, french2]);
    assert_eq!((fetched, removed), (1, 1));
    assert!(!exists("lang/fr-voice.bin"));
    assert_eq!((read("lang/fr.txt"), read("lang/fr-subtitles.txt")), (fr.clone(), subtitles));
    assert_eq!(listed(out), [(123, game2_files), (124, vec!["lang/fr-subtitles.txt".into()])]);

    // Each depot alone: the French one takes fr.txt back, so the game
    // dropping it must leave it, as the French depot is not downloaded then.
    assert_eq!(run(vec![french3]), (0, 2, 0));
    assert_eq!(run(vec![game3]), (0, 6, 0));
    assert_eq!(read("lang/fr.txt"), fr);
}

/// Makes a folder writable again when dropped, so that a test that fails
/// while it is read-only still cleans up.
#[cfg(unix)]
struct Unlock(PathBuf);

#[cfg(unix)]
impl Drop for Unlock {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
    }
}

#[test]
fn an_edited_or_damaged_list_removes_nothing_it_should_not() {
    let fixture = fixture();
    let temp = TempDir::new("list");
    let out = temp.0.join("out");
    fs::create_dir_all(out.join("folder")).unwrap();
    fs::create_dir_all(temp.0.join("outside")).unwrap();
    fs::write(temp.0.join("outside.txt"), "keep").unwrap();
    fs::write(temp.0.join("outside/a.txt"), "keep").unwrap();
    let package = depot::Package {
        depots: vec![depot::parse_manifest(&manifest(&[fixture.mapping(b"game.bin")], false),
                                           &HashMap::from([(123, KEY)]), &mut Vec::new()).unwrap().unwrap()],
        ..Default::default()
    };
    let server = serve(&fixture.responses, false);
    let (agent, servers) = (download::agent(1), [server.url.clone()]);
    let run = || {
        let progress = Progress::default();
        let summary = download::download(&package, &out, 1, &agent, &servers, None, &progress).unwrap();
        assert_eq!(summary.fetched + summary.reused, 3);
        (removed(&progress), progress.lines(0).join("\n"))
    };
    let mut names = vec!["../outside.txt".to_string(), temp.0.join("outside.txt").display().to_string(),
                         "folder\\..\\..\\outside.txt".into(), "folder".into()];
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(temp.0.join("outside"), out.join("link")).unwrap();
        names.push("link/a.txt".into());
        // A file that cannot be removed now stays listed, to be tried again.
        use std::os::unix::fs::PermissionsExt;
        fs::create_dir(out.join("locked")).unwrap();
        fs::write(out.join("locked/x.bin"), "old").unwrap();
        fs::set_permissions(out.join("locked"), fs::Permissions::from_mode(0o555)).unwrap();
        names.push("locked/x.bin".into());
    }
    #[cfg(unix)]
    let _unlock = Unlock(out.join("locked"));
    fs::write(out.join(installed::FILE), serde_json::json!({ "depots": { "123": names } }).to_string()).unwrap();

    let (removed, said) = run();
    assert!(said.contains("Not removing \"../outside.txt\""), "{said}");
    assert_eq!(fs::read(temp.0.join("outside.txt")).unwrap(), b"keep");
    assert_eq!(fs::read(temp.0.join("outside/a.txt")).unwrap(), b"keep");
    assert!(out.join("folder").is_dir());
    #[cfg(unix)]
    {
        let stuck = out.join("locked/x.bin").exists(); // Removable all the same when tests run as root.
        assert_eq!(removed, usize::from(!stuck));
        let expected: Vec<String> = ["game.bin", "locked/x.bin"].into_iter().filter(|name| stuck || *name == "game.bin")
            .map(String::from).collect();
        assert_eq!(listed(&out), [(123, expected)]);
    }
    #[cfg(not(unix))]
    assert_eq!((removed, listed(&out)), (0, vec![(123, vec!["game.bin".to_string()])]));

    // A damaged list is replaced, removing nothing.
    fs::write(out.join(installed::FILE), "{ not json").unwrap();
    let (removed, said) = run();
    assert_eq!(removed, 0);
    assert!(said.contains("is damaged"), "{said}");
    assert_eq!(listed(&out), [(123, vec!["game.bin".to_string()])]);
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

/// A throwaway key pair made for these tests with `minisign -G -W`; its secret
/// half is gone. The signatures were made with minisign 0.12, for example
/// `minisign -S -m new -t "trashpanda 0.3.0 linux-x86_64"`, as `release.sh` does.
const TEST_KEY: &str = "untrusted comment: minisign public key DF451113307EDA6E
RWRu2n4wExFF30L2IgjCZhJtN6HAAyXjwQ2C6sp//k5sagAycmQAOVzz
";
const NEW_BUILD: &[u8] = b"new build\n";
/// `NEW_BUILD`, signed as trashpanda 0.3.0 for linux-x86_64.
const NEW_SIGNATURE: &str = "untrusted comment: signature from minisign secret key
RURu2n4wExFF3581XhIiZtnSuYZ6DVM8tfXptNWvgEy7VOYBCHGCJvu7fbvtKeCo74Q3iS6x8OWOvC4l61Ajh1+BMIaFBAUuVQA=
trusted comment: trashpanda 0.3.0 linux-x86_64
8LFnWuWr+TciEqxVU4rb2++mEBCsEvQt9iFHLVLX7XaWYbB50mtzMjzGaj25L9+eZn/q0a4bA5LQu6DGtf+nDw==
";
const OLD_BUILD: &[u8] = b"old build\n";
/// `OLD_BUILD`, signed as trashpanda 0.1.0 for linux-x86_64.
const OLD_SIGNATURE: &str = "untrusted comment: signature from minisign secret key
RURu2n4wExFF3x3UBWBAfA2sNZ2VJ1O09ruVwcE2MrM4OyepbxJeYMud2Ki/HWz4wdBEzBzQT9IlShaRjg7y5MUBqVsyjMSZzQA=
trusted comment: trashpanda 0.1.0 linux-x86_64
Azo82RRrRwWSJTLG94b/WDqMM4jZefMr3zED3JufSvwCPY1KsllRPWT+zINgJm1nI+lhXsxTtZYJEE+yVz7pCQ==
";
/// `NEW_BUILD`, signed as trashpanda 0.3.0 for linux-x86_64 with another key.
const FOREIGN_SIGNATURE: &str = "untrusted comment: signature from minisign secret key
RURc1oXKTT6tUZMiWTKWH3KK3KGrSN851gVeGfVTqTIwWgzTM2RoOmW0bNpeZ8N9ChvGI1H2p5Io8u8c1j6VlH5xtOM5EEXukgc=
trusted comment: trashpanda 0.3.0 linux-x86_64
N5esGj9W7dtNHLB7Y4i/JRlOozDAKKgfJSPqiq3CUYh5bo4o4mW6DIBt4xdxyNHKuhCmyUxg8J2mote8W4xBCg==
";
const LINUX: &str = "trashpanda-linux-x86_64";
const WINDOWS: &str = "trashpanda-windows-x86_64.exe";

fn latest(version: &str) -> String {
    format!(r#"{{"version": "{version}", "assets": {{"linux-x86_64": "{LINUX}", "windows-x86_64": "{WINDOWS}"}}}}"#)
}

/// A stand-in for GitHub Releases: `latest/download/` redirects to the
/// release `tag`, which holds `files`.
fn releases(tag: &str, files: &[(&str, &[u8])]) -> Server {
    let prefix = format!("/releases/download/{tag}/");
    let files: HashMap<String, Vec<u8>> =
        files.iter().map(|(name, data)| (format!("{prefix}{name}"), data.to_vec())).collect();
    serve_with(move |path| match path.strip_prefix("/releases/latest/download/") {
        Some(name) => Reply::Redirect(format!("{prefix}{name}")),
        None => files.get(path).map_or(Reply::Body("404 Not Found", Vec::new()), |data| Reply::Body("200 OK", data.clone())),
    })
}

/// Release 0.3.0 with `build` and `signature` as the build for `asset`.
fn release_with(asset: &str, build: &[u8], signature: &str) -> Server {
    releases("v0.3.0", &[("latest.json", latest("0.3.0").as_bytes()), (asset, build),
                         (&format!("{asset}.minisig"), signature.as_bytes())])
}

fn updater(server: &Server, platform: &str, current: &str) -> update::Updater {
    let url = format!("{}/releases", server.url);
    update::Updater::with(&url, platform, current.parse().unwrap(), TEST_KEY).unwrap()
}

/// Install `release` over a stand-in for the program at `exe`, copying it into
/// place as `self_replace` does.
fn install(updater: &update::Updater, release: &update::Release, exe: &Path) -> anyhow::Result<()> {
    updater.install_at(release, exe, |staged| {
        #[cfg(unix)]
        assert_eq!(std::os::unix::fs::PermissionsExt::mode(&fs::metadata(staged)?.permissions()) & 0o777, 0o755);
        fs::copy(staged, exe).map(drop)
    })
}

/// Whether an update left a build it downloaded behind in `dir`.
fn staged_left(dir: &Path) -> bool {
    fs::read_dir(dir).unwrap().any(|entry| entry.unwrap().file_name().to_string_lossy().starts_with(update::STAGED))
}

/// Offer release 0.3.0 to a 0.2.0 build for `platform`, serving `build` and
/// `signature` for it, and return why installing it failed. The program
/// must be left as it was. `name` names the scratch directory.
fn refusal(name: &str, platform: &str, build: &[u8], signature: &str) -> String {
    let asset = if platform.starts_with("windows") { WINDOWS } else { LINUX };
    let server = release_with(asset, build, signature);
    let updater = updater(&server, platform, "0.2.0");
    let release = updater.check().unwrap().expect("0.3.0 is offered");
    let temp = TempDir::new(name);
    let exe = temp.0.join("trashpanda");
    fs::write(&exe, OLD_BUILD).unwrap();
    let error = install(&updater, &release, &exe).unwrap_err();
    assert_eq!(fs::read(&exe).unwrap(), OLD_BUILD);
    assert!(!staged_left(&temp.0));
    format!("{error:#}")
}

#[test]
fn compares_versions_by_number() {
    let version = |text: &str| text.parse::<update::Version>().unwrap();
    assert!(version("0.10.0") > version("0.9.9"));
    assert!(version("1.0.0") > version("0.99.99"));
    assert!(version("0.2.1") > version("0.2.0"));
    assert_eq!(version("1.20.3").to_string(), "1.20.3");
    for bad in ["", "1.2", "1.2.3.4", "v1.2.3", "1.2.x", "1..2", "1.2.+3", "1.2.3-beta"] {
        assert!(bad.parse::<update::Version>().is_err(), "{bad:?}");
    }
    // The key in minisign.pub and this build's version are usable.
    update::Updater::new();
    update::current();
}

#[test]
fn reads_latest_json() {
    let server = releases("v0.3.0", &[]);
    let linux = updater(&server, "linux-x86_64", "0.2.0");
    let release = linux.parse_latest(&latest("0.3.0")).unwrap().unwrap();
    assert_eq!(release.version.to_string(), "0.3.0");
    assert_eq!(release.page, format!("{}/releases/tag/v0.3.0", server.url));
    // A release that is not newer is not offered, whatever else it holds.
    assert!(linux.parse_latest(&latest("0.2.0")).unwrap().is_none());
    assert!(linux.parse_latest(r#"{"version": "0.1.0"}"#).unwrap().is_none());
    let error = updater(&server, "macos-aarch64", "0.2.0").parse_latest(&latest("0.3.0")).unwrap_err();
    assert!(error.to_string().contains("no build for macos-aarch64"), "{error}");
    for bad in ["", "{}", r#"{"version": "0.3"}"#, r#"{"version": 3}"#,
                r#"{"version": "0.3.0", "assets": {"linux-x86_64": "../trashpanda"}}"#,
                r#"{"version": "0.3.0", "assets": {"linux-x86_64": ".hidden"}}"#,
                r#"{"version": "0.3.0", "assets": {"linux-x86_64": "a?b"}}"#] {
        assert!(linux.parse_latest(bad).is_err(), "{bad}");
    }
}

#[test]
fn installs_a_signed_newer_build() {
    let server = release_with(LINUX, NEW_BUILD, NEW_SIGNATURE);
    let updater = updater(&server, "linux-x86_64", "0.2.0");
    let release = updater.check().unwrap().expect("0.3.0 is newer");
    assert_eq!(release.version.to_string(), "0.3.0");
    let temp = TempDir::new("update");
    let exe = temp.0.join("trashpanda");
    fs::write(&exe, OLD_BUILD).unwrap();
    install(&updater, &release, &exe).unwrap();
    assert_eq!(fs::read(&exe).unwrap(), NEW_BUILD);
    assert!(!staged_left(&temp.0));
    // latest.json is found through the redirect, the build in the release it names.
    assert_eq!(*server.requests.lock().unwrap(), [
        "/releases/latest/download/latest.json",
        "/releases/download/v0.3.0/latest.json",
        "/releases/download/v0.3.0/trashpanda-linux-x86_64",
        "/releases/download/v0.3.0/trashpanda-linux-x86_64.minisig",
    ]);
    // Once on 0.3.0, there is nothing to offer.
    assert!(self::updater(&server, "linux-x86_64", "0.3.0").check().unwrap().is_none());
    // An unreachable server is an error, not "up to date".
    let gone = update::Updater::with("http://127.0.0.1:9/releases", "linux-x86_64", update::current(), TEST_KEY);
    assert!(gone.unwrap().check().is_err());
}

#[test]
fn a_second_update_cannot_spoil_the_first_ones_build() {
    let server = release_with(LINUX, NEW_BUILD, NEW_SIGNATURE);
    let updater = updater(&server, "linux-x86_64", "0.2.0");
    let release = updater.check().unwrap().unwrap();
    let temp = TempDir::new("two-updates");
    let exe = temp.0.join("trashpanda");
    fs::write(&exe, OLD_BUILD).unwrap();
    // A second update starts after the first has checked its build, and
    // fails to download its own.
    let gone = update::Updater::with("http://127.0.0.1:9/releases", "linux-x86_64", "0.2.0".parse().unwrap(), TEST_KEY);
    updater.install_at(&release, &exe, |staged| {
        assert!(gone.unwrap().install_at(&release, &exe, |_| unreachable!()).is_err());
        fs::copy(staged, &exe).map(drop)
    }).unwrap();
    assert_eq!(fs::read(&exe).unwrap(), NEW_BUILD);
    assert!(!staged_left(&temp.0));
}

#[test]
fn refuses_builds_without_a_good_signature() {
    let mut tampered = NEW_BUILD.to_vec();
    tampered[0] ^= 1;
    for (name, build, signature, reason) in [
        ("tampered", &tampered[..], NEW_SIGNATURE, "not signed with the trashpanda release key"),
        ("foreign", NEW_BUILD, FOREIGN_SIGNATURE, "not signed with the trashpanda release key"),
        ("garbled", NEW_BUILD, "not a signature", "unreadable signature"),
        ("unsigned", NEW_BUILD, "", "unreadable signature"),
    ] {
        let error = refusal(name, "linux-x86_64", build, signature);
        assert!(error.contains(reason) && error.contains("refusing the download of version 0.3.0"), "{error}");
    }
}

#[test]
fn refuses_downgrades() {
    // An older release is not offered.
    let server = releases("v0.1.0", &[("latest.json", latest("0.1.0").as_bytes()), (LINUX, OLD_BUILD),
                                      (&format!("{LINUX}.minisig"), OLD_SIGNATURE.as_bytes())]);
    assert!(updater(&server, "linux-x86_64", "0.2.0").check().unwrap().is_none());
    // An old build, properly signed, served as the new version.
    let error = refusal("downgrade", "linux-x86_64", OLD_BUILD, OLD_SIGNATURE);
    assert!(error.contains("it is signed as version 0.1.0"), "{error}");
    // A signed build that is not newer than the running one.
    let server = release_with(LINUX, NEW_BUILD, NEW_SIGNATURE);
    let release = updater(&server, "linux-x86_64", "0.2.0").check().unwrap().unwrap();
    let error = updater(&server, "linux-x86_64", "0.3.0").download(&release).unwrap_err();
    assert!(error.to_string().contains("it is not newer than this version, 0.3.0"), "{error}");
}

#[test]
fn refuses_builds_for_another_platform() {
    // The Linux build, uploaded as the Windows one.
    let error = refusal("platform", "windows-x86_64", NEW_BUILD, NEW_SIGNATURE);
    assert!(error.contains("it is signed as a build for linux-x86_64, not windows-x86_64"), "{error}");
}

#[cfg(unix)]
#[test]
fn sends_users_to_the_release_page_when_the_folder_is_read_only() {
    use std::os::unix::fs::PermissionsExt;
    let server = release_with(LINUX, NEW_BUILD, NEW_SIGNATURE);
    let updater = updater(&server, "linux-x86_64", "0.2.0");
    let release = updater.check().unwrap().unwrap();
    let temp = TempDir::new("read-only");
    let exe = temp.0.join("trashpanda");
    fs::write(&exe, OLD_BUILD).unwrap();
    fs::set_permissions(&temp.0, fs::Permissions::from_mode(0o555)).unwrap();
    let ignored = fs::File::create(temp.0.join("probe")).is_ok(); // Root ignores permissions.
    let result = (!ignored).then(|| install(&updater, &release, &exe));
    fs::set_permissions(&temp.0, fs::Permissions::from_mode(0o755)).unwrap();
    let Some(result) = result else { return };
    let error = result.unwrap_err();
    let unwritable = error.downcast_ref::<update::Unwritable>().expect("the folder is reported as read-only");
    assert_eq!(unwritable.page, format!("{}/releases/tag/v0.3.0", server.url));
    assert!(error.to_string().contains("download the new version by hand from http://"), "{error}");
    assert_eq!(fs::read(&exe).unwrap(), OLD_BUILD);
    // It gives up before downloading the build.
    assert!(!server.requests.lock().unwrap().iter().any(|path| path.contains(LINUX)));
}

#[test]
fn window_icon_is_a_square_png() {
    let icon = crate::gui::icon();
    assert_eq!((icon.width, icon.height), (256, 256));
    assert_eq!(icon.rgba.len(), 256 * 256 * 4);
}
