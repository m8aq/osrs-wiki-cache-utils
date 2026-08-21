use std::collections::BTreeSet;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use parsoid::{Wikicode, prelude::*};
use rusqlite::{Connection, Transaction, params};
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::extract::attrs;

#[derive(Debug, Clone, PartialEq)]
struct Spawn {
    entity_type: &'static str,
    entity_id: Option<u32>,
    name: String,
    x: f64,
    y: f64,
    plane: i32,
    map_id: Option<i32>,
    location: Option<String>,
    raw_json: String,
}

/// One source-preserving NPC, object, or ground-item placement.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SpawnResult {
    pub source: String,
    pub entity_type: String,
    pub entity_id: Option<u32>,
    pub name: String,
    pub x: f64,
    pub y: f64,
    pub plane: i32,
    pub map_id: Option<i32>,
    pub location: Option<String>,
    pub source_url: String,
}

/// Exact structured spawn matches and the uncapped match count.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SpawnSearchOutput {
    pub results: Vec<SpawnResult>,
    pub total: usize,
}

/// Adds source-preserving spawn tables to an existing Wiki index.
pub(crate) fn create_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS spawns(
           row_id INTEGER PRIMARY KEY,
           source TEXT NOT NULL CHECK(source IN ('wiki', 'mejrs')),
           entity_type TEXT NOT NULL CHECK(entity_type IN ('npc', 'object', 'item')),
           entity_id INTEGER,
           name TEXT NOT NULL,
           x REAL NOT NULL,
           y REAL NOT NULL,
           plane INTEGER NOT NULL CHECK(plane BETWEEN 0 AND 3),
           map_id INTEGER,
           location TEXT,
           page_id INTEGER REFERENCES pages(id) ON DELETE CASCADE,
           source_url TEXT NOT NULL,
           raw_json TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS spawns_entity_id ON spawns(entity_type, entity_id);
         CREATE INDEX IF NOT EXISTS spawns_name ON spawns(entity_type, name COLLATE NOCASE);
         CREATE INDEX IF NOT EXISTS spawns_position ON spawns(map_id, plane, x, y);",
    )?;
    Ok(())
}

/// Replaces the Wiki-derived spawn rows from lossless Parsoid HTML.
pub(crate) fn rebuild_wiki(transaction: &Transaction<'_>) -> Result<usize> {
    let pages = {
        let mut statement = transaction.prepare(
            "SELECT id, title, revision_url, raw_content_zstd
             FROM pages
             WHERE source_kind = 'wiki'",
        )?;
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };

    let mut parsed = Vec::new();
    for (page_id, title, source_url, compressed) in pages {
        let html = zstd::stream::decode_all(compressed.as_slice())
            .with_context(|| format!("decode stored HTML for {title}"))?;
        let html = String::from_utf8(html).with_context(|| format!("decode UTF-8 for {title}"))?;
        parsed.extend(
            parse_wiki_page(&title, &html)?
                .into_iter()
                .map(|spawn| (page_id, source_url.clone(), spawn)),
        );
    }

    transaction.execute("DELETE FROM spawns WHERE source = 'wiki'", [])?;
    for (page_id, source_url, spawn) in &parsed {
        transaction.execute(
            "INSERT INTO spawns(source, entity_type, entity_id, name, x, y, plane, map_id, location, page_id, source_url, raw_json)
             VALUES ('wiki', ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                spawn.entity_type,
                spawn.entity_id,
                spawn.name,
                spawn.x,
                spawn.y,
                spawn.plane,
                spawn.map_id,
                spawn.location,
                page_id,
                source_url,
                spawn.raw_json,
            ],
        )?;
    }
    transaction.execute(
        "INSERT INTO meta(key, value) VALUES ('spawns/wiki', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [serde_json::json!({
            "indexedAt": Utc::now().to_rfc3339(),
            "rows": parsed.len(),
            "source": "stored Wiki snapshot"
        })
        .to_string()],
    )?;
    Ok(parsed.len())
}

