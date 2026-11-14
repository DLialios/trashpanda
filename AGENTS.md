# Steam game content

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

Each chunk entry also has a checksum (`crc`, a fixed32): Adler-32 of the
chunk's content, but started from 0 rather than the standard 1. Adler-32 can be
moved along a file one byte at a time, so it finds a chunk at any offset.

### Updates reuse chunks

Content that a new version only moved keeps its chunk: the new manifest lists
the same SHA-1 at another offset, or in another file. Inserting data near the
start of a `.pak` moves everything after it, and renaming a file moves all of
it. The Steam client copies such chunks from the old files instead of
downloading them. A chunk counts as present only if it is at its offset in its
file, so comparing in place alone overstates an update: Bodycam's update from
September to November 2026 was 69.6 GiB in all, with 22.2 GiB still in place,
29.0 GiB on disk at other offsets or in other files, and 18.4 GiB new. Nearly
all chunks are exactly 1 MiB; only the last of a file, or one beside a change,
is smaller.

## How Steam picks depots

Steam does not guess which depots an install needs. The developer tags each
depot in the app's record (its appinfo), and the client downloads every depot
whose tags all fit the install. Only six kinds of tag occur, all under the
depot's `config` except `dlcappid`, and every value is a string:

| Tag | Values | Fits when |
| --- | --- | --- |
| `oslist` | `windows`, `macos`, `linux`, comma-separated | it lists the install's system |
| `osarch` | `32` or `64` | it equals the install's architecture |
| `language` | a Steam language code: `german`, `koreana`, `schinese` | it equals the install's language |
| `lowviolence` | `1` | the install wants low violence |
| `dlcappid`, `optionaldlc` | a DLC's app ID | the account has that DLC |

A tag that is absent fits everyone, so a depot without tags is installed for
everyone: Bodycam's 69 GiB game depot has none. Values and depots can still
run to the thousands: Train Simulator's record lists 1,326 depots for 1,320
DLC. `depotfromapp` marks a depot borrowed from another app, such as
`228989` (VC 2022 Redist) from `228980`; it is not a tag. The record's
`depots` map also holds keys that are not depots (`branches`,
`privatebranches`, `baselanguages`, ...); only all-digit keys are depots.
`depots.branches.public` gives the newest public `buildid` and its
`timebuildupdated` (Unix time). A DLC app's record has its own
`common.name` and no depots, and some have nothing at all (Civilization VI's
512031).

**The record is not in the package.** The package holds only keys and
manifests, though this program also reads a package that carries the mirror's
answers as `appinfo/<appid>.json`, and then asks no one. Steam serves the record over its own client protocol (PICS), not
plain HTTP; `https://api.steamcmd.net/v1/info/<appid>` is a third-party mirror that
serves it as JSON to anyone, `{"status": "success", "data": {"<appid>": {...}}}`.
Without the record, the systems a depot is for can only be guessed from the
names of its programs and libraries (`.exe`, `.so`, `.dylib`, ...).

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
