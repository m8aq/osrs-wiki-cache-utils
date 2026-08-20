use std::collections::{BTreeMap, HashMap};

use anyhow::{Result, bail};
use rusqlite::{Connection, Transaction, params};

const SKILLS: &[(&str, &str)] = &[
    ("attack", "Attack"),
    ("strength", "Strength"),
    ("defence", "Defence"),
    ("ranged", "Ranged"),
    ("prayer", "Prayer"),
    ("magic", "Magic"),
    ("runecraft", "Runecraft"),
    ("construction", "Construction"),
    ("hitpoints", "Hitpoints"),
    ("agility", "Agility"),
    ("herblore", "Herblore"),
    ("thieving", "Thieving"),
    ("crafting", "Crafting"),
    ("fletching", "Fletching"),
    ("slayer", "Slayer"),
    ("hunter", "Hunter"),
    ("mining", "Mining"),
    ("smithing", "Smithing"),
    ("fishing", "Fishing"),
    ("cooking", "Cooking"),
    ("firemaking", "Firemaking"),
    ("woodcutting", "Woodcutting"),
    ("farming", "Farming"),
    ("sailing", "Sailing"),
];

#[derive(Debug, Clone, PartialEq, Eq)]
struct Requirement {
    kind: &'static str,
    name: String,
    level: Option<u8>,
    context: &'static str,
    basis: &'static str,
    group_id: u8,
    trimmed_only: bool,
    evidence: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EquipmentItem {
    id: u32,
    name: String,
}

struct ParsedPage {
    page_id: i64,
    title: String,
    items: Vec<EquipmentItem>,
    requirements: Vec<Requirement>,
    status: &'static str,
    variant: Option<(String, String)>,
}

type RequirementMap = BTreeMap<(&'static str, String, &'static str, u8, bool), Requirement>;

/// Adds the derived equipment tables to an existing Wiki index.
pub(crate) fn create_schema(connection: &Connection) -> Result<()> {
    let has_old_requirements_schema: bool = connection.query_row(
        "SELECT EXISTS(
           SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = 'equipment_requirements'
         ) AND (
           NOT EXISTS(
             SELECT 1 FROM pragma_table_info('equipment_requirements') WHERE name = 'context'
           ) OR COALESCE(
             (SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = 'equipment_requirements'),
             ''
           ) NOT LIKE '%music_track_set%'
         )",
        [],
        |row| row.get(0),
    )?;
    if has_old_requirements_schema {
        connection.execute_batch("DROP TABLE equipment_requirements;")?;
    }
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS equipment_items(
           item_id INTEGER PRIMARY KEY,
           page_id INTEGER NOT NULL REFERENCES pages(id),
           name TEXT NOT NULL,
           requirement_status TEXT NOT NULL CHECK(requirement_status IN ('parsed', 'explicit_none', 'unresolved'))
         );
         CREATE TABLE IF NOT EXISTS equipment_requirements(
           item_id INTEGER NOT NULL REFERENCES equipment_items(item_id),
           context TEXT NOT NULL CHECK(context IN ('main', 'deadman', 'lms')),
           group_id INTEGER NOT NULL CHECK(group_id >= 0),
           kind TEXT NOT NULL CHECK(kind IN ('skill', 'skill_set', 'combat_level', 'boss_kill', 'quest', 'quest_set', 'miniquest', 'miniquest_section', 'diary', 'diary_set', 'music_track_set', 'unlock')),
           name TEXT NOT NULL,
           level INTEGER CHECK((kind IN ('skill', 'skill_set') AND level BETWEEN 1 AND 99) OR (kind = 'combat_level' AND level BETWEEN 3 AND 126) OR (kind = 'boss_kill' AND level >= 1) OR (kind NOT IN ('skill', 'skill_set', 'combat_level', 'boss_kill') AND level IS NULL)),
           basis TEXT NOT NULL CHECK(basis IN ('direct', 'effective_minimum', 'acquisition', 'continuing_eligibility')),
           evidence TEXT NOT NULL,
           PRIMARY KEY(item_id, context, group_id, kind, name)
         );
         CREATE INDEX IF NOT EXISTS equipment_requirements_lookup ON equipment_requirements(context, kind, name, level);",
    )?;
    Ok(())
}

