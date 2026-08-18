# osrs-wiki-cache-utils

Build and search a local copy of the Old School RuneScape Wiki and decoded game
cache.

Unlike searching downloaded files with `rg`, this tool understands Wiki page
sections, nested tables, tabs, titles, and redirects. It packs that structure
into searchable chunks and ranks matching pages with SQLite FTS5/BM25. Exact
Wiki titles, redirect aliases, cache symbols, and cache IDs are boosted.

Everything runs offline after the data is built. MCP tools return bounded
evidence with canonical URLs and revision provenance for a calling agent to
interpret. The server does not generate answers.

## Build

Requires Rust. A full Wiki snapshot and index need several gigabytes of disk.
Progress is printed throughout both operations.

```sh
cargo build --release

# Fetch revision-pinned Parsoid HTML.
target/release/osrs-wiki-offline snapshot --output snapshot

# Build the Wiki search database. Interrupted builds resume automatically.
target/release/osrs-wiki-offline index \
  --snapshot snapshot \
  --database wiki.sqlite
```

The snapshot includes current, non-redirect Main and Transcript pages. Pages
explicitly categorized as historical, removed, obsolete, or discontinued are
excluded. Raw Parsoid HTML is retained in the snapshot and compressed in the
Wiki database.

## Add Cache Search

The optional cache index reads decoded configs, interfaces, client scripts,
and symbols from [`Joshua-F/osrs-dumps`](https://github.com/Joshua-F/osrs-dumps).
Maps, models, sprites, audio, and other binary assets are not indexed.

```sh
GIT_LFS_SKIP_SMUDGE=1 git clone --depth 1 \
  https://github.com/Joshua-F/osrs-dumps.git cache-dump
CACHE_COMMIT=$(git -C cache-dump rev-parse HEAD)

target/release/osrs-wiki-offline cache-index \
  --cache-dump cache-dump \
  --cache-commit "$CACHE_COMMIT" \
  --database cache.sqlite
```

Wiki and cache data use separate databases so either source can be updated or
distributed independently.

## Search

```sh
target/release/osrs-wiki-offline search \
  --database wiki.sqlite \
  "charged bow ranged strength"

target/release/osrs-wiki-offline cache-search \
  --database wiki.sqlite \
  --cache-database cache.sqlite \
  --kind config/obj \
  "abyssal whip destroy option"

target/release/osrs-wiki-offline cache-get \
  --database wiki.sqlite \
  --cache-database cache.sqlite \
  config/obj 4151
```

## MCP

Run the stdio server:

```sh
target/release/osrs-wiki-offline serve \
  --database /absolute/path/wiki.sqlite \
  --cache-database /absolute/path/cache.sqlite
```

MCP client configuration:

```json
{
  "mcpServers": {
    "osrs": {
      "command": "/absolute/path/osrs-wiki-offline",
      "args": [
        "serve",
        "--database", "/absolute/path/wiki.sqlite",
        "--cache-database", "/absolute/path/cache.sqlite"
      ]
    }
  }
}
```

Tools:

- `search_unified`
- `search_wiki`
- `get_wiki_page`
- `get_wiki_sections`
- `get_wiki_section`
- `search_cache`
- `get_cache_entry`

`search_cache` accepts an optional kind such as `config/loc`, `config/varbit`,
`interface`, or `script`.

## Update And Verify

Rerun `snapshot`, `index`, and `cache-index` with the same paths. Unchanged Wiki
pages and cache records are reused. Interrupted initial builds resume from the
last committed page or cache batch.

```sh
target/release/osrs-wiki-offline verify \
  --snapshot snapshot \
  --database wiki.sqlite \
  --cache-database cache.sqlite
```

Wiki content remains under
[CC BY-NC-SA 3.0](https://creativecommons.org/licenses/by-nc-sa/3.0/) with
per-revision attribution. Game-cache data has separate provenance and is not
covered by the Wiki license. This project's source code is MIT licensed.