/// Replaces mejrs NPC placements after validating the entire pinned JSON array.
pub(crate) fn replace_mejrs(
    transaction: &Transaction<'_>,
    data: &str,
    commit: &str,
) -> Result<usize> {
    let commit = commit.to_ascii_lowercase();
    if commit.len() != 40 || !commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("mejrs commit must be a full 40-character Git SHA");
    }
    let rows = serde_json::from_str::<Value>(data)
        .context("decode mejrs NPCList_OSRS.json")?
        .as_array()
        .cloned()
        .context("mejrs NPCList_OSRS.json must be an array")?;
    let parsed = rows
        .into_iter()
        .enumerate()
        .map(|(index, row)| {
            let object = row
                .as_object()
                .with_context(|| format!("mejrs row {index} must be an object"))?;
            let name = object
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .with_context(|| format!("mejrs row {index} has no name"))?;
            let npc_id = required_u32(object, "id", index)?;
            let plane = required_i32(object, "p", index)?;
            let x = required_i32(object, "x", index)?;
            let y = required_i32(object, "y", index)?;
            if !(0..=3).contains(&plane) {
                bail!("mejrs row {index} plane is outside 0..=3");
            }
            if !(0..=0x3fff).contains(&x) || !(0..=0x3fff).contains(&y) {
                bail!("mejrs row {index} coordinates are outside 0..=16383");
            }
            Ok((npc_id, name.to_string(), plane, x, y, row.to_string()))
        })
        .collect::<Result<Vec<_>>>()?;
    let source_url = format!("https://github.com/mejrs/data_osrs/blob/{commit}/NPCList_OSRS.json");

    transaction.execute("DELETE FROM spawns WHERE source = 'mejrs'", [])?;
    for (npc_id, name, plane, x, y, raw_json) in &parsed {
        transaction.execute(
            "INSERT INTO spawns(source, entity_type, entity_id, name, x, y, plane, map_id, location, page_id, source_url, raw_json)
             VALUES ('mejrs', 'npc', ?1, ?2, ?3, ?4, ?5, NULL, NULL, NULL, ?6, ?7)",
            params![npc_id, name, x, y, plane, source_url, raw_json],
        )?;
    }
    transaction.execute(
        "INSERT INTO meta(key, value) VALUES ('spawns/mejrs', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [serde_json::json!({
            "commit": commit,
            "indexedAt": Utc::now().to_rfc3339(),
            "license": null,
            "rows": parsed.len(),
            "sha256": format!("{:x}", Sha256::digest(data.as_bytes())),
            "sourceUrl": source_url
        })
        .to_string()],
    )?;
    Ok(parsed.len())
}

