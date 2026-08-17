# osrs-wiki-cache-utils

Build and search a local copy of the Old School RuneScape Wiki and decoded game
cache. It parses pages and complex tables into structured chunks, indexes exact
terms with SQLite FTS5, and generates semantic embeddings locally using
FastEmbed and BGE-small-en-v1.5. Keyword and meaning-based results are combined
into one ranking and exposed through CLI and MCP tools.

## Quick start

Requires Rust and about 12 GB of free disk space for a full Wiki build. The
final Wiki snapshot, index, and model use about 5 GB. Cache data needs
additional space. Expect the first full build to take several hours.

```sh
cargo build --release

# Download current Wiki pages as revision-pinned Parsoid HTML.
target/release/osrs-wiki-offline snapshot --output snapshot

# Build the local search index. The embedding model downloads on first use.
target/release/osrs-wiki-offline index \
  --snapshot snapshot \
  --database index.sqlite \
  --model-cache model-cache

# Search the Wiki.
target/release/osrs-wiki-offline search \
  --database index.sqlite \
  --model-cache model-cache \
  "charged bow ranged strength and attack bonus"
```

The snapshot includes current, non-redirect articles and transcripts. Pages
explicitly categorized as historical, removed, obsolete, or discontinued are
excluded. Raw Parsoid HTML is retained so derived search text is recoverable.

## Add game-cache search

The optional cache import reads decoded configs, interfaces, client scripts,
and symbols from [`Joshua-F/osrs-dumps`](https://github.com/Joshua-F/osrs-dumps).
It skips binary maps, models, sprites, and audio.

```sh
GIT_LFS_SKIP_SMUDGE=1 git clone --depth 1 \
  https://github.com/Joshua-F/osrs-dumps.git cache-dump
CACHE_COMMIT=$(git -C cache-dump rev-parse HEAD)

target/release/osrs-wiki-offline index \
  --snapshot snapshot \
  --database index.sqlite \
  --model-cache model-cache \
  --cache-dump cache-dump \
  --cache-commit "$CACHE_COMMIT"

target/release/osrs-wiki-offline cache-search \
  --database index.sqlite \
  --model-cache model-cache \
  "abyssal whip destroy option"

target/release/osrs-wiki-offline cache-get \
  --database index.sqlite \
  --model-cache model-cache \
  config/obj 4151
```

## MCP server

Run the stdio server directly:

```sh
target/release/osrs-wiki-offline serve \
  --database /absolute/path/index.sqlite \
  --model-cache /absolute/path/model-cache
```

Or add it to an MCP client:

```json
{
  "mcpServers": {
    "osrs": {
      "command": "/absolute/path/target/release/osrs-wiki-offline",
      "args": [
        "serve",
        "--database", "/absolute/path/index.sqlite",
        "--model-cache", "/absolute/path/model-cache"
      ]
    }
  }
}
```

Available tools:

- `search_unified`
- `search_wiki`
- `get_wiki_page`
- `get_wiki_sections`
- `get_wiki_section`
- `search_cache`
- `get_cache_entry`

`search_cache` accepts an optional `kind` such as `config/loc`,
`config/varbit`, `interface`, or `script`.

### Example: investigate a game mechanic

Suppose a user asks:

> I'm building a Blast Furnace plugin. Which varbits and game objects should I
> watch for ore on the conveyor belt?

An MCP client can keep the natural question for Wiki context, then translate
it into focused cache searches:

```json
[
  {
    "tool": "search_wiki",
    "arguments": {
      "query": "How does ore move through the Blast Furnace conveyor belt?",
      "limit": 2
    }
  },
  {
    "tool": "search_cache",
    "arguments": {
      "query": "blast furnace conveyor belt",
      "kind": "config/loc",
      "limit": 4
    }
  },
  {
    "tool": "search_cache",
    "arguments": {
      "query": "blast furnace ore",
      "kind": "config/varbit",
      "limit": 4
    }
  }
]
```

A combined, abridged view of the output:

```json
{
  "wiki": [
    {
      "title": "Blast Furnace/Strategies",
      "section": "Possible methods",
      "snippet": "Load coal and/or ore on the conveyor belt ... then load up to 28 ore."
    },
    {
      "title": "Blast Furnace",
      "section": "Operating the Blast Furnace > Notes",
      "snippet": "The maximum amount of primary ores ... is 28."
    }
  ],
  "locations": [
    {
      "kind": "config/loc",
      "id": "9101",
      "symbol": "blast_furnace_conveyer_belt",
      "snippet": "model=model_9063\nname=Conveyor belt"
    },
    {
      "kind": "config/loc",
      "id": "9100",
      "symbol": "blast_furnace_conveyer_belt_clickable",
      "snippet": "op1=Put-ore-on\nmodel=model_9063\nname=Conveyor belt"
    }
  ],
  "varbits": [
    {
      "id": "18167",
      "symbol": "blast_furnace_lead_ore",
      "snippet": "basevar=blast_furnace_6\nstartbit=8\nendbit=15"
    },
    {
      "id": "18168",
      "symbol": "blast_furnace_nickel_ore",
      "snippet": "basevar=blast_furnace_6\nstartbit=16\nendbit=23"
    },
    {
      "id": "951",
      "symbol": "blast_furnace_iron_ore",
      "snippet": "basevar=blast_furnace_3\nstartbit=16\nendbit=23"
    },
    {
      "id": "953",
      "symbol": "blast_furnace_adamantite_ore",
      "snippet": "basevar=blast_furnace_4\nstartbit=0\nendbit=7"
    }
  ]
}
```

Full tool responses also include source URLs, Wiki revision IDs, and the exact
cache-dump commit.

## Update and verify

Rerun `snapshot` and then `index` with the same paths. Successful pages are
checkpointed even if another page fails, so unchanged Wiki pages and index
entries are reused. Include the cache arguments when adding or updating
game-cache data.

```sh
target/release/osrs-wiki-offline verify \
  --snapshot snapshot \
  --database index.sqlite
```

Wiki content remains under
[CC BY-NC-SA 3.0](https://creativecommons.org/licenses/by-nc-sa/3.0/) with
per-revision attribution. Game-cache data has separate provenance and is not
covered by the Wiki license. This project's source code is MIT licensed.
