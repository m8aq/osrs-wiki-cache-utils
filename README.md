# osrs-wiki-cache-utils

## About

Search the Old School RuneScape Wiki, decoded game cache, RuneLite, and Plugin
Hub source entirely offline.

Unlike `rg`, this tool understands Wiki sections, nested tables, tabs, titles,
and redirects. SQLite FTS5 ranks the most useful matching sections and cache
records instead of returning isolated lines from raw files.

Use it from the command line or connect it to an MCP client. No database
server, embedding model, or answer-generating model is required.

## Why Rust?

Rust keeps large snapshot and indexing jobs fast and memory-conscious. The
entire tool ships as one executable with no language runtime to install.

## Quick Start

Download the program and prebuilt Wiki database from the
[latest release](https://github.com/m8aq/osrs-wiki-cache-utils/releases/latest).
No Rust toolchain or indexing is needed.

### macOS (Apple Silicon)

```sh
mkdir -p "$HOME/Documents/osrs" && cd "$HOME/Documents/osrs"
BASE=https://github.com/m8aq/osrs-wiki-cache-utils/releases/latest/download
curl -fL "$BASE/osrs-wiki-cache-utils-v0.2.1-aarch64-apple-darwin" -o osrs-wiki-cache-utils
curl -fLO "$BASE/wiki.sqlite"
chmod +x osrs-wiki-cache-utils
```

### Linux (x86-64)

```sh
mkdir -p "$HOME/Documents/osrs" && cd "$HOME/Documents/osrs"
BASE=https://github.com/m8aq/osrs-wiki-cache-utils/releases/latest/download
curl -fL "$BASE/osrs-wiki-cache-utils-v0.2.1-x86_64-unknown-linux-musl" -o osrs-wiki-cache-utils
curl -fLO "$BASE/wiki.sqlite"
chmod +x osrs-wiki-cache-utils
```

### Windows (64-bit PowerShell)

```powershell
New-Item -ItemType Directory -Force "$HOME\Documents\osrs" | Out-Null
Set-Location "$HOME\Documents\osrs"
$base = "https://github.com/m8aq/osrs-wiki-cache-utils/releases/latest/download"
Invoke-WebRequest "$base/osrs-wiki-cache-utils-v0.2.1-x86_64-pc-windows-msvc.exe" -OutFile osrs-wiki-cache-utils.exe
Invoke-WebRequest "$base/wiki.sqlite" -OutFile wiki.sqlite
```

## Search

Run this from the download directory:

```sh
./osrs-wiki-cache-utils search --database wiki.sqlite \
  "Blast Furnace coal bag ice gloves"
```

On Windows, replace `./osrs-wiki-cache-utils` with
`.\osrs-wiki-cache-utils.exe`.

Search uses words, titles, IDs, and symbols rather than generated embeddings.
Short, distinctive queries work best.

### Search the game cache

On macOS or Linux, download the optional cache database and search it:

```sh
curl -fLO "$BASE/cache.sqlite"

./osrs-wiki-cache-utils cache-search \
  --database wiki.sqlite \
  --cache-database cache.sqlite \
  --kind config/obj \
  "abyssal whip"
```

On Windows PowerShell:

```powershell
Invoke-WebRequest "$base/cache.sqlite" -OutFile cache.sqlite
.\osrs-wiki-cache-utils.exe cache-search --database wiki.sqlite --cache-database cache.sqlite --kind config/obj "abyssal whip"
```

The cache index covers decoded configs, interfaces, scripts, and symbols. It
does not include binary assets such as maps, models, sprites, or audio.

### Search RuneLite and Plugin Hub code

Download the optional code database, then search all indexed Java source or one
RuneLite module/Plugin Hub plugin. Plugin Hub kinds use
`pluginhub/<internalName>`:

```sh
curl -fLO "$BASE/runelite-code.sqlite"

./osrs-wiki-cache-utils code-search \
  --database wiki.sqlite \
  --code-database runelite-code.sqlite \
  --kind pluginhub/blast-furnace-trainer \
  "blast furnace"
```

On Windows PowerShell, download it with:

```powershell
Invoke-WebRequest "$base/runelite-code.sqlite" -OutFile runelite-code.sqlite
```

Use the result's case-sensitive `kind` and `id` with `code-get` to retrieve the
exact source file.

Checksums for every release download are in `SHA256SUMS.txt`.

## MCP

Add the server to an MCP client using absolute paths:

```json
{
  "mcpServers": {
    "osrs": {
      "command": "/absolute/path/osrs-wiki-cache-utils",
      "args": [
        "serve",
        "--database", "/absolute/path/wiki.sqlite",
        "--cache-database", "/absolute/path/cache.sqlite",
        "--code-database", "/absolute/path/runelite-code.sqlite"
      ]
    }
  }
}
```

The MCP returns source text and provenance for the client to interpret. It can
search each configured database, retrieve Wiki pages or individual sections,
and retrieve exact cache records or source files.

## Build Your Own Data

This requires Rust and several gigabytes of free disk space:

```sh
cargo build --release
BIN=target/release/osrs-wiki-cache-utils

$BIN snapshot --output snapshot
$BIN index --snapshot snapshot --database wiki.sqlite
```

To rebuild only the derived equipment requirement tables from an existing Wiki
index, without verifying or reading the snapshot again:

```sh
$BIN equipment-index --database wiki.sqlite
```

To add structured NPC, object, and ground-item coordinates from the stored Wiki
HTML:

```sh
$BIN spawn-index --database wiki.sqlite
$BIN spawn-search --database wiki.sqlite --entity npc --name "Citizen"
$BIN spawn-search --database wiki.sqlite --entity npc --id 10638
```

Wiki rows retain `mapId`, plane, source URL, and raw template data. `entityId` is
null when a multi-version Wiki page does not identify which NPC or object ID is
at a coordinate; the index never guesses that mapping. Coordinates are retained
exactly, including fractional Wiki map-pin centres that are not literal game
tiles.

An exact mejrs revision can also be imported for additional NPC rows:

```sh
curl -fL https://raw.githubusercontent.com/mejrs/data_osrs/6a3ca6f19d65c5609434b51cac8dee9d4af97c02/NPCList_OSRS.json -o NPCList_OSRS.json
$BIN spawn-index --database wiki.sqlite --mejrs-json NPCList_OSRS.json --mejrs-commit 6a3ca6f19d65c5609434b51cac8dee9d4af97c02
```

The source commit and file hash are recorded in `meta`. The upstream mejrs
repository does not currently declare a license, so this project makes no
license claim for imported rows. Some cache configs are genuinely unnamed, so
mejrs rows can have an empty `name`; use their numeric ID for lookup.

`equipment_items.requirement_status` distinguishes parsed requirements,
explicitly requirement-free items, and unresolved Wiki prose.
`equipment_requirements` stores typed requirement atoms with their context,
evidence, and basis. Group zero atoms are all required; atoms sharing another
group number are alternatives within that required group. Atom kinds cover
skills, combat levels, quests, diaries, boss kills, mastery sets, and dynamic
quest/diary/music-track sets. Version-specific prose such as trimmed-cape
requirements is attached only to the matching item ID.
The generated item-level table is checked in at
[`data/equipment-requirements.json`](data/equipment-requirements.json).

The snapshot contains revision-pinned Parsoid HTML from current Main and
Transcript pages. Raw HTML is preserved. Explicitly historical, removed,
obsolete, and discontinued pages are excluded.

Generated Wiki and code indexes also apply the fixed content exclusions in
`src/index.rs`. These exclusions are applied to both new and incremental builds.

To build `cache.sqlite` from
[`Joshua-F/osrs-dumps`](https://github.com/Joshua-F/osrs-dumps):

```sh
GIT_LFS_SKIP_SMUDGE=1 git clone --depth 1 \
  https://github.com/Joshua-F/osrs-dumps.git cache-dump

$BIN cache-index \
  --cache-dump cache-dump \
  --cache-commit "$(git -C cache-dump rev-parse HEAD)" \
  --database cache.sqlite
```

Rerun these commands to update the data. Unchanged records are reused, and
interrupted builds resume automatically.

To build `runelite-code.sqlite`, clone the source aggregate and a sparse
RuneLite checkout, then pin both exact commits:

```sh
git clone --depth 1 \
  https://github.com/JZomDev/pluginhub-searcher.git pluginhub-searcher
git clone --depth 1 --filter=blob:none --sparse \
  https://github.com/runelite/runelite.git runelite
git -C runelite sparse-checkout set runelite-api runelite-client
git clone --depth 1 \
  https://github.com/runelite/plugin-hub-tooling.git plugin-hub-tooling
git clone --depth 1 --filter=blob:none --sparse \
  https://github.com/runelite/api.runelite.net.git api.runelite.net
git -C api.runelite.net sparse-checkout set http-api

$BIN code-index \
  --database runelite-code.sqlite \
  --pluginhub-repo pluginhub-searcher \
  --pluginhub-commit "$(git -C pluginhub-searcher rev-parse HEAD)" \
  --runelite-repo runelite \
  --runelite-commit "$(git -C runelite rev-parse HEAD)" \
  --tooling-repo plugin-hub-tooling \
  --tooling-commit "$(git -C plugin-hub-tooling rev-parse HEAD)" \
  --http-api-repo api.runelite.net \
  --http-api-commit "$(git -C api.runelite.net rev-parse HEAD)"

$BIN verify --code-database runelite-code.sqlite
```

This indexes the 29,973 Java files across all 2,124 plugin records bundled by
the pinned `pluginhub-searcher` revision, including unavailable/build-failing
records. It also indexes Java from `runelite-api`, `runelite-client`, Plugin Hub
tooling, and the RuneLite HTTP API module. Original Plugin Hub repositories are
not cloned or fetched individually. The database retains each file's source
repository, pinned commit, immutable URL, and source header; upstream sources
retain their individual licenses.

## License

Wiki content is [CC BY-NC-SA 3.0](https://creativecommons.org/licenses/by-nc-sa/3.0/)
with per-revision attribution. Game-cache data has separate provenance. The
RuneLite and Plugin Hub sources retain their individual upstream licenses. The
source code in this repository is MIT licensed.
