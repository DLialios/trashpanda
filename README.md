# Downloader

Background on how Steam delivers game content: how it is stored, and what a
download from Valve's CDN needs.

## How Steam stores game content

Steam organizes content in four layers:

- **App**: the game, identified by an app ID (Liftoff: `410340`).
- **Depot**: one bundle of the app's files, such as a platform build or a
  language pack, identified by a depot ID (`410341`). An app has one or more.
- **Manifest**: one exact snapshot of a depot, identified by a manifest ID
  (`8305800646910824302`). Every build that changes a depot gets a new
  manifest. The manifest *file* lists every file in the snapshot with its size
  and SHA-1, and every chunk with its SHA-1, offset and length. Its metadata
  section also records the depot ID and the manifest ID.
- **Chunk**: a piece of a file, stored on Valve's content servers at
  `depot/<depotid>/chunk/<sha1>`, where the SHA-1 is that of the chunk's
  decrypted, decompressed content.

### Chunk format

Chunks are AES-256 encrypted with the depot key: a 16-byte ECB-encrypted IV,
then an AES-CBC payload. The decrypted payload is compressed: `VSZa`, a CRC32,
a zstd frame, the decompressed size and `zsv`. Older content uses `VZa`/LZMA or
a zip container instead.

### Manifest format

A manifest saved by the Steam client in `depotcache/` is a plaintext protobuf.
One fetched from the CDN arrives ZIP-wrapped and normally has its file names
encrypted with the depot key. The chunk list is sorted by SHA-1 rather than
offset, so chunks must be written at their `offset` or the file hash fails.

## What a download needs

Strictly, two things: the **depot key** and the **manifest file**. Everything
else is either inside the manifest file or optional.

- **The manifest file supplies the rest.** The depot ID and manifest ID are in
  its metadata, and its chunk SHA-1s are the download URLs.
- **The key decrypts the chunks.** Without it every chunk is unreadable noise.
- **The app ID is not part of any CDN URL.** Its only use is choosing servers:
  some entries in the server list carry `allowed_app_ids` and serve only those
  apps.
- **Finding servers needs no login.** The public Web API
  (`IContentServerDirectoryService/GetServersForSteamPipe`) answers with
  `cell_id=0` and no session at all.

### The manifest ID is not the manifest

Chunk URLs are `depot/<depotid>/chunk/<sha1>`, and those SHA-1s exist only
inside the manifest file, so nothing can be downloaded without it. Steam's CDN
does serve manifests, at `depot/<depotid>/manifest/<manifestid>/5/<request code>`,
but the request code is a second gate. It comes from `GetManifestRequestCode` on
a logged-in Steam session, is issued only for apps that account has a license
for, and expires quickly. An anonymous session therefore cannot fetch the
manifest of a paid app; the anonymous account is licensed only for anonymously
downloadable apps such as dedicated servers.

### Why ownership matters, and where it stops mattering

Ownership gates two things, and both come before any content is downloaded:

1. **The depot key.** Asking Steam for it anonymously does **not** work —
   `ClientGetDepotDecryptionKey` returns `EResult 15 (AccessDenied)` — and
   current PICS appinfo carries no `decryptionkey` field. Keys are per depot and
   effectively never change.
2. **The manifest.** Fetching it needs a request code, which needs a license
   (see above). Manifests change with every build, so this gate comes back after
   every game update.

The chunks themselves are not gated. The CDN serves `depot/<depotid>/chunk/<sha1>`
to anonymous callers with no token or cookie and checks nothing against the
account; the encryption, plus not knowing the chunk SHA-1s without a manifest,
is the only protection.

### Where an installed copy keeps them

The Steam client keeps the key and the manifest only because the account owns
the app, so an owned install is what puts them on the machine:

| Item | Where it lives (Liftoff) |
| --- | --- |
| Depot key | `config/config.vdf`, the `depots` map |
| Manifest file | `steamapps/depotcache/410341_8305800646910824302.manifest` |
| App, depot and manifest IDs | `steamapps/appmanifest_410340.acf`: the `appid` line, `InstalledDepots`, and the `manifest` line |

**`config/config.vdf` typically contains the depot decryption key.** It holds a
`depots` map under `InstallConfigStore/Software/Valve/Steam/`, keyed by depot
ID, each with a `DecryptionKey` value:

```
"InstallConfigStore"
  "Software"
    "Valve"
      "Steam"
        "depots"
          "410341"
            "DecryptionKey"   "<64-hex-digit depot key>"
```

That map is a general cache, not a Liftoff special case: the local file held 136
keys for all sorts of depots. Once a game has been installed, its key is usually
sitting there in plain hex. Digging it out by hand is the fastest route:

```bash
grep -A2 '"410341"' ~/.local/share/Steam/config/config.vdf
```

The manifest is usually still in `depotcache/` after an install, under
`<depotid>_<manifestid>.manifest`.

## Usage

`rin-dl` downloads every depot in a manifest package into the current
directory. A package is a zip holding a `.lua` file with the depot keys
(`addappid(<depot>, 1, "<64 hex digits>")` lines) and the depots' `.manifest`
files, such as `2862860.zip`. It needs no Steam login: content servers come
from the public Web API.

### Build

