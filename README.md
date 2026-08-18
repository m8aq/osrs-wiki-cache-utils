# osrs-wiki-cache-utils

## About

Search the Old School RuneScape Wiki and decoded game cache entirely offline.

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
curl -fL "$BASE/osrs-wiki-cache-utils-v0.1.0-aarch64-apple-darwin" -o osrs-wiki-cache-utils
curl -fLO "$BASE/wiki.sqlite"
chmod +x osrs-wiki-cache-utils
```

### Linux (x86-64)

```sh
mkdir -p "$HOME/Documents/osrs" && cd "$HOME/Documents/osrs"
BASE=https://github.com/m8aq/osrs-wiki-cache-utils/releases/latest/download
curl -fL "$BASE/osrs-wiki-cache-utils-v0.1.0-x86_64-unknown-linux-musl" -o osrs-wiki-cache-utils
curl -fLO "$BASE/wiki.sqlite"
chmod +x osrs-wiki-cache-utils
```

### Windows (64-bit PowerShell)

```powershell
New-Item -ItemType Directory -Force "$HOME\Documents\osrs" | Out-Null
Set-Location "$HOME\Documents\osrs"
$base = "https://github.com/m8aq/osrs-wiki-cache-utils/releases/latest/download"
Invoke-WebRequest "$base/osrs-wiki-cache-utils-v0.1.0-x86_64-pc-windows-msvc.exe" -OutFile osrs-wiki-cache-utils.exe
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
        "--cache-database", "/absolute/path/cache.sqlite"
      ]
    }
  }
}
```

The MCP returns source text and provenance for the client to interpret. It can
search both databases, retrieve Wiki pages or individual sections, and retrieve
exact cache records.

## Build Your Own Data

This requires Rust and several gigabytes of free disk space:

```sh
cargo build --release
BIN=target/release/osrs-wiki-cache-utils

$BIN snapshot --output snapshot
$BIN index --snapshot snapshot --database wiki.sqlite
```

The snapshot contains revision-pinned Parsoid HTML from current Main and
Transcript pages. Raw HTML is preserved. Explicitly historical, removed,
obsolete, and discontinued pages are excluded.

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

## License

Wiki content is [CC BY-NC-SA 3.0](https://creativecommons.org/licenses/by-nc-sa/3.0/)
with per-revision attribution. Game-cache data has separate provenance. The
source code in this repository is MIT licensed.