/// Finds placements by entity kind and exact case-insensitive name or numeric ID.
pub(crate) fn search(
    connection: &Connection,
    entity_type: &str,
    entity_id: Option<u32>,
    name: Option<&str>,
    limit: usize,
) -> Result<SpawnSearchOutput> {
    if !matches!(entity_type, "npc" | "object" | "item") {
        bail!("entity type must be npc, object, or item");
    }
    let name = name.map(str::trim).filter(|name| !name.is_empty());
    if entity_id.is_none() && name.is_none() {
        bail!("spawn search requires --id or --name");
    }
    if limit == 0 {
        bail!("spawn search limit must be greater than zero");
    }
    let total: i64 = connection.query_row(
        "SELECT count(*) FROM spawns
         WHERE entity_type = ?1
           AND (?2 IS NULL OR entity_id = ?2)
           AND (?3 IS NULL OR name = ?3 COLLATE NOCASE)",
        params![entity_type, entity_id, name],
        |row| row.get(0),
    )?;
    let mut statement = connection.prepare(
        "SELECT source, entity_type, entity_id, name, x, y, plane, map_id, location, source_url
         FROM spawns
         WHERE entity_type = ?1
           AND (?2 IS NULL OR entity_id = ?2)
           AND (?3 IS NULL OR name = ?3 COLLATE NOCASE)
         ORDER BY source, map_id, plane, x, y, row_id
         LIMIT ?4",
    )?;
    let results = statement
        .query_map(
            params![entity_type, entity_id, name, limit.min(1_000) as i64],
            |row| {
                Ok(SpawnResult {
                    source: row.get(0)?,
                    entity_type: row.get(1)?,
                    entity_id: row.get(2)?,
                    name: row.get(3)?,
                    x: row.get(4)?,
                    y: row.get(5)?,
                    plane: row.get(6)?,
                    map_id: row.get(7)?,
                    location: row.get(8)?,
                    source_url: row.get(9)?,
                })
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(SpawnSearchOutput {
        results,
        total: usize::try_from(total).context("spawn count is negative")?,
    })
}

fn required_i32(object: &Map<String, Value>, field: &str, index: usize) -> Result<i32> {
    let value = object
        .get(field)
        .and_then(Value::as_i64)
        .with_context(|| format!("mejrs row {index} has no integer {field}"))?;
    i32::try_from(value).with_context(|| format!("mejrs row {index} {field} is out of range"))
}

fn required_u32(object: &Map<String, Value>, field: &str, index: usize) -> Result<u32> {
    let value = object
        .get(field)
        .and_then(Value::as_u64)
        .with_context(|| format!("mejrs row {index} has no nonnegative integer {field}"))?;
    u32::try_from(value).with_context(|| format!("mejrs row {index} {field} is out of range"))
}

fn parse_wiki_page(title: &str, html: &str) -> Result<Vec<Spawn>> {
    let templates = templates(html)?;
    let npc_ids = infobox_ids(&templates, "infobox npc");
    let item_ids = infobox_ids(&templates, "infobox item");
    let object_ids = infobox_ids(&templates, "infobox object");
    let single_npc_id = (npc_ids.len() == 1).then(|| npc_ids[0]);
    let single_item_id = (item_ids.len() == 1).then(|| item_ids[0]);
    let single_object_id = (object_ids.len() == 1).then(|| object_ids[0]);
    let mut spawns = Vec::new();

    for template in templates {
        let Some((name, params)) = template_parts(&template) else {
            continue;
        };
        let entity_type = match name.as_str() {
            "locline" => "npc",
            "map" => "npc",
            "itemspawnline" => "item",
            "objectlocline" => "object",
            _ => continue,
        };
        if param(params, "mtype").is_some_and(|value| !value.eq_ignore_ascii_case("pin")) {
            continue;
        }
        let plane = integer_param(params, "plane")?.unwrap_or(0);
        let map_id = integer_param(params, "mapID")?;
        let spawn_name = param(params, "name").unwrap_or(title).trim().to_string();
        let location = param(params, "location").map(str::to_string);
        let contextual_id = match entity_type {
            "npc" => single_npc_id,
            "item" => single_item_id,
            "object" => match param(params, "version") {
                Some(version) => single_integer(Some(version)),
                None => single_object_id,
            },
            _ => None,
        };
        for coordinate in
            coordinates(params).with_context(|| format!("parse {name} coordinates on {title}"))?
        {
            let entity_id = coordinate.npc_id.or(contextual_id);
            if name == "map" && coordinate.npc_id.is_none() {
                continue;
            }
            spawns.push(Spawn {
                entity_type,
                entity_id,
                name: spawn_name.clone(),
                x: coordinate.x,
                y: coordinate.y,
                plane,
                map_id,
                location: location.clone(),
                raw_json: serde_json::to_string(&template)?,
            });
        }
    }
    Ok(spawns)
}

fn single_integer(value: Option<&str>) -> Option<u32> {
    let mut values = value?
        .split(',')
        .filter_map(|part| part.trim().parse::<u32>().ok());
    let first = values.next()?;
    values.next().is_none().then_some(first)
}

fn templates(html: &str) -> Result<Vec<Value>> {
    let code = Wikicode::new(html);
    let mut templates = Vec::new();
    for node in code.select("[data-mw]") {
        let attributes = attrs(&node);
        let Some(data) = attributes.get("data-mw") else {
            continue;
        };
        let value: Value = serde_json::from_str(data).context("decode Parsoid data-mw")?;
        let Some(parts) = value.get("parts").and_then(Value::as_array) else {
            continue;
        };
        templates.extend(
            parts
                .iter()
                .filter_map(|part| part.get("template"))
                .cloned(),
        );
    }
    Ok(templates)
}

fn template_parts(template: &Value) -> Option<(String, &Map<String, Value>)> {
    let name = template
        .get("target")?
        .get("wt")?
        .as_str()?
        .trim()
        .trim_start_matches("Template:")
        .trim()
        .to_ascii_lowercase();
    Some((name, template.get("params")?.as_object()?))
}

fn param<'a>(params: &'a Map<String, Value>, name: &str) -> Option<&'a str> {
    params.get(name)?.get("wt")?.as_str().map(str::trim)
}

fn integer_param(params: &Map<String, Value>, name: &str) -> Result<Option<i32>> {
    param(params, name)
        .map(|value| {
            value
                .split_once("<!--")
                .map_or(value, |(visible, _)| visible)
                .trim()
        })
        .filter(|value| !value.is_empty())
        .map(|value| {
            value
                .parse()
                .with_context(|| format!("parse {name}={value}"))
        })
        .transpose()
}