Requires a stable [Rust](https://rustup.rs/) toolchain. Build on the system
that will run it:

```bash
cargo build --release   # target/release/rin-dl, or rin-dl.exe on Windows
```

On Windows, rustup's default MSVC toolchain is all that is needed. To build a
Windows `.exe` from Linux instead, install MinGW (`mingw-w64-gcc` on Arch),
then run `rustup target add x86_64-pc-windows-gnu` and
`cargo build --release --target x86_64-pc-windows-gnu`.

### Terminal

```bash
cd /where/the/game/goes
rin-dl ~/Downloads/2862860.zip
```

```
App 2862860: Ukrainian Fight Drone Simulator, build 26508517 (public branch)
Depot 2862861 (manifest 6952845047153854805, 2026-11-07): 543 files, 30.3 GiB
Depot 228990 (manifest 1829726630299308803, 2013-02-18): 158 files, 98.2 MiB - DirectX Jun 2010 Redist (Shared from App 228980)
Depot 228989 (manifest 5753583882400741046, 2026-06-30): 5 files, 24.5 MiB - VC 2022 Redist (Shared from App 228980)
Servers (Steam picks them by your location): cache13-iad1.steamcontent.com, …
Output: /where/the/game/goes
Already downloaded: 9.8 GiB of 30.4 GiB.
[########------------]  41.7%  12.7/30.4 GiB  5.9 MiB/s  21m 07s left
```

The app ID comes from the `.lua` file's `addappid(<app>)` line. The game's
name, the build and the depot names come from its comments, so they appear
only when the tool that made the package wrote them. Each depot's manifest
ID and date (UTC) come from its manifest.

The line shows the download speed, which counts bytes actually received,
and the time left, both averaged over the last 10 seconds. Chunks arrive
compressed, so the files grow faster than the download speed suggests; the
time left is measured from how fast the files are being filled. When the run
ends it reports the total time and the average speed.

`-o DIR` downloads somewhere other than the current directory. `-j N` sets
the number of parallel chunk downloads (default 16); 16 already fills a
typical home connection, so it rarely needs changing.

A package can hold depots you may not want, such as language packs, builds
for another system, or shared redistributables like DirectX. `-l` lists the
depots and stops; `-d 2862861,228989` downloads only the depots named.

```bash
rin-dl ~/Downloads/2862860.zip -l
rin-dl ~/Downloads/2862860.zip -d 2862861
```

### Window

Run `rin-dl` without arguments, or double-click it; the window is titled
The Gupta Promise. Choose the package (or drop it on the window); the window
then shows what is in it: the game, app ID and build, and each depot's ID,
manifest ID, date, file count and size. Check the output folder, which
starts as the directory the program was started from.

As soon as both are chosen, the window checks how much of the package is
already in that folder and shows it on the progress bar, so a download
started earlier, even in a previous session or from the terminal, shows
where it stands. The button then reads Resume, or Download if nothing is
there yet. Below the progress bar it shows the speed, the time left and how
much is done.

Every depot has a box, ticked to start with; untick the depots you do not
want, or use the box above them to tick or untick them all. The total, the
check and the download follow the ticks. When the package names no depot,
hovering a depot's ID lists its largest files, which usually tells what it
is for (`_CommonRedist/DirectX/...`, `..._de.pak`).

Pause finishes the chunks in flight and stops; Resume continues. The
parallel downloads setting and the depot boxes can only be changed while no
download is running, so pause to change them.

### Content servers

Steam's server directory chooses servers near you from your IP address; the
first log line lists the ones in use. Each parallel download sticks to one
server and moves to the next if it fails, so the work is spread over all of
them. If downloads seem slow, compare the speed with your connection's: more
parallel downloads (`-j`) only help while the connection is not yet full.

### Resuming and repairing

Resuming works per chunk (normally 1 MiB), not per file, so an interrupted
10 GB file is not downloaded again from the start. Each chunk is written
straight into its file at its offset. Every run, after Ctrl+C, Pause, a
crash or a network failure, first checks the folder: it reads the chunks
already on disk, keeps those whose SHA-1 matches the manifest, and then
fetches only the rest. There is no state file, and the same pass repairs
damaged files; a finished download is rechecked without fetching anything.
The check reads what is already on disk, which takes seconds on an SSD and
longer on a hard drive.

Within one session the window remembers what it found, along with each
file's size and modification time. Pressing Resume after its check, or after
a Pause, therefore reads again only the files that changed since. Closing
the window forgets this, and the next session checks everything again.

A chunk that fails is retried on every content server, three rounds, before
it is reported. The run continues without it and ends with an error telling
you to run again; after 20 failed chunks it stops early.

### Details

- Supports raw and ZIP-wrapped manifests, encrypted file names, and zstd,
  LZMA and ZIP chunks. Every chunk's SHA-1 is checked before it is kept.
- A manifest whose depot has no key in the `.lua` file is skipped with a
  warning, as are symbolic-link entries.
- Two depots that hold the same file, typically one game built for two
  systems, cannot be downloaded together; the error names both, and you
  pick one.
- Manifest paths cannot leave the output directory, and output paths that
  run through an existing symbolic link are refused.
- Executable flags are set on Linux. Files in the output directory that are
  not in the manifests are left alone.

You must supply the actual manifest files and keys; an app ID or manifest ID
alone is not enough. This tool does not fetch keys or manifests or modify
Steam's installation records. The regular
[DepotDownloader](https://github.com/SteamRE/DepotDownloader) cannot take
local key and manifest inputs;
[DepotDownloaderMod](https://github.com/lunaeves/DepotDownloaderMod) adds
them but uses .NET and an anonymous Steam session. `rin-dl` talks to the CDN
over HTTPS directly.

The tests run offline, with local HTTP servers standing in for the CDN:

```bash
cargo test
```