/// Rebuilds the derived equipment and requirement tables from indexed Wiki lead sections.
pub(crate) fn rebuild(transaction: &Transaction<'_>) -> Result<()> {
    let progression_titles = progression_titles(transaction)?;
    let pages = {
        let mut statement = transaction.prepare(
            "SELECT p.id, p.title, s.content
             FROM pages p
             JOIN sections s ON s.page_id = p.id AND s.section_index = 0
             WHERE p.source_kind = 'wiki'
               AND p.categories_json LIKE '%\"Equipable items\"%'",
        )?;
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };

    let mut pages = pages
        .into_iter()
        .map(|(page_id, title, content)| {
            let (items, requirements, status) = parse_page(&title, &content, &progression_titles)?;
            Ok(ParsedPage {
                page_id,
                title,
                items,
                requirements,
                status,
                variant: cosmetic_variant(&content),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let mut targets = pages
        .iter()
        .map(|page| (words(&page.title).join(" "), page.page_id))
        .collect::<Vec<_>>();
    {
        let page_ids = pages
            .iter()
            .map(|page| (page.page_id, ()))
            .collect::<HashMap<_, _>>();
        let mut statement = transaction.prepare("SELECT alias, page_id FROM aliases")?;
        for alias in statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })? {
            let (alias, page_id) = alias?;
            if page_ids.contains_key(&page_id) {
                targets.push((words(&alias).join(" "), page_id));
            }
        }
    }
    targets.sort_by_key(|(name, _)| std::cmp::Reverse(name.len()));
    let sources = pages
        .iter()
        .map(|page| {
            (
                page.page_id,
                (page.title.clone(), page.requirements.clone(), page.status),
            )
        })
        .collect::<HashMap<_, _>>();
    for page in &mut pages {
        let Some((target, relation)) = &page.variant else {
            continue;
        };
        let mut target = words(target);
        if target.first().is_some_and(|word| word == "regular") {
            target.remove(0);
        }
        let target = target.join(" ");
        let Some((_, source_id)) = targets.iter().find(|(name, source_id)| {
            *source_id != page.page_id
                && (target == *name || target.starts_with(&format!("{name} ")))
        }) else {
            continue;
        };
        let (source_title, source_requirements, source_status) = &sources[source_id];
        if page.status == "explicit_none" || *source_status == "unresolved" {
            continue;
        }
        for source in source_requirements {
            if page.requirements.iter().any(|requirement| {
                requirement.kind == source.kind
                    && requirement.name == source.name
                    && requirement.context == source.context
                    && requirement.group_id == source.group_id
                    && requirement.trimmed_only == source.trimmed_only
            }) {
                continue;
            }
            let mut inherited = source.clone();
            inherited.evidence = format!(
                "{relation}; inherited from {source_title}: {}",
                source.evidence
            );
            page.requirements.push(inherited);
        }
        if page.status == "unresolved" {
            page.status = source_status;
        }
    }

    transaction.execute("DELETE FROM equipment_requirements", [])?;
    transaction.execute("DELETE FROM equipment_items", [])?;
    for ParsedPage {
        page_id,
        items,
        requirements,
        status,
        ..
    } in pages
    {
        for item in items {
            let requirements = requirements
                .iter()
                .filter(|requirement| !requirement.trimmed_only || item.name.ends_with("(t)"))
                .collect::<Vec<_>>();
            let status = if requirements.is_empty() && status == "parsed" {
                "unresolved"
            } else {
                status
            };
            transaction.execute(
                "INSERT INTO equipment_items(item_id, page_id, name, requirement_status)
                 VALUES (?1, ?2, ?3, ?4)",
                params![item.id, page_id, item.name, status],
            )?;
            for requirement in requirements {
                transaction.execute(
                    "INSERT INTO equipment_requirements(item_id, context, group_id, kind, name, level, basis, evidence)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        item.id,
                        requirement.context,
                        requirement.group_id,
                        requirement.kind,
                        requirement.name,
                        requirement.level,
                        requirement.basis,
                        requirement.evidence,
                    ],
                )?;
            }
        }
    }
    Ok(())
}

fn cosmetic_variant(content: &str) -> Option<(String, String)> {
    let prose = content.split_once("Template:Infobox Item:")?.0;
    for evidence in prose
        .split("\n\n")
        .flat_map(|block| block.split(['.', '?', '\n']))
        .map(str::trim)
        .filter(|sentence| !sentence.is_empty())
    {
        for phrase in ["cosmetic upgrade to the ", "cosmetic variant of the "] {
            if let Some((_, target)) = split_once_ascii_case(evidence, phrase) {
                return Some((target.trim().to_string(), evidence.to_string()));
            }
        }
    }
    None
}

fn progression_titles(connection: &Connection) -> Result<Vec<(String, &'static str)>> {
    let mut statement = connection.prepare(
        "SELECT title,
                CASE WHEN categories_json LIKE '%\"Quests\"%' THEN 'quest' ELSE 'miniquest' END
         FROM pages
         WHERE source_kind = 'wiki'
           AND namespace = 0
           AND title NOT IN ('Quests', 'Miniquests')
           AND title NOT LIKE 'Quests/%'
           AND title NOT LIKE 'Quest items/%'
           AND (categories_json LIKE '%\"Quests\"%'
                OR categories_json LIKE '%\"Miniquests\"%')",
    )?;
    let mut titles = statement
        .query_map([], |row| {
            let kind = match row.get::<_, String>(1)?.as_str() {
                "quest" => "quest",
                _ => "miniquest",
            };
            Ok((row.get::<_, String>(0)?, kind))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    titles.sort_by_key(|(title, _)| std::cmp::Reverse(title.len()));
    Ok(titles)
}

fn parse_page(
    title: &str,
    content: &str,
    progression_titles: &[(String, &'static str)],
) -> Result<(Vec<EquipmentItem>, Vec<Requirement>, &'static str)> {
    let Some((prose, infobox)) = content.split_once("Template:Infobox Item:") else {
        return Ok((Vec::new(), Vec::new(), "unresolved"));
    };
    let fields = infobox
        .split("\n\nTemplate:")
        .next()
        .unwrap_or(infobox)
        .split(" | ")
        .filter_map(|field| field.trim().split_once('='))
        .map(|(key, value)| (key.trim(), value.trim()))
        .collect::<HashMap<_, _>>();
    let mut items = fields
        .iter()
        .filter_map(|(key, value)| {
            let suffix = key.strip_prefix("id")?;
            if !suffix.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            let id = value.parse().ok()?;
            equipable(&fields, suffix).then(|| EquipmentItem {
                id,
                name: fields
                    .get(format!("name{suffix}").as_str())
                    .or_else(|| fields.get("name"))
                    .copied()
                    .unwrap_or(title)
                    .to_string(),
            })
        })
        .collect::<Vec<_>>();
    items.sort_by_key(|item| item.id);
    let untradeable = fields
        .get("tradeable")
        .is_some_and(|value| value.eq_ignore_ascii_case("no"));

    let mut requirements = RequirementMap::new();
    let mut explicit_none = false;
    for evidence in prose
        .split("\n\n")
        .filter(|block| !block.trim_start().starts_with("Columns:"))
        .flat_map(|block| block.split(['.', '?', '\n']))
        .map(str::trim)
        .filter(|sentence| !sentence.is_empty())
    {
        let tokens = words(evidence);
        let normalized = tokens.join(" ");
        if normalized.contains("level 99 in all") && normalized.contains("skills") {
            insert_requirement(
                &mut requirements,
                Requirement {
                    kind: "skill_set",
                    name: "All skills".to_string(),
                    level: Some(99),
                    context: "main",
                    basis: "acquisition",
                    group_id: 0,
                    trimmed_only: false,
                    evidence: evidence.to_string(),
                },
            )?;
        }
        for (phrase, name) in [
            (
                "unlocked all non holiday music tracks",
                "All non-holiday music tracks",
            ),
            (
                "unlock all holiday event tracks",
                "All holiday event tracks",
            ),
        ] {
            if normalized.contains(phrase) {
                insert_requirement(
                    &mut requirements,
                    Requirement {
                        kind: "music_track_set",
                        name: name.to_string(),
                        level: None,
                        context: "main",
                        basis: "continuing_eligibility",
                        group_id: 0,
                        trimmed_only: normalized.contains("trim"),
                        evidence: evidence.to_string(),
                    },
                )?;
            }
        }
        if normalized.contains("achievement diaries")
            && (normalized.contains("complete all") || normalized.contains("completed all"))
        {
            insert_requirement(
                &mut requirements,
                Requirement {
                    kind: "diary_set",
                    name: "All Achievement Diaries".to_string(),
                    level: None,
                    context: "main",
                    basis: "continuing_eligibility",
                    group_id: 0,
                    trimmed_only: normalized.contains("trim"),
                    evidence: evidence.to_string(),
                },
            )?;
        }
        if normalized.contains("completed all quests") {
            insert_requirement(
                &mut requirements,
                Requirement {
                    kind: "quest_set",
                    name: "All current quests".to_string(),
                    level: None,
                    context: "main",
                    basis: "continuing_eligibility",
                    group_id: 0,
                    trimmed_only: normalized.contains("trim"),
                    evidence: evidence.to_string(),
                },
            )?;
        }
        if untradeable && acquisition_context(&tokens) && completion_context(&tokens) {
            let padded = format!(" {normalized} ");
            for (name, kind) in progression_titles {
                if progression_aliases(name)
                    .iter()
                    .any(|alias| padded.contains(&format!(" {alias} ")))
                {
                    insert_requirement(
                        &mut requirements,
                        Requirement {
                            kind,
                            name: name.clone(),
                            level: None,
                            context: "main",
                            basis: "acquisition",
                            group_id: 0,
                            trimmed_only: false,
                            evidence: evidence.to_string(),
                        },
                    )?;
                }
            }
        }
        if untradeable
            && acquisition_context(&tokens)
            && tokens
                .iter()
                .any(|token| matches!(token.as_str(), "achieved" | "attained" | "mastery"))
        {
            for mut requirement in skill_requirements(evidence, &tokens, progression_titles) {
                requirement.basis = "acquisition";
                insert_requirement(&mut requirements, requirement)?;
            }
        }
        let contextual = normalized.contains("deadman mode");
        if !equip_context(&tokens) && !contextual {
            continue;
        }
        if normalized.contains("no requirements to wield")
            || normalized.contains("no requirements to wear")
            || normalized.contains("no requirements to equip")
            || normalized.contains("no level requirements to")
            || normalized.contains("do not require")
            || normalized.contains("does not require")
            || normalized.contains("don t require")
            || normalized.contains("doesn t require")
        {
            explicit_none = true;
            continue;
        }
        if normalized.contains("not required to") {
            continue;
        }
        if normalized.contains("no quest completion is required")
            || normalized.contains("not a requirement")
            || normalized.contains("isn t a requirement")
            || normalized.contains("requires no particular")
            || normalized.contains("does not need to be worn")
            || normalized.contains("doesn t need to be worn")
        {
            continue;
        }
        if let Some(requirement) = boss_kill_requirement(evidence) {
            insert_requirement(&mut requirements, requirement)?;
        }
        for requirement in skill_requirements(evidence, &tokens, progression_titles) {
            insert_requirement(&mut requirements, requirement)?;
        }
        if tokens
            .iter()
            .any(|token| requirement_word(token) && !matches!(token.as_str(), "have" | "has"))
            && completion_context(&tokens)
            && let Some(requirement) = diary_requirement(evidence, &tokens)
        {
            insert_requirement(&mut requirements, requirement)?;
        }
        let alternatives = alternative_requirements(evidence, &normalized, progression_titles);
        for requirement in alternatives {
            insert_requirement(&mut requirements, requirement)?;
        }
        if requirement_context(&tokens)
            && completion_context(&tokens)
            && !normalized.contains(" or ")
        {
            let padded = format!(" {normalized} ");
            for (name, kind) in progression_titles {
                let matched_title = progression_aliases(name)
                    .into_iter()
                    .find(|alias| padded.contains(&format!(" {alias} ")));
                let Some(matched_title) = matched_title else {
                    continue;
                };
                {
                    let section = (*kind == "miniquest")
                        .then(|| {
                            SKILLS.iter().find_map(|(key, skill)| {
                                (padded.contains(&format!(
                                    " {key} section of the {} miniquest ",
                                    matched_title
                                )) || padded.contains(&format!(" {} in {key} ", matched_title)))
                                .then_some(*skill)
                            })
                        })
                        .flatten();
                    insert_requirement(
                        &mut requirements,
                        Requirement {
                            kind: if section.is_some() {
                                "miniquest_section"
                            } else {
                                kind
                            },
                            name: section.map_or_else(
                                || name.clone(),
                                |section| format!("{name}: {section}"),
                            ),
                            level: None,
                            context: "main",
                            basis: "direct",
                            group_id: 0,
                            trimmed_only: false,
                            evidence: evidence.to_string(),
                        },
                    )?;
                }
            }
        }
    }
    let requirements = requirements.into_values().collect::<Vec<_>>();
    let status = if requirements.is_empty() {
        if explicit_none {
            "explicit_none"
        } else {
            "unresolved"
        }
    } else {
        "parsed"
    };
    Ok((items, requirements, status))
}

fn equipable(fields: &HashMap<&str, &str>, suffix: &str) -> bool {
    if !suffix.is_empty() {
        if let Some(value) = fields.get(format!("equipable{suffix}").as_str()) {
            return value.eq_ignore_ascii_case("yes");
        }
        if let Some(value) = fields.get(format!("options{suffix}").as_str()) {
            return equip_action(value);
        }
    }
    fields
        .get("equipable")
        .is_some_and(|value| value.eq_ignore_ascii_case("yes"))
}

fn equip_action(value: &str) -> bool {
    value.split(',').any(|option| {
        matches!(
            option.trim().to_ascii_lowercase().as_str(),
            "wear" | "wield" | "equip"
        )
    })
}

fn boss_kill_requirement(evidence: &str) -> Option<Requirement> {
    let (_, after_killed) = split_once_ascii_case(evidence, "killed ")?;
    let (name, _) = split_once_ascii_case(after_killed, " at least once")?;
    let mut characters = name.trim().chars();
    let name = characters.next()?.to_uppercase().collect::<String>() + characters.as_str();
    Some(Requirement {
        kind: "boss_kill",
        name,
        level: Some(1),
        context: "main",
        basis: "direct",
        group_id: 0,
        trimmed_only: false,
        evidence: evidence.to_string(),
    })
}

fn skill_requirements(
    evidence: &str,
    tokens: &[String],
    progression_titles: &[(String, &'static str)],
) -> Vec<Requirement> {
    let deadman = tokens.iter().any(|token| token == "deadman")
        && tokens.iter().any(|token| token == "level");
    if !requirement_context(tokens) && !deadman {
        return Vec::new();
    }
    let first_equip = tokens.iter().position(|token| equip_word(token));
    let before_equip = first_equip.map_or(tokens, |equip| &tokens[..equip]);
    let marker = before_equip
        .iter()
        .position(|token| requirement_word(token) && token != "minimum")
        .or_else(|| before_equip.iter().position(|token| token == "minimum"))
        .or_else(|| {
            tokens
                .iter()
                .position(|token| requirement_word(token) && token != "minimum")
        })
        .or_else(|| tokens.iter().position(|token| token == "minimum"));
    let equip = marker
        .and_then(|marker| {
            tokens[marker..]
                .iter()
                .position(|token| equip_word(token))
                .map(|position| marker + position)
        })
        .or_else(|| tokens.iter().position(|token| equip_word(token)));
    let action_clause_start = equip.and_then(|equip| {
        let other_action = tokens[..equip]
            .iter()
            .rposition(|token| other_action_word(token))?;
        if tokens[..other_action]
            .iter()
            .any(|token| token.parse::<u8>().is_ok())
        {
            return tokens[other_action + 1..equip]
                .iter()
                .position(|token| token.parse::<u8>().is_ok())
                .map(|position| other_action + position + 1);
        }
        let markers = tokens[..equip]
            .iter()
            .enumerate()
            .filter(|(_, token)| requirement_word(token))
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        (markers.len() > 1).then(|| *markers.last().unwrap())
    });
    let trailing_requirements = equip.is_some_and(|equip| {
        tokens[equip + 1..]
            .windows(3)
            .any(|window| window == ["as", "well", "as"])
            || tokens[equip + 1..]
                .windows(2)
                .any(|window| window == ["along", "with"])
            || marker.is_some_and(|marker| {
                tokens[marker] == "minimum"
                    && tokens[equip + 1..]
                        .iter()
                        .any(|token| token.parse::<u8>().is_ok())
            })
    });
    let clause = match (equip, marker, action_clause_start, trailing_requirements) {
        (_, Some(_), _, true) => tokens,
        (Some(equip), _, Some(start), _) => &tokens[start..=equip],
        (Some(equip), Some(marker), _, _) if marker <= equip => {
            let start = if matches!(tokens[marker].as_str(), "required" | "needed") {
                tokens[..marker]
                    .iter()
                    .rposition(|token| token.parse::<u8>().is_ok())
                    .map_or(marker, |number| number.saturating_sub(4))
            } else {
                marker
            };
            &tokens[start..=equip]
        }
        (Some(equip), Some(_), _, _) => &tokens[equip..],
        _ => tokens,
    };
    let numbers = clause
        .iter()
        .enumerate()
        .filter_map(|(index, token)| {
            token
                .parse::<u8>()
                .ok()
                .filter(|level| (1..=126).contains(level))
                .map(|level| (index, level))
        })
        .collect::<Vec<_>>();
    if numbers.is_empty() {
        return Vec::new();
    }
    let mut progression_tokens = vec![false; clause.len()];
    for (title, _) in progression_titles {
        for alias in progression_aliases(title) {
            let alias = words(&alias);
            for start in 0..=clause.len().saturating_sub(alias.len()) {
                if clause[start..].starts_with(&alias) {
                    progression_tokens[start..start + alias.len()].fill(true);
                }
            }
        }
    }
    clause
        .iter()
        .enumerate()
        .filter_map(|(skill_index, token)| {
            if progression_tokens[skill_index] {
                return None;
            }
            if clause.get(skill_index + 1).is_some_and(|token| {
                matches!(
                    token.as_str(),
                    "armour"
                        | "bonus"
                        | "bonuses"
                        | "equipment"
                        | "speed"
                        | "style"
                        | "styles"
                        | "weapon"
                )
            }) {
                return None;
            }
            let (kind, name) = if token == "combat" {
                ("combat_level", "Combat")
            } else {
                let (_, skill) = SKILLS.iter().find(|(key, _)| token == key)?;
                ("skill", *skill)
            };
            if clause.iter().skip(skill_index + 1).take(2).any(|token| {
                matches!(
                    token.as_str(),
                    "quest" | "quests" | "subquest" | "subquests"
                )
            }) {
                return None;
            }
            if clause
                .get(skill_index + 1)
                .is_some_and(|token| token == "section")
                && clause[skill_index + 2..]
                    .iter()
                    .any(|token| token == "miniquest")
            {
                return None;
            }
            let previous = numbers
                .iter()
                .rev()
                .find(|(number_index, _)| *number_index < skill_index);
            let next = numbers
                .iter()
                .find(|(number_index, _)| *number_index > skill_index);
            let level = match (previous, next) {
                (Some((number_index, level)), _) if *number_index + 1 == skill_index => *level,
                (_, Some((number_index, level)))
                    if clause[skill_index + 1..*number_index]
                        .iter()
                        .all(|token| matches!(token.as_str(), "level" | "of" | "at" | "least"))
                        || *number_index - skill_index <= 3
                            && !clause[skill_index + 1..*number_index].iter().any(|token| {
                                matches!(token.as_str(), "and" | "along" | "with" | "plus")
                            }) =>
                {
                    *level
                }
                (Some((_, level)), _) => *level,
                (None, Some((_, level))) => *level,
                (None, None) => return None,
            };
            if kind == "skill" && level > 99 {
                return None;
            }
            Some(Requirement {
                kind,
                name: name.to_string(),
                level: Some(level),
                context: if deadman { "deadman" } else { "main" },
                basis: if effective_minimum(evidence) {
                    "effective_minimum"
                } else {
                    "direct"
                },
                group_id: 0,
                trimmed_only: false,
                evidence: evidence.to_string(),
            })
        })
        .collect()
}

fn diary_requirement(evidence: &str, tokens: &[String]) -> Option<Requirement> {
    let diary = tokens.iter().position(|token| token == "diary")?;
    let start = tokens[..diary]
        .iter()
        .rposition(|token| matches!(token.as_str(), "easy" | "medium" | "hard" | "elite"))?;
    let name = tokens[start..=diary]
        .iter()
        .map(|word| {
            let mut characters = word.chars();
            characters
                .next()
                .map(|first| first.to_uppercase().collect::<String>() + characters.as_str())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ");
    Some(Requirement {
        kind: "diary",
        name,
        level: None,
        context: "main",
        basis: "direct",
        group_id: 0,
        trimmed_only: false,
        evidence: evidence.to_string(),
    })
}

fn alternative_requirements(
    evidence: &str,
    normalized: &str,
    progression_titles: &[(String, &'static str)],
) -> Vec<Requirement> {
    if !normalized.contains("only equipable if") || !normalized.contains(" or ") {
        return Vec::new();
    }
    let Some((left, right)) = split_once_ascii_case(evidence, " or ") else {
        return Vec::new();
    };
    [left, right]
        .into_iter()
        .filter_map(|side| {
            let candidate = split_once_ascii_case(side, "completed ")
                .map(|(_, value)| value)
                .unwrap_or(side)
                .trim_matches(|character: char| {
                    character.is_whitespace()
                        || character.is_ascii_punctuation() && character != '\''
                })
                .replace(" 's", "'s");
            if candidate.is_empty() {
                return None;
            }
            let progression = progression_titles.iter().find(|(title, _)| {
                progression_aliases(title)
                    .iter()
                    .any(|alias| words(&candidate).join(" ").contains(alias))
            });
            Some(Requirement {
                kind: progression.map_or("unlock", |(_, kind)| *kind),
                name: progression.map_or_else(|| candidate, |(title, _)| title.clone()),
                level: None,
                context: "main",
                basis: "direct",
                group_id: 1,
                trimmed_only: false,
                evidence: evidence.to_string(),
            })
        })
        .collect()
}

fn progression_aliases(title: &str) -> Vec<String> {
    let mut aliases = vec![words(title).join(" ")];
    if let Some(display) = title.rsplit('/').next()
        && display != title
    {
        aliases.push(words(display).join(" "));
    }
    aliases
}

fn split_once_ascii_case<'a>(value: &'a str, needle: &str) -> Option<(&'a str, &'a str)> {
    let index = value.to_ascii_lowercase().find(needle)?;
    Some((&value[..index], &value[index + needle.len()..]))
}

fn other_action_word(token: &str) -> bool {
    matches!(
        token,
        "use" | "used" | "using" | "create" | "created" | "make" | "made" | "obtain" | "obtained"
    )
}

fn insert_requirement(requirements: &mut RequirementMap, requirement: Requirement) -> Result<()> {
    let key = (
        requirement.kind,
        requirement.name.clone(),
        requirement.context,
        requirement.group_id,
        requirement.trimmed_only,
    );
    if let Some(existing) = requirements.get(&key)
        && existing.level != requirement.level
    {
        match (
            effective_minimum(&existing.evidence),
            effective_minimum(&requirement.evidence),
        ) {
            (false, true) => {
                requirements.insert(key, requirement);
                return Ok(());
            }
            (true, false) => return Ok(()),
            _ => {}
        }
        bail!(
            "conflicting {} requirement for {}: {:?} from {:?} and {:?} from {:?}",
            requirement.kind,
            requirement.name,
            existing.level,
            existing.evidence,
            requirement.level,
            requirement.evidence,
        );
    }
    requirements.entry(key).or_insert(requirement);
    Ok(())
}

fn effective_minimum(evidence: &str) -> bool {
    let tokens = words(evidence);
    tokens.iter().any(|token| token == "minimum")
        && (tokens
            .iter()
            .any(|token| matches!(token.as_str(), "effective" | "effectively"))
            || tokens.iter().any(|token| token == "acquire")
                && tokens.iter().any(|token| token == "equip"))
}

fn words(value: &str) -> Vec<String> {
    value
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

fn equip_context(tokens: &[String]) -> bool {
    tokens.iter().any(|token| equip_word(token))
}

fn equip_word(token: &str) -> bool {
    matches!(
        token,
        "wear" | "worn" | "wield" | "wielded" | "equip" | "equipable" | "equipped" | "equipping"
    )
}

fn requirement_context(tokens: &[String]) -> bool {
    tokens.iter().any(|token| requirement_word(token))
        || tokens.windows(2).any(|window| {
            matches!(window, [player, verb] if matches!(player.as_str(), "player" | "players") && matches!(verb.as_str(), "have" | "has"))
        })
        || tokens.windows(3).any(|window| {
            matches!(window, [player, who, verb] if matches!(player.as_str(), "player" | "players") && who == "who" && matches!(verb.as_str(), "have" | "has"))
        })
}

fn requirement_word(token: &str) -> bool {
    matches!(
        token,
        "require"
            | "requires"
            | "required"
            | "requiring"
            | "requirement"
            | "requirements"
            | "must"
            | "need"
            | "needs"
            | "needed"
            | "minimum"
    )
}

fn acquisition_context(tokens: &[String]) -> bool {
    tokens.iter().any(|token| {
        matches!(
            token.as_str(),
            "acquire"
                | "acquired"
                | "obtain"
                | "obtained"
                | "purchase"
                | "purchased"
                | "buy"
                | "bought"
        )
    })
}

fn completion_context(tokens: &[String]) -> bool {
    tokens.iter().any(|token| {
        matches!(
            token.as_str(),
            "complete" | "completed" | "completing" | "completion" | "finish" | "finished"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_three_salamander_requirements() {
        let content = "The red salamander requires level 60 Ranged, 60 Attack, and 60 Magic to wield.\n\nTemplate:Infobox Item: equipable=Yes | id=10147 | name=Red salamander | options=Wield, Release";
        let (items, requirements, status) = parse_page("Red salamander", content, &[]).unwrap();
        assert_eq!(items[0].id, 10147);
        assert_eq!(status, "parsed");
        assert_eq!(
            requirements
                .iter()
                .map(|requirement| (requirement.name.as_str(), requirement.level))
                .collect::<Vec<_>>(),
            vec![
                ("Attack", Some(60)),
                ("Magic", Some(60)),
                ("Ranged", Some(60)),
            ]
        );
    }

    #[test]
    fn parses_green_dhide_skills_and_quest() {
        let content = "In order to wear the body, a player must first have completed the Dragon Slayer I quest and have level 40 Ranged and Defence.\n\nTemplate:Infobox Item: equipable=Yes | id=1135 | name=Green d'hide body | options=Wear, Drop";
        let titles = vec![("Dragon Slayer I".to_string(), "quest")];
        let (_, requirements, status) = parse_page("Green d'hide body", content, &titles).unwrap();
        assert_eq!(status, "parsed");
        assert_eq!(requirements.len(), 3);
        assert!(requirements.iter().any(|requirement| {
            requirement.kind == "skill"
                && requirement.name == "Defence"
                && requirement.level == Some(40)
        }));
        assert!(requirements.iter().any(|requirement| {
            requirement.kind == "skill"
                && requirement.name == "Ranged"
                && requirement.level == Some(40)
        }));
        assert!(requirements.iter().any(|requirement| {
            requirement.kind == "quest" && requirement.name == "Dragon Slayer I"
        }));
    }

    #[test]
    fn does_not_parse_skill_names_inside_progression_titles() {
        let content = "It requires 40 Defence and completion of the quest Dragon Slayer I to equip.\n\nTemplate:Infobox Item: equipable=Yes | id=1127 | name=Rune platebody | options=Wear, Drop";
        let titles = vec![("Dragon Slayer I".to_string(), "quest")];
        let (_, requirements, _) = parse_page("Rune platebody", content, &titles).unwrap();
        assert!(
            !requirements
                .iter()
                .any(|requirement| { requirement.kind == "skill" && requirement.name == "Slayer" })
        );
    }

    #[test]
    fn excludes_progression_catalog_subpages() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE pages(source_kind TEXT, namespace INTEGER, title TEXT, categories_json TEXT);
                 INSERT INTO pages VALUES
                   ('wiki', 0, 'Quests/Master', '[\"Quests\"]'),
                   ('wiki', 0, 'Quest items/Achievement Diary', '[\"Quests\"]'),
                   ('wiki', 0, 'Recipe for Disaster/Defeating the Culinaromancer', '[\"Quests\"]');",
            )
            .unwrap();
        let titles = progression_titles(&connection).unwrap();
        assert_eq!(
            titles,
            vec![(
                "Recipe for Disaster/Defeating the Culinaromancer".to_string(),
                "quest"
            )]
        );
    }

    #[test]
    fn ignores_requirements_belonging_to_a_negated_comparator() {
        let content = "The dragon cane requires level 60 Attack to wield. This weapon has similar stats to the dragon mace (including the +5 Prayer bonus), although no quest completion is required to wield the dragon cane (while completion of Heroes' Quest is needed to wield the dragon mace).\n\nTemplate:Infobox Item: equipable=Yes | id=12373 | name=Dragon cane | options=Wield, Drop";
        let titles = vec![("Heroes' Quest".to_string(), "quest")];
        let (_, requirements, _) = parse_page("Dragon cane", content, &titles).unwrap();
        assert_eq!(
            requirements
                .iter()
                .map(|requirement| (
                    requirement.kind,
                    requirement.name.as_str(),
                    requirement.level
                ))
                .collect::<Vec<_>>(),
            vec![("skill", "Attack", Some(60))]
        );
    }

    #[test]
    fn ignores_equip_requirements_for_a_different_item() {
        let content = "To equip the shield, players must have completed the Elemental Workshop I quest. Upon completion of Elemental Workshop II, players can equip the mind shield for an additional +3 Magic defence over the elemental shield.\n\nTemplate:Infobox Item: equipable=Yes | id=2890 | name=Elemental shield | options=Wield, Drop";
        let titles = vec![
            ("Elemental Workshop I".to_string(), "quest"),
            ("Elemental Workshop II".to_string(), "quest"),
        ];
        let (_, requirements, _) = parse_page("Elemental shield", content, &titles).unwrap();
        assert!(requirements.iter().any(|requirement| {
            requirement.kind == "quest" && requirement.name == "Elemental Workshop I"
        }));
        assert!(!requirements.iter().any(|requirement| {
            requirement.kind == "quest" && requirement.name == "Elemental Workshop II"
        }));
    }

    #[test]
    fn parses_have_requirements_without_incidental_unlocks() {
        let content = "It can only be wielded by players who have 60 Attack and have completed the Monkey Madness I quest. A monkeyspeak amulet does not need to be worn to trade, unless the player has completed Monkey Madness II.\n\nTemplate:Infobox Item: equipable=Yes | id=4587 | name=Dragon scimitar | options=Wield, Drop";
        let titles = vec![
            ("Monkey Madness I".to_string(), "quest"),
            ("Monkey Madness II".to_string(), "quest"),
        ];
        let (_, requirements, _) = parse_page("Dragon scimitar", content, &titles).unwrap();
        assert!(requirements.iter().any(|requirement| {
            requirement.kind == "skill"
                && requirement.name == "Attack"
                && requirement.level == Some(60)
        }));
        assert!(requirements.iter().any(|requirement| {
            requirement.kind == "quest" && requirement.name == "Monkey Madness I"
        }));
        assert!(!requirements.iter().any(|requirement| {
            requirement.kind == "quest" && requirement.name == "Monkey Madness II"
        }));
    }

    #[test]
    fn pairs_each_skill_with_its_explicit_following_level() {
        let content = "The shield requires a Magic level of 70 and a Defence level of at least 75 to equip, as well as having started Dragon Slayer I.\n\nTemplate:Infobox Item: equipable=Yes | id=21633 | name=Ancient wyvern shield | options=Wield, Drop";
        let titles = vec![("Dragon Slayer I".to_string(), "quest")];
        let (_, requirements, _) = parse_page("Ancient wyvern shield", content, &titles).unwrap();
        for (name, level) in [("Magic", 70), ("Defence", 75)] {
            assert!(requirements.iter().any(|requirement| {
                requirement.kind == "skill"
                    && requirement.name == name
                    && requirement.level == Some(level)
            }));
        }
    }

    #[test]
    fn ignores_skill_words_used_as_stat_descriptions() {
        let content = "The mace has a 1 tick slower attack speed and requires level 15 Attack and level 25 Prayer to wield.\n\nTemplate:Infobox Item: equipable=Yes | id=11061 | name=Ancient mace | options=Wield, Drop";
        let (_, requirements, _) = parse_page("Ancient mace", content, &[]).unwrap();
        for (name, level) in [("Attack", 15), ("Prayer", 25)] {
            assert!(requirements.iter().any(|requirement| {
                requirement.kind == "skill"
                    && requirement.name == name
                    && requirement.level == Some(level)
            }));
        }
    }

    #[test]
    fn ignores_equipped_stat_bonus_sentences() {
        let content = "The full set has a +20 Magic attack bonus when the blessing is equipped.\n\nTemplate:Infobox Item: equipable=Yes | id=1 | name=Crystal blessing | options=Wear, Drop";
        let (_, requirements, status) = parse_page("Crystal blessing", content, &[]).unwrap();
        assert!(requirements.is_empty());
        assert_eq!(status, "unresolved");
    }

    #[test]
    fn retains_requirements_trailing_the_equip_phrase() {
        let content = "These boots are a piece of Ranged armour; at least 40 Defence is required to wear these boots, along with 70 Ranged.\n\nTemplate:Infobox Item: equipable=Yes | id=19921 | name=Ancient d'hide boots | options=Wear, Drop";
        let (_, requirements, _) = parse_page("Ancient d'hide boots", content, &[]).unwrap();
        for (name, level) in [("Defence", 40), ("Ranged", 70)] {
            assert!(requirements.iter().any(|requirement| {
                requirement.kind == "skill"
                    && requirement.name == name
                    && requirement.level == Some(level)
            }));
        }
    }

    #[test]
    fn does_not_treat_descriptive_made_as_a_requirement_action() {
        let content = "An adamant halberd is made from adamantite, requiring level 30 Attack and 15 Strength to wield. Although completion of Regicide is required to wield the dragon halberd, it is not a requirement to wield other halberds.\n\nTemplate:Infobox Item: equipable=Yes | id=3200 | name=Adamant halberd | options=Wield, Drop";
        let titles = vec![("Regicide".to_string(), "quest")];
        let (_, requirements, _) = parse_page("Adamant halberd", content, &titles).unwrap();
        for (name, level) in [("Attack", 30), ("Strength", 15)] {
            assert!(requirements.iter().any(|requirement| {
                requirement.kind == "skill"
                    && requirement.name == name
                    && requirement.level == Some(level)
            }));
        }
        assert!(
            !requirements.iter().any(|requirement| {
                requirement.kind == "quest" && requirement.name == "Regicide"
            })
        );
    }

    #[test]
    fn prefers_the_effective_minimum_equip_level() {
        let content = "While only 1 Defence is required to wear these gloves, it is impossible to equip them at this level. This effectively makes 34 Defence the minimum required level to wear the gloves legitimately.\n\nTemplate:Infobox Item: equipable=Yes | id=7460 | name=Rune gloves | options=Wear, Drop";
        let (_, requirements, _) = parse_page("Rune gloves", content, &[]).unwrap();
        let defence = requirements
            .iter()
            .find(|requirement| requirement.name == "Defence")
            .unwrap();
        assert_eq!(defence.level, Some(34));
    }

    #[test]
    fn skips_broken_version_with_drop_only_options() {
        let content = "It requires 70 Defence to wear.\n\nTemplate:Infobox Item: equipable=Yes | id1=1 | id2=2 | name1=Armour | name2=Armour (broken) | options1=Wear, Drop | options2=Drop";
        let (items, _, _) = parse_page("Armour", content, &[]).unwrap();
        assert_eq!(
            items,
            vec![EquipmentItem {
                id: 1,
                name: "Armour".to_string()
            }]
        );
    }

    #[test]
    fn excludes_use_requirement_after_wield_clause() {
        let content = "It requires 65 Attack to wield and 61 Woodcutting to use.\n\nTemplate:Infobox Item: equipable=Yes | id=1 | name=3rd age axe";
        let (_, requirements, _) = parse_page("3rd age axe", content, &[]).unwrap();
        assert_eq!(requirements.len(), 1);
        assert_eq!(requirements[0].name, "Attack");
        assert_eq!(requirements[0].level, Some(65));
    }

    #[test]
    fn ignores_rendered_infobox_text() {
        let content = "Columns: Pre-nature amulet [colspan=20] Options [colspan=7]: Wear, Drop [colspan=13] Examine [colspan=7]: Strung with the root of a Magic Tree; needs enchanting.\n\nThe pre-nature amulet is the precursor to the amulet of nature.\n\nTemplate:Infobox Item: equipable=Yes | id=6041 | name=Pre-nature amulet | options=Wear, Drop";
        let (_, requirements, status) = parse_page("Pre-nature amulet", content, &[]).unwrap();
        assert!(requirements.is_empty());
        assert_eq!(status, "unresolved");
    }

    #[test]
    fn parses_negated_requirement_lists_as_explicit_none() {
        let content = "Unlike the rest of the Void Knight equipment, players do not require at least 42 Attack, Strength, Defence, Hitpoints, along with 22 Prayer to buy and wear one.\n\nTemplate:Infobox Item: equipable=Yes | id=11673 | name=Void seal(1) | options=Wear, Rub, Drop";
        let (_, requirements, status) = parse_page("Void seal", content, &[]).unwrap();
        assert!(requirements.is_empty());
        assert_eq!(status, "explicit_none");
    }

    #[test]
    fn parses_no_level_requirements_as_explicit_none() {
        for (title, sentence) in [
            (
                "Guthix mjolnir",
                "There are no level requirements to wield this item",
            ),
            (
                "Bronze dagger",
                "There are no level requirements to create or wield a bronze dagger",
            ),
        ] {
            let content = format!(
                "{sentence}.\n\nTemplate:Infobox Item: equipable=Yes | id=1 | name={title} | options=Wield, Drop"
            );
            let (_, requirements, status) = parse_page(title, &content, &[]).unwrap();
            assert!(requirements.is_empty(), "{title}: {requirements:?}");
            assert_eq!(status, "explicit_none", "{title}");
        }
    }

    #[test]
    fn applies_a_level_to_a_skill_list_until_the_next_level() {
        let content = "A player must have at least 42 Attack, Strength, Defence, Hitpoints, Ranged, and Magic, along with 22 Prayer to buy and wear one.\n\nTemplate:Infobox Item: equipable=Yes | id=8839 | name=Void knight top | options=Wear, Drop";
        let (_, requirements, _) = parse_page("Void knight top", content, &[]).unwrap();
        let levels = requirements
            .iter()
            .map(|requirement| (requirement.name.as_str(), requirement.level.unwrap()))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            levels,
            BTreeMap::from([
                ("Attack", 42),
                ("Defence", 42),
                ("Hitpoints", 42),
                ("Magic", 42),
                ("Prayer", 22),
                ("Ranged", 42),
                ("Strength", 42),
            ])
        );
    }

    #[test]
    fn separates_use_requirements_before_the_equip_clause() {
        for (title, sentence, id, level) in [
            (
                "Rune pickaxe",
                "The rune pickaxe requires 41 Mining to use, and 40 Attack to equip",
                1275,
                40,
            ),
            (
                "Dragon harpoon",
                "The dragon harpoon requires 61 Fishing to use and 60 Attack to wield",
                21028,
                60,
            ),
        ] {
            let content = format!(
                "{sentence}.\n\nTemplate:Infobox Item: equipable=Yes | id={id} | name={title} | options=Wield, Drop"
            );
            let (_, requirements, _) = parse_page(title, &content, &[]).unwrap();
            assert_eq!(requirements.len(), 1, "{title}: {requirements:?}");
            assert_eq!(requirements[0].name, "Attack", "{title}");
            assert_eq!(requirements[0].level, Some(level), "{title}");
        }
    }

    #[test]
    fn separates_creation_requirements_from_wear_requirements() {
        let content = "It is made using Crafting, which requires level 37 Crafting; players must have level 24 Hunter to wear it.\n\nTemplate:Infobox Item: equipable=Yes | id=10132 | name=Strung rabbit foot | options=Wear, Drop";
        let (_, requirements, _) = parse_page("Strung rabbit foot", content, &[]).unwrap();
        assert_eq!(requirements.len(), 1, "{requirements:?}");
        assert_eq!(requirements[0].name, "Hunter");
        assert_eq!(requirements[0].level, Some(24));
    }

    #[test]
    fn preserves_punctuation_inside_progression_titles() {
        let content = "The white warhammer requires 10 Strength and the completion of the Wanted! quest to wield.\n\nTemplate:Infobox Item: equipable=Yes | id=6613 | name=White warhammer | options=Wield, Drop";
        let titles = vec![("Wanted!".to_string(), "quest")];
        let (_, requirements, status) = parse_page("White warhammer", content, &titles).unwrap();
        assert_eq!(status, "parsed");
        assert!(requirements.iter().any(|requirement| {
            requirement.kind == "skill"
                && requirement.name == "Strength"
                && requirement.level == Some(10)
        }));
        assert!(
            requirements
                .iter()
                .any(|requirement| requirement.kind == "quest" && requirement.name == "Wanted!")
        );
    }

    #[test]
    fn parses_combat_level_separately_from_skills() {
        let content = "It requires 20 Strength, 10 Defence, and 40 Combat to equip.\n\nTemplate:Infobox Item: equipable=Yes | id=8921 | name=Black mask | options=Wear, Drop";
        let (_, requirements, _) = parse_page("Black mask", content, &[]).unwrap();
        assert!(requirements.iter().any(|requirement| {
            requirement.kind == "combat_level"
                && requirement.name == "Combat"
                && requirement.level == Some(40)
        }));
    }

    #[test]
    fn parses_named_miniquest_sections_without_a_bogus_skill_level() {
        let content = "The dragon hasta requires 60 Attack and completion of the Smithing section of the Barbarian Training miniquest to be wielded.\n\nTemplate:Infobox Item: equipable=Yes | id=22731 | name=Dragon hasta | options=Wield, Drop";
        let titles = vec![("Barbarian Training".to_string(), "miniquest")];
        let (_, requirements, _) = parse_page("Dragon hasta", content, &titles).unwrap();
        assert!(requirements.iter().any(|requirement| {
            requirement.kind == "skill"
                && requirement.name == "Attack"
                && requirement.level == Some(60)
        }));
        assert!(
            !requirements
                .iter()
                .any(|requirement| requirement.kind == "skill" && requirement.name == "Smithing")
        );
        assert!(requirements.iter().any(|requirement| {
            requirement.kind == "miniquest_section"
                && requirement.name == "Barbarian Training: Smithing"
        }));
    }

    #[test]
    fn parses_diary_gates_alongside_skill_levels() {
        let content = "It requires completion of the Hard Western Provinces Diary to wield, as well as 70 Attack, 35 Strength and 50 Agility.\n\nTemplate:Infobox Item: equipable=Yes | id=23987 | name=Crystal halberd | options=Wield, Check, Revert, Drop";
        let (_, requirements, _) = parse_page("Crystal halberd", content, &[]).unwrap();
        assert!(requirements.iter().any(|requirement| {
            requirement.kind == "diary" && requirement.name == "Hard Western Provinces Diary"
        }));
        for (name, level) in [("Attack", 70), ("Strength", 35), ("Agility", 50)] {
            assert!(requirements.iter().any(|requirement| {
                requirement.kind == "skill"
                    && requirement.name == name
                    && requirement.level == Some(level)
            }));
        }
    }

    #[test]
    fn ignores_diaries_that_only_describe_an_equipped_perk() {
        let content = "Zahur will create unfinished potions when the player has the cape equipped, the same perk from completing the Hard Desert Diary.\n\nTemplate:Infobox Item: equipable=Yes | id=9774 | name=Herblore cape | options=Wear, Drop";
        let (_, requirements, status) = parse_page("Herblore cape", content, &[]).unwrap();
        assert!(requirements.is_empty());
        assert_eq!(status, "unresolved");
    }

    #[test]
    fn parses_dynamic_all_current_quests_eligibility() {
        let content = "The quest point cape can be obtained by players who have completed all quests (excluding miniquests). Should a new quest be released, the cape will be unequipped, and the player will not be able to equip it until the new quest is completed.\n\nTemplate:Infobox Item: equipable=Yes | id=9813 | name=Quest point cape | tradeable=No | options=Wear, Drop";
        let (_, requirements, status) = parse_page("Quest point cape", content, &[]).unwrap();
        assert_eq!(status, "parsed");
        assert!(requirements.iter().any(|requirement| {
            requirement.kind == "quest_set" && requirement.name == "All current quests"
        }));
    }

    #[test]
    fn parses_boss_kill_equip_gates_without_creation_levels() {
        let content = "The ring requires players to have killed the Whisperer at least once to wear. The ring requires level 90 Magic and 80 Crafting to create.\n\nTemplate:Infobox Item: equipable=Yes | id=28316 | name=Bellator ring | options=Wear, Drop";
        let (_, requirements, _) = parse_page("Bellator ring", content, &[]).unwrap();
        assert!(requirements.iter().any(|requirement| {
            requirement.kind == "boss_kill"
                && requirement.name == "The Whisperer"
                && requirement.level == Some(1)
        }));
        assert!(
            !requirements.iter().any(|requirement| {
                requirement.name == "Magic" || requirement.name == "Crafting"
            })
        );
    }

    #[test]
    fn parses_skill_mastery_acquisition_requirements() {
        let content = "The Attack cape can be bought by any player who has achieved level 99 in the Attack skill.\n\nTemplate:Infobox Item: equipable=Yes | id=9747 | name=Attack cape | tradeable=No | options=Wear, Drop";
        let (_, requirements, status) = parse_page("Attack cape", content, &[]).unwrap();
        assert_eq!(status, "parsed");
        assert!(requirements.iter().any(|requirement| {
            requirement.kind == "skill"
                && requirement.name == "Attack"
                && requirement.level == Some(99)
                && requirement.basis == "acquisition"
        }));
    }

    #[test]
    fn parses_all_skills_mastery_requirements() {
        let content = "The max cape is available to players who have attained level 99 in all 24 skills.\n\nTemplate:Infobox Item: equipable=Yes | id=13280 | name=Max cape | tradeable=No | options=Wear, Drop";
        let (_, requirements, status) = parse_page("Max cape", content, &[]).unwrap();
        assert_eq!(status, "parsed");
        assert!(requirements.iter().any(|requirement| {
            requirement.kind == "skill_set"
                && requirement.name == "All skills"
                && requirement.level == Some(99)
        }));
    }

    #[test]
    fn parses_dynamic_track_and_diary_sets() {
        let content = "The cape is obtainable by players who have unlocked all non-holiday music tracks. To trim the cape, players must unlock all holiday event tracks and have completed all quests and Achievement Diaries.\n\nTemplate:Infobox Item: equipable=Yes | id1=13221 | name1=Music cape | options1=Wear, Drop | id2=13222 | name2=Music cape(t) | options2=Wear, Drop";
        let (_, requirements, _) = parse_page("Music cape", content, &[]).unwrap();
        for (kind, name, trimmed_only) in [
            ("music_track_set", "All non-holiday music tracks", false),
            ("music_track_set", "All holiday event tracks", true),
            ("quest_set", "All current quests", true),
            ("diary_set", "All Achievement Diaries", true),
        ] {
            assert!(requirements.iter().any(|requirement| {
                requirement.kind == kind
                    && requirement.name == name
                    && requirement.trimmed_only == trimmed_only
            }));
        }
    }

    #[test]
    fn parses_effective_minimums_and_alternative_unlocks() {
        let content = "While only 1 Defence is required to wear these gloves, it is impossible to equip them at this level. Specifically, the gloves are only equipable if the player has already completed Daero's training OR Defeating the Culinaromancer. This effectively makes 34 Defence the minimum required level to wear rune gloves legitimately.\n\nTemplate:Infobox Item: equipable=Yes | id=7460 | name=Rune gloves | tradeable=No | options=Wear, Drop";
        let titles = vec![(
            "Recipe for Disaster/Defeating the Culinaromancer".to_string(),
            "quest",
        )];
        let (_, requirements, _) = parse_page("Rune gloves", content, &titles).unwrap();
        assert!(requirements.iter().any(|requirement| {
            requirement.kind == "skill"
                && requirement.name == "Defence"
                && requirement.level == Some(34)
                && requirement.basis == "effective_minimum"
                && requirement.group_id == 0
        }));
        assert!(
            requirements.iter().any(|requirement| {
                requirement.kind == "unlock"
                    && requirement.name == "Daero's training"
                    && requirement.group_id == 1
            }),
            "{requirements:?}"
        );
        assert!(requirements.iter().any(|requirement| {
            requirement.kind == "quest"
                && requirement.name == "Recipe for Disaster/Defeating the Culinaromancer"
                && requirement.group_id == 1
        }));
    }

    #[test]
    fn keeps_normal_deadman_and_lms_requirements_separate() {
        let content = "Barrows gloves can be purchased after completing the entire Recipe for Disaster quest. The minimum Defence level a player can have to acquire and equip these gloves is 35 when all quest requirements are calculated. In limited time modes like Deadman Mode, these gloves can be used by accounts with level 1 Defence after completing Recipe for Disaster via a quest lamp.\n\nTemplate:Infobox Item: equipable=Yes | id=7462 | name=Barrows gloves | tradeable=No | options=Wear, Drop";
        let titles = vec![("Recipe for Disaster".to_string(), "quest")];
        let (_, requirements, _) = parse_page("Barrows gloves", content, &titles).unwrap();
        assert!(
            requirements.iter().any(|requirement| {
                requirement.kind == "skill"
                    && requirement.name == "Defence"
                    && requirement.level == Some(35)
                    && requirement.context == "main"
                    && requirement.basis == "effective_minimum"
            }),
            "{requirements:?}"
        );
        assert!(requirements.iter().any(|requirement| {
            requirement.kind == "skill"
                && requirement.name == "Defence"
                && requirement.level == Some(1)
                && requirement.context == "deadman"
        }));
        assert!(requirements.iter().any(|requirement| {
            requirement.kind == "quest"
                && requirement.name == "Recipe for Disaster"
                && requirement.context == "main"
                && requirement.basis == "acquisition"
        }));

        let lms = "Like all items within the minigame they have no requirements to wield.\n\nTemplate:Infobox Item: equipable=Yes | id=23593 | name=Barrows gloves | options=Wear, Drop";
        let (_, requirements, status) =
            parse_page("Barrows gloves (Last Man Standing)", lms, &[]).unwrap();
        assert!(requirements.is_empty());
        assert_eq!(status, "explicit_none");
    }

    #[test]
    #[ignore = "requires WIKI_DATABASE_PATH"]
    fn parses_requirements_from_the_real_wiki_database() {
        let path = std::env::var_os("WIKI_DATABASE_PATH").unwrap();
        let connection = Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .unwrap();
        let titles = progression_titles(&connection).unwrap();

        for (title, item_id, level) in [
            ("Swamp lizard", 10149, 30),
            ("Orange salamander", 10146, 50),
            ("Red salamander", 10147, 60),
            ("Black salamander", 10148, 70),
        ] {
            let content: String = connection
                .query_row(
                    "SELECT s.content FROM pages p JOIN sections s ON s.page_id = p.id AND s.section_index = 0 WHERE p.title = ?1",
                    [title],
                    |row| row.get(0),
                )
                .unwrap();
            let (items, requirements, status) = parse_page(title, &content, &titles).unwrap();
            assert!(items.iter().any(|item| item.id == item_id), "{title}");
            assert_eq!(status, "parsed", "{title}");
            for skill in ["Attack", "Magic", "Ranged"] {
                assert!(
                    requirements.iter().any(|requirement| {
                        requirement.kind == "skill"
                            && requirement.name == skill
                            && requirement.level == Some(level)
                    }),
                    "{title}: {skill} {level}"
                );
            }
        }

        let content: String = connection
            .query_row(
                "SELECT s.content FROM pages p JOIN sections s ON s.page_id = p.id AND s.section_index = 0 WHERE p.title = 'Green d''hide body'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let (items, requirements, status) =
            parse_page("Green d'hide body", &content, &titles).unwrap();
        assert!(items.iter().any(|item| item.id == 1135));
        assert_eq!(status, "parsed");
        for (kind, name, level) in [
            ("skill", "Defence", Some(40)),
            ("skill", "Ranged", Some(40)),
            ("quest", "Dragon Slayer I", None),
        ] {
            assert!(requirements.iter().any(|requirement| {
                requirement.kind == kind && requirement.name == name && requirement.level == level
            }));
        }
    }

    #[test]
    #[ignore = "requires WIKI_DATABASE_PATH"]
    fn parses_the_real_adversarial_equipment_pages() {
        let path = std::env::var_os("WIKI_DATABASE_PATH").unwrap();
        let connection = Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .unwrap();
        let titles = progression_titles(&connection).unwrap();
        let parse = |title: &str| {
            let content: String = connection
                .query_row(
                    "SELECT s.content FROM pages p JOIN sections s ON s.page_id = p.id AND s.section_index = 0 WHERE p.title = ?1",
                    [title],
                    |row| row.get(0),
                )
                .unwrap();
            parse_page(title, &content, &titles).unwrap()
        };

        let (_, requirements, status) = parse("Void seal");
        assert_eq!(status, "explicit_none");
        assert!(requirements.is_empty());

        let (_, requirements, _) = parse("Void knight top");
        for (name, level) in [
            ("Attack", 42),
            ("Strength", 42),
            ("Defence", 42),
            ("Hitpoints", 42),
            ("Ranged", 42),
            ("Magic", 42),
            ("Prayer", 22),
        ] {
            assert!(requirements.iter().any(|requirement| {
                requirement.name == name && requirement.level == Some(level)
            }));
        }

        for (title, skill, level, excluded) in [
            ("Rune pickaxe", "Attack", 40, "Mining"),
            ("Dragon harpoon", "Attack", 60, "Fishing"),
        ] {
            let (_, requirements, _) = parse(title);
            assert!(requirements.iter().any(|requirement| {
                requirement.name == skill && requirement.level == Some(level)
            }));
            assert!(
                !requirements
                    .iter()
                    .any(|requirement| requirement.name == excluded)
            );
        }

        let (_, requirements, _) = parse("Dragon hasta");
        assert!(requirements.iter().any(|requirement| {
            requirement.kind == "miniquest_section"
                && requirement.name == "Barbarian Training: Smithing"
        }));
        assert!(
            !requirements
                .iter()
                .any(|requirement| requirement.kind == "skill" && requirement.name == "Smithing")
        );

        let (_, requirements, _) = parse("White warhammer");
        assert!(
            requirements
                .iter()
                .any(|requirement| requirement.kind == "quest" && requirement.name == "Wanted!")
        );

        let (_, requirements, _) = parse("Black mask");
        assert!(requirements.iter().any(|requirement| {
            requirement.kind == "combat_level" && requirement.level == Some(40)
        }));

        let (_, requirements, _) = parse("Crystal halberd");
        assert!(requirements.iter().any(|requirement| {
            requirement.kind == "diary" && requirement.name == "Hard Western Provinces Diary"
        }));

        let (_, requirements, _) = parse("Rune gloves");
        assert!(requirements.iter().any(|requirement| {
            requirement.name == "Defence"
                && requirement.level == Some(34)
                && requirement.basis == "effective_minimum"
        }));
        assert!(
            requirements
                .iter()
                .filter(|requirement| requirement.group_id == 1)
                .count()
                >= 2
        );
        assert!(
            requirements
                .iter()
                .any(|requirement| requirement.name == "Daero's training")
        );

        let (_, requirements, _) = parse("Barrows gloves");
        for (context, level) in [("main", 35), ("deadman", 1)] {
            assert!(requirements.iter().any(|requirement| {
                requirement.name == "Defence"
                    && requirement.level == Some(level)
                    && requirement.context == context
            }));
        }

        let (_, requirements, _) = parse("Quest point cape");
        assert!(
            requirements
                .iter()
                .any(|requirement| requirement.kind == "quest_set")
        );

        let (_, requirements, status) = parse("Pre-nature amulet");
        assert_eq!(status, "unresolved");
        assert!(requirements.is_empty());
    }

    #[test]
    #[ignore = "requires WIKI_DATABASE_PATH"]
    fn audits_every_equipment_page_in_the_real_wiki_database() {
        let path = std::env::var_os("WIKI_DATABASE_PATH").unwrap();
        let connection = Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .unwrap();
        let titles = progression_titles(&connection).unwrap();
        let mut statement = connection
            .prepare(
                "SELECT p.title, s.content
                 FROM pages p
                 JOIN sections s ON s.page_id = p.id AND s.section_index = 0
                 WHERE p.categories_json LIKE '%\"Equipable items\"%'",
            )
            .unwrap();
        let pages = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        let mut item_pages = BTreeMap::new();
        let mut parsed_items = 0;
        let mut requirements = 0;
        for (title, content) in pages {
            let (items, parsed, status) = parse_page(&title, &content, &titles)
                .unwrap_or_else(|error| panic!("{title}: {error:#}"));
            requirements += parsed.len() * items.len();
            if status == "parsed" {
                parsed_items += items.len();
            }
            for item in items {
                assert_eq!(
                    item_pages.insert(item.id, title.clone()),
                    None,
                    "duplicate item ID {}",
                    item.id
                );
            }
        }
        eprintln!(
            "audited {} equipment items: {} parsed, {} requirements",
            item_pages.len(),
            parsed_items,
            requirements
        );
        assert!(!item_pages.is_empty());
        assert!(parsed_items > 0);
        assert!(requirements > 0);
    }

    #[test]
    #[ignore = "requires WIKI_DATABASE_PATH"]
    fn parses_real_followup_edge_cases() {
        let path = std::env::var_os("WIKI_DATABASE_PATH").unwrap();
        let connection = Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .unwrap();
        let titles = progression_titles(&connection).unwrap();
        let parse = |title: &str| {
            let content: String = connection
                .query_row(
                    "SELECT s.content FROM pages p JOIN sections s ON s.page_id = p.id AND s.section_index = 0 WHERE p.title = ?1",
                    [title],
                    |row| row.get(0),
                )
                .unwrap();
            parse_page(title, &content, &titles).unwrap()
        };
        let has = |requirements: &[Requirement], kind: &str, name: &str, level| {
            requirements.iter().any(|requirement| {
                requirement.kind == kind && requirement.name == name && requirement.level == level
            })
        };

        for (title, expected, forbidden) in [
            (
                "Dragon cane",
                vec![("skill", "Attack", Some(60))],
                vec![("skill", "Prayer"), ("quest", "Heroes' Quest")],
            ),
            (
                "Slayer helmet",
                vec![("skill", "Defence", Some(10))],
                vec![("skill", "Slayer")],
            ),
            (
                "Adamant halberd",
                vec![
                    ("skill", "Attack", Some(30)),
                    ("skill", "Strength", Some(15)),
                ],
                vec![("quest", "Regicide")],
            ),
            (
                "Dragon scimitar",
                vec![
                    ("skill", "Attack", Some(60)),
                    ("quest", "Monkey Madness I", None),
                ],
                vec![("quest", "Monkey Madness II")],
            ),
        ] {
            let (_, requirements, _) = parse(title);
            for (kind, name, level) in expected {
                assert!(
                    has(&requirements, kind, name, level),
                    "{title}: {requirements:?}"
                );
            }
            for (kind, name) in forbidden {
                assert!(
                    !requirements
                        .iter()
                        .any(|requirement| requirement.kind == kind && requirement.name == name),
                    "{title}: {requirements:?}"
                );
            }
        }

        for (title, expected) in [
            (
                "Ancient wyvern shield",
                vec![("Magic", 70), ("Defence", 75)],
            ),
            ("Amethyst broad bolts", vec![("Slayer", 65), ("Ranged", 61)]),
            (
                "Ancient d'hide boots",
                vec![("Defence", 40), ("Ranged", 70)],
            ),
        ] {
            let (_, requirements, _) = parse(title);
            for (name, level) in expected {
                assert!(
                    has(&requirements, "skill", name, Some(level)),
                    "{title}: {requirements:?}"
                );
            }
        }

        let (_, requirements, _) = parse("Void mage helm");
        for (name, level) in [
            ("Attack", 42),
            ("Strength", 42),
            ("Defence", 42),
            ("Hitpoints", 42),
            ("Ranged", 42),
            ("Magic", 42),
            ("Prayer", 22),
        ] {
            assert!(has(&requirements, "skill", name, Some(level)));
        }

        let (_, requirements, _) = parse("Bellator ring");
        assert!(has(&requirements, "boss_kill", "The Whisperer", Some(1)));
        for title in ["Attack cape", "Crafting cape", "Prayer cape"] {
            let skill = title.strip_suffix(" cape").unwrap();
            let (_, requirements, _) = parse(title);
            assert!(has(&requirements, "skill", skill, Some(99)), "{title}");
        }
        let (_, requirements, _) = parse("Max cape");
        assert!(has(&requirements, "skill_set", "All skills", Some(99)));

        let (_, requirements, status) = parse("Guthix mjolnir");
        assert!(requirements.is_empty());
        assert_eq!(status, "explicit_none");
    }
}