fn infobox_ids(templates: &[Value], infobox: &str) -> Vec<u32> {
    let mut ids = BTreeSet::new();
    for template in templates {
        let Some((name, params)) = template_parts(template) else {
            continue;
        };
        if name != infobox {
            continue;
        }
        ids.extend(params.iter().filter_map(|(key, value)| {
            (key == "id"
                || key.strip_prefix("id").is_some_and(|suffix| {
                    !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
                }))
            .then(|| value.get("wt")?.as_str()?.trim().parse::<u32>().ok())
            .flatten()
        }));
    }
    ids.into_iter().collect()
}

#[derive(Debug)]
struct Coordinate {
    x: f64,
    y: f64,
    npc_id: Option<u32>,
}

fn coordinates(params: &Map<String, Value>) -> Result<Vec<Coordinate>> {
    let mut values = params
        .iter()
        .filter_map(|(key, value)| Some((key.parse::<usize>().ok()?, value.get("wt")?.as_str()?)))
        .collect::<Vec<_>>();
    values.sort_by_key(|(index, _)| *index);
    values
        .into_iter()
        .filter_map(|(_, value)| {
            match parse_coordinate(value).with_context(|| format!("parse coordinate {value:?}")) {
                Ok(Some(coordinate)) => Some(Ok(coordinate)),
                Ok(None) => None,
                Err(error) => Some(Err(error)),
            }
        })
        .collect()
}

