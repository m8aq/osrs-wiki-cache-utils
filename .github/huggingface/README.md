---
license: cc-by-nc-sa-3.0
pretty_name: Offline OSRS Wiki Parsoid Search
---

# Offline OSRS Wiki Parsoid Search

Revision-pinned Parsoid HTML and derived SQLite retrieval indexes for current
Old School RuneScape Wiki Main and Transcript pages. The corpus is split by
`page_id % 32`; `measurement.json` records exact aggregate byte and page counts.

Wiki content is attributed to Old School RuneScape Wiki contributors and is
licensed under CC BY-NC-SA 3.0. Per-page revision URLs and hashes are retained
inside each shard's `manifest.jsonl`.