fn parse_coordinate(value: &str) -> Result<Option<Coordinate>> {
    let value = value
        .split_once("<!--")
        .map_or(value, |(visible, _)| visible);
    let mut plain = Vec::new();
    let mut x = None;
    let mut y = None;
    let mut npc_id = None;
    for part in value.trim().split(',').map(str::trim) {
        if let Some((key, value)) = part.split_once(':') {
            match key.trim().to_ascii_lowercase().as_str() {
                "x" => x = Some(value.trim().parse().context("parse coordinate x")?),
                "y" => {
                    y = Some(
                        value
                            .trim()
                            .parse()
                            .with_context(|| format!("parse coordinate y={value}"))?,
                    )
                }
                "npcid" => npc_id = Some(value.trim().parse().context("parse coordinate npcid")?),
                _ => {}
            }
        } else if let Ok(value) = part.parse::<f64>() {
            plain.push(value);
        }
    }
    let x = x.or_else(|| plain.first().copied());
    let y = y.or_else(|| plain.get(1).copied());
    Ok(match (x, y) {
        (Some(x), Some(y)) => Some(Coordinate { x, y, npc_id }),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::{Connection, params};

    #[test]
    fn keeps_multi_version_wiki_npcs_unresolved_but_uses_coordinate_npcid() {
        let html = r#"<html><body>
          <span data-mw='{"parts":[
            {"template":{"target":{"wt":"Infobox NPC"},"params":{"name":{"wt":"Citizen"},"id1":{"wt":"13164"},"id2":{"wt":"13165"}}}},
            {"template":{"target":{"wt":"LocLine"},"params":{"name":{"wt":"Citizen"},"plane":{"wt":"0"},"mapID":{"wt":"-1"},"1":{"wt":"x:1763,y:3068"}}}},
            {"template":{"target":{"wt":"Map"},"params":{"name":{"wt":"Gregg"},"mtype":{"wt":"pin"},"1":{"wt":"3105,3259,npcid:10638"}}}}
          ]}'></span>
        </body></html>"#;

        let spawns = parse_wiki_page("Citizen (Civitas illa Fortis)", html).unwrap();

        assert_eq!(spawns.len(), 2);
        assert_eq!(spawns[0].entity_type, "npc");
        assert_eq!(spawns[0].entity_id, None);
        assert_eq!(
            (spawns[0].x, spawns[0].y, spawns[0].plane),
            (1763.0, 3068.0, 0)
        );
        assert_eq!(spawns[0].map_id, Some(-1));
        assert_eq!(spawns[1].entity_id, Some(10638));
        assert_eq!(
            (spawns[1].x, spawns[1].y, spawns[1].plane),
            (3105.0, 3259.0, 0)
        );
    }

    #[test]
    fn parses_item_and_object_spawn_syntax_without_guessing_multiple_object_ids() {
        let html = r#"<html><body>
          <span data-mw='{"parts":[
            {"template":{"target":{"wt":"Infobox Item"},"params":{"id":{"wt":"1205"}}}},
            {"template":{"target":{"wt":"Infobox Object"},"params":{"id":{"wt":"999"}}}},
            {"template":{"target":{"wt":"ItemSpawnLine"},"params":{"name":{"wt":"Bronze dagger"},"plane":{"wt":"1"},"1":{"wt":"1624,3166,qty:1"}}}},
            {"template":{"target":{"wt":"ObjectLocLine"},"params":{"name":{"wt":"Chest"},"version":{"wt":"1234"},"1":{"wt":"x:1623,y:3162"}}}},
            {"template":{"target":{"wt":"ObjectLocLine"},"params":{"name":{"wt":"Bank booth"},"version":{"wt":"28430, 28431"},"1":{"wt":"1636,3740"}}}}
          ]}'></span>
        </body></html>"#;

        let spawns = parse_wiki_page("Bronze dagger", html).unwrap();

        assert_eq!(spawns.len(), 3);
        assert_eq!(
            (spawns[0].entity_type, spawns[0].entity_id),
            ("item", Some(1205))
        );
        assert_eq!(
            (spawns[0].x, spawns[0].y, spawns[0].plane),
            (1624.0, 3166.0, 1)
        );
        assert_eq!(
            (spawns[1].entity_type, spawns[1].entity_id),
            ("object", Some(1234))
        );
        assert_eq!(
            (spawns[2].entity_type, spawns[2].entity_id),
            ("object", None)
        );
    }

    #[test]
    fn ignores_commented_out_wiki_coordinates() {
        let html = r#"<html><body><span data-mw='{"parts":[
          {"template":{"target":{"wt":"LocLine"},"params":{"name":{"wt":"Fire giant"},"plane":{"wt":"0\n<!-- note -->"},"1":{"wt":"x:2643,y:9563\n<!--|x:2564,y:9543|x:2566,y:9551 -->"}}}}
        ]}'></span></body></html>"#;

        let spawns = parse_wiki_page("Fire giant", html).unwrap();

        assert_eq!(spawns.len(), 1);
        assert_eq!((spawns[0].x, spawns[0].y), (2643.0, 9563.0));
    }

    #[test]
    fn preserves_fractional_wiki_map_coordinates() {
        let html = r#"<html><body><span data-mw='{"parts":[
          {"template":{"target":{"wt":"Infobox NPC"},"params":{"id":{"wt":"7416"}}}},
          {"template":{"target":{"wt":"LocLine"},"params":{"name":{"wt":"Obor"},"1":{"wt":"x:3091.5,y:9799"}}}}
        ]}'></span></body></html>"#;

        let spawns = parse_wiki_page("Obor", html).unwrap();

        assert_eq!((spawns[0].x, spawns[0].y), (3091.5, 9799.0));
    }

    #[test]
    fn treats_empty_optional_numeric_params_as_absent() {
        let html = r#"<html><body><span data-mw='{"parts":[
          {"template":{"target":{"wt":"LocLine"},"params":{"name":{"wt":"Citizen"},"plane":{"wt":""},"mapID":{"wt":""},"1":{"wt":"1763,3068"}}}}
        ]}'></span></body></html>"#;

        let spawns = parse_wiki_page("Citizen", html).unwrap();

        assert_eq!(spawns[0].plane, 0);
        assert_eq!(spawns[0].map_id, None);
    }

    #[test]
    fn rebuilds_wiki_spawns_from_lossless_parsoid_html() {
        let mut connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 CREATE TABLE pages(
                   id INTEGER PRIMARY KEY,
                   source_kind TEXT NOT NULL,
                   title TEXT NOT NULL,
                   revision_id INTEGER NOT NULL,
                   revision_url TEXT NOT NULL,
                   raw_content_zstd BLOB NOT NULL
                 );",
            )
            .unwrap();
        let html = r#"<html><body><span data-mw='{"parts":[
          {"template":{"target":{"wt":"Infobox NPC"},"params":{"id":{"wt":"13164"}}}},
          {"template":{"target":{"wt":"LocLine"},"params":{"name":{"wt":"Citizen"},"mapID":{"wt":"-1"},"1":{"wt":"x:1763,y:3068"}}}}
        ]}'></span></body></html>"#;
        let compressed = zstd::stream::encode_all(html.as_bytes(), 1).unwrap();
        connection
            .execute(
                "INSERT INTO pages VALUES (1, 'wiki', 'Citizen', 42, 'https://example.test/revision/42', ?1)",
                params![compressed],
            )
            .unwrap();

        create_schema(&connection).unwrap();
        let transaction = connection.transaction().unwrap();
        assert_eq!(rebuild_wiki(&transaction).unwrap(), 1);
        transaction.commit().unwrap();

        let row = connection
            .query_row(
                "SELECT source, entity_type, entity_id, name, x, y, plane, map_id, page_id, source_url
                 FROM spawns",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<u32>>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, f64>(4)?,
                        row.get::<_, f64>(5)?,
                        row.get::<_, i32>(6)?,
                        row.get::<_, Option<i32>>(7)?,
                        row.get::<_, Option<i64>>(8)?,
                        row.get::<_, String>(9)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            row,
            (
                "wiki".to_string(),
                "npc".to_string(),
                Some(13164),
                "Citizen".to_string(),
                1763.0,
                3068.0,
                0,
                Some(-1),
                Some(1),
                "https://example.test/revision/42".to_string(),
            )
        );
    }

    #[test]
    fn removes_wiki_spawns_when_their_source_page_is_deleted() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "PRAGMA foreign_keys = ON;
             CREATE TABLE pages(id INTEGER PRIMARY KEY);
             INSERT INTO pages VALUES (1);",
            )
            .unwrap();
        create_schema(&connection).unwrap();
        connection.execute_batch(
            "INSERT INTO spawns(source, entity_type, name, x, y, plane, page_id, source_url, raw_json)
             VALUES ('wiki', 'npc', 'Citizen', 1763, 3068, 0, 1, 'wiki', '{}');
             DELETE FROM pages WHERE id = 1;",
        ).unwrap();

        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM spawns", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn imports_mejrs_duplicates_and_rejects_invalid_rows_before_replacement() {
        let mut connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 CREATE TABLE pages(id INTEGER PRIMARY KEY);",
            )
            .unwrap();
        create_schema(&connection).unwrap();
        let data = r#"[
          {"name":"Tool Leprechaun","id":0,"p":0,"x":2941,"y":3432,"combatLevel":0},
          {"name":"Tool Leprechaun","id":0,"p":0,"x":2941,"y":3432,"combatLevel":0},
          {"name":"","id":1172,"p":0,"x":2435,"y":4450,"combatLevel":0}
        ]"#;
        let commit = "6a3ca6f19d65c5609434b51cac8dee9d4af97c02";
        let transaction = connection.transaction().unwrap();
        assert_eq!(replace_mejrs(&transaction, data, commit).unwrap(), 3);
        transaction.commit().unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT count(*) FROM spawns WHERE source='mejrs'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            3
        );

        let transaction = connection.transaction().unwrap();
        assert!(
            replace_mejrs(
                &transaction,
                r#"[{"name":"Bad","id":1,"p":4,"x":1,"y":2}]"#,
                commit
            )
            .is_err()
        );
        drop(transaction);
        assert_eq!(
            connection
                .query_row(
                    "SELECT count(*) FROM spawns WHERE source='mejrs'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            3
        );
    }

    #[test]
    fn searches_spawns_by_exact_name_or_id_without_hiding_unresolved_rows() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 CREATE TABLE pages(id INTEGER PRIMARY KEY);
                 INSERT INTO pages VALUES (1);",
            )
            .unwrap();
        create_schema(&connection).unwrap();
        connection.execute_batch(
            "INSERT INTO spawns(source, entity_type, entity_id, name, x, y, plane, map_id, page_id, source_url, raw_json)
             VALUES ('wiki', 'npc', NULL, 'Citizen', 1763, 3068, 0, -1, 1, 'wiki', '{}');
             INSERT INTO spawns(source, entity_type, entity_id, name, x, y, plane, source_url, raw_json)
             VALUES ('mejrs', 'npc', 0, 'Tool Leprechaun', 2941, 3432, 0, 'mejrs', '{}');",
        ).unwrap();

        let citizens = search(&connection, "npc", None, Some("citizen"), 10).unwrap();
        assert_eq!(citizens.total, 1);
        assert_eq!(citizens.results[0].entity_id, None);
        assert_eq!(citizens.results[0].map_id, Some(-1));

        let leprechauns = search(&connection, "npc", Some(0), None, 10).unwrap();
        assert_eq!(leprechauns.total, 1);
        assert_eq!(leprechauns.results[0].name, "Tool Leprechaun");
        assert!(search(&connection, "npc", None, None, 10).is_err());
    }
}
