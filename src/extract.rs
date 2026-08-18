use std::collections::HashMap;

use anyhow::{Context, Result};
use parsoid::{Wikicode, node::Wikinode, prelude::*};
use percent_encoding::percent_decode_str;

use crate::model::{ExtractedPage, ExtractedSection};

pub const CHUNK_CHARACTERS: usize = 1_500;

pub fn extract_page(html: &str) -> Result<ExtractedPage> {
    let code = Wikicode::new(html);
    let categories = extract_categories(&code);
    let mut sections = Vec::new();
    let mut heading_stack: Vec<String> = Vec::new();

    for (position, section) in code
        .select("section[data-mw-section-id]")
        .into_iter()
        .enumerate()
    {
        let index = position as i64;
        let heading_node = section.children().find(|node| {
            matches!(
                tag_name(node).as_deref(),
                Some("h1" | "h2" | "h3" | "h4" | "h5" | "h6")
            )
        });
        let level: usize = heading_node
            .as_ref()
            .and_then(tag_name)
            .and_then(|tag| tag[1..].parse().ok())
            .unwrap_or(1);
        let heading = heading_node
            .as_ref()
            .map(|node| compact_space(&node.text_contents()))
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "Lead".to_string());

        if index != 0 {
            heading_stack.truncate(level.saturating_sub(1));
            while heading_stack.len() < level.saturating_sub(1) {
                heading_stack.push(String::new());
            }
            heading_stack.push(heading.clone());
        }
        let heading_path = if index == 0 {
            heading.clone()
        } else {
            heading_stack
                .iter()
                .filter(|part| !part.is_empty())
                .cloned()
                .collect::<Vec<_>>()
                .join(" > ")
        };

        let mut blocks = Vec::new();
        for child in section.children() {
            render_node(&child, &mut blocks);
        }
        if index == 0 {
            blocks.extend(render_key_templates(&code)?);
        }
        sections.push(ExtractedSection {
            index,
            level,
            heading: heading_path,
            blocks: blocks
                .into_iter()
                .filter(|block| !block.is_empty())
                .collect(),
        });
    }

    if sections.is_empty() {
        let mut blocks = Vec::new();
        for node in code.select("body") {
            render_node(&node, &mut blocks);
        }
        blocks.extend(render_key_templates(&code)?);
        sections.push(ExtractedSection {
            index: 0,
            level: 1,
            heading: "Lead".to_string(),
            blocks,
        });
    }

    Ok(ExtractedPage {
        revision_id: code.revision_id(),
        categories,
        sections,
    })
}

pub fn exclusion_reason(categories: &[String]) -> Option<String> {
    categories.iter().find_map(|category| {
        let normalized = category.to_ascii_lowercase();
        (normalized.starts_with("historical")
            || normalized.starts_with("removed")
            || normalized.starts_with("obsolete")
            || normalized == "discontinued content")
            .then(|| format!("Category:{category}"))
    })
}

pub fn chunks_for_section(title: &str, heading: &str, blocks: &[String]) -> Vec<String> {
    let prefix = format!("{title}\n{heading}\n");
    let available = CHUNK_CHARACTERS
        .saturating_sub(prefix.chars().count())
        .max(300);
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut tab = None;

    for block in blocks {
        if block.starts_with("Tab: ") {
            if !current.is_empty() {
                chunks.push(format!("{prefix}{}", current.trim()));
            }
            current = block.clone();
            tab = Some(block.as_str());
            continue;
        }
        for piece in split_text(block, available) {
            let separator = usize::from(!current.is_empty()) * 2;
            if current.chars().count() + separator + piece.chars().count() > available
                && !current.is_empty()
            {
                chunks.push(format!("{prefix}{}", current.trim()));
                current.clear();
                if let Some(tab) = tab {
                    current.push_str(tab);
                }
            }
            if !current.is_empty() {
                current.push_str("\n\n");
            }
            current.push_str(&piece);
        }
    }
    if !current.is_empty() {
        chunks.push(format!("{prefix}{}", current.trim()));
    }
    chunks
}

fn extract_categories(code: &Wikicode) -> Vec<String> {
    let mut categories = code
        .select("link[rel=\"mw:PageProp/Category\"]")
        .into_iter()
        .filter_map(|node| attrs(&node).get("href").cloned())
        .filter_map(|href| href.split("Category:").nth(1).map(str::to_string))
        .map(|value| value.split('#').next().unwrap_or(&value).to_string())
        .map(|value| {
            percent_decode_str(&value)
                .decode_utf8_lossy()
                .replace('_', " ")
        })
        .collect::<Vec<_>>();
    categories.sort();
    categories.dedup();
    categories
}

fn render_node(node: &Wikinode, blocks: &mut Vec<String>) {
    let Some(tag) = tag_name(node) else {
        return;
    };
    let node_attrs = attrs(node);
    let classes = node_attrs.get("class").map(String::as_str).unwrap_or("");
    if classes.contains("infobox-resources")
        || classes.contains("navbox")
        || classes.contains("mw-editsection")
        || matches!(
            tag.as_str(),
            "script" | "style" | "link" | "meta" | "noscript"
        )
    {
        return;
    }

    match tag.as_str() {
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {}
        "p" | "blockquote" | "pre" | "figcaption" => push_text(blocks, text_without_tables(node)),
        "ul" | "ol" => {
            let ordered = tag == "ol";
            let mut index = 1;
            for child in node
                .children()
                .filter(|child| tag_name(child).as_deref() == Some("li"))
            {
                let text = compact_space(&text_without_tables(&child));
                if !text.is_empty() {
                    blocks.push(if ordered {
                        let value = format!("{index}. {text}");
                        index += 1;
                        value
                    } else {
                        format!("- {text}")
                    });
                }
            }
        }
        "dl" => {
            for child in node.children() {
                if matches!(tag_name(&child).as_deref(), Some("dt" | "dd")) {
                    push_text(blocks, text_without_tables(&child));
                }
            }
        }
        "table" => push_text(blocks, render_table(node)),
        _ => {
            if classes.contains("tabbertab")
                && let Some(title) = node_attrs.get("data-title")
            {
                blocks.push(format!("Tab: {title}"));
            }
            for child in node.children() {
                render_node(&child, blocks);
            }
        }
    }
}

fn render_table(table: &Wikinode) -> String {
    let mut lines = Vec::new();
    let mut headers = Vec::new();
    for row in direct_rows(table) {
        let cells = row
            .children()
            .filter(|cell| matches!(tag_name(cell).as_deref(), Some("th" | "td")))
            .collect::<Vec<_>>();
        if cells.is_empty() {
            continue;
        }
        let values = cells
            .iter()
            .map(|cell| {
                let cell_attrs = attrs(cell);
                let mut value = compact_space(&text_without_tables(cell));
                let rowspan = cell_attrs
                    .get("rowspan")
                    .filter(|value| value.as_str() != "1");
                let colspan = cell_attrs
                    .get("colspan")
                    .filter(|value| value.as_str() != "1");
                if rowspan.is_some() || colspan.is_some() {
                    let mut spans = Vec::new();
                    if let Some(value) = rowspan {
                        spans.push(format!("rowspan={value}"));
                    }
                    if let Some(value) = colspan {
                        spans.push(format!("colspan={value}"));
                    }
                    value.push_str(&format!(" [{}]", spans.join(",")));
                }
                value
            })
            .collect::<Vec<_>>();
        let is_header = cells
            .iter()
            .all(|cell| tag_name(cell).as_deref() == Some("th"));
        if is_header && headers.is_empty() {
            headers = values.clone();
            lines.push(format!("Columns: {}", values.join(" | ")));
        } else if values.len() == 2 && tag_name(&cells[0]).as_deref() == Some("th") {
            lines.push(format!("{}: {}", values[0], values[1]));
        } else if !headers.is_empty() && headers.len() == values.len() {
            lines.push(
                headers
                    .iter()
                    .zip(values.iter())
                    .map(|(header, value)| format!("{header}: {value}"))
                    .collect::<Vec<_>>()
                    .join(" | "),
            );
        } else {
            lines.push(values.join(" | "));
        }

        for cell in cells {
            for nested in direct_child_tables(&cell) {
                let rendered = render_table(&nested);
                if !rendered.is_empty() {
                    lines.push(format!("Nested table: {rendered}"));
                }
            }
        }
    }
    lines.join("\n")
}

fn direct_rows(table: &Wikinode) -> Vec<Wikinode> {
    let mut rows = Vec::new();
    for child in table.children() {
        match tag_name(&child).as_deref() {
            Some("tr") => rows.push(child),
            Some("thead" | "tbody" | "tfoot") => {
                rows.extend(
                    child
                        .children()
                        .filter(|node| tag_name(node).as_deref() == Some("tr")),
                );
            }
            _ => {}
        }
    }
    rows
}

fn direct_child_tables(node: &Wikinode) -> Vec<Wikinode> {
    let mut tables = Vec::new();
    for child in node.children() {
        if tag_name(&child).as_deref() == Some("table") {
            tables.push(child);
        } else {
            tables.extend(direct_child_tables(&child));
        }
    }
    tables
}

fn render_key_templates(code: &Wikicode) -> Result<Vec<String>> {
    let mut blocks = Vec::new();
    for template in code.filter_templates().context("read Parsoid templates")? {
        let name = template.name();
        if !(name.contains("Infobox") || name.contains("DiarySkillStats")) {
            continue;
        }
        let mut params = template.params().into_iter().collect::<Vec<_>>();
        params.sort_by(|left, right| left.0.cmp(&right.0));
        let rendered = params
            .into_iter()
            .map(|(key, value)| format!("{}={}", compact_space(&key), compact_space(&value)))
            .collect::<Vec<_>>()
            .join(" | ");
        blocks.push(format!("{name}: {rendered}"));
    }
    Ok(blocks)
}

fn text_without_tables(node: &Wikinode) -> String {
    if tag_name(node).as_deref() == Some("table") {
        return String::new();
    }
    if tag_name(node).as_deref() == Some("img") {
        let attributes = attrs(node);
        return attributes
            .get("alt")
            .or_else(|| attributes.get("title"))
            .cloned()
            .unwrap_or_default();
    }
    if node.as_element().is_none() {
        return node.text_contents();
    }
    node.children()
        .map(|child| text_without_tables(&child))
        .collect::<Vec<_>>()
        .join(" ")
}

fn split_text(value: &str, limit: usize) -> Vec<String> {
    let mut remaining = value.trim();
    let mut pieces = Vec::new();
    while remaining.chars().count() > limit {
        let byte_limit = remaining
            .char_indices()
            .nth(limit)
            .map(|(index, _)| index)
            .unwrap_or(remaining.len());
        let candidate = &remaining[..byte_limit];
        let split = candidate
            .rfind(|character: char| character.is_whitespace())
            .filter(|index| *index >= limit / 2)
            .unwrap_or(byte_limit);
        pieces.push(remaining[..split].trim().to_string());
        remaining = remaining[split..].trim();
    }
    if !remaining.is_empty() {
        pieces.push(remaining.to_string());
    }
    pieces
}

fn push_text(blocks: &mut Vec<String>, value: String) {
    let value = compact_space(&value);
    if !value.is_empty() {
        blocks.push(value);
    }
}

fn compact_space(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn tag_name(node: &Wikinode) -> Option<String> {
    node.as_element()
        .map(|element| element.name.local.to_string())
}

fn attrs(node: &Wikinode) -> HashMap<String, String> {
    node.as_element()
        .map(|element| {
            element
                .attributes
                .borrow()
                .map
                .iter()
                .map(|(key, value)| (key.local.to_string(), value.value.clone()))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excludes_only_explicit_markers() {
        for category in [
            "Historical content",
            "Removed content",
            "Obsolete money making guides",
            "Discontinued content",
        ] {
            assert_eq!(
                exclusion_reason(&[category.to_string()]),
                Some(format!("Category:{category}"))
            );
        }
        assert!(exclusion_reason(&["Leagues".to_string()]).is_none());
    }

    #[test]
    fn chunking_keeps_title_and_heading() {
        let chunks = chunks_for_section("Bow", "Combat stats", &["x ".repeat(1_000)]);
        assert!(chunks.len() > 1);
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.starts_with("Bow\nCombat stats\n"))
        );
    }

    #[test]
    fn tab_labels_are_repeated_on_continuation_chunks() {
        let blocks = vec![
            "Tab: Melee".to_string(),
            "melee ".repeat(500),
            "Tab: Range".to_string(),
            "toxic blowpipe ".repeat(500),
        ];
        let chunks = chunks_for_section("Gargoyles", "Common setups", &blocks);
        let ranged = chunks
            .iter()
            .filter(|chunk| chunk.contains("toxic blowpipe"))
            .collect::<Vec<_>>();
        assert!(!ranged.is_empty());
        assert!(
            ranged
                .iter()
                .all(|chunk| chunk.starts_with("Gargoyles\nCommon setups\nTab: Range"))
        );
        assert!(ranged.iter().all(|chunk| !chunk.contains("Tab: Melee")));
    }

    #[test]
    fn table_rendering_keeps_spans_and_nested_levels() {
        let code = Wikicode::new(
            "<table><tbody><tr><th colspan=\"2\">Requirements</th></tr><tr><td rowspan=\"2\">Easy</td><td><table><tbody><tr><td><table><tbody><tr><td>Quest</td></tr></tbody></table></td></tr></tbody></table></td></tr></tbody></table>",
        );
        let rendered = render_table(&code.select("table")[0]);
        assert!(rendered.contains("Requirements [colspan=2]"));
        assert!(rendered.contains("Easy [rowspan=2]"));
        assert!(rendered.matches("Nested table:").count() >= 2);
        assert!(rendered.contains("Quest"));
    }

    #[test]
    fn repeated_parsoid_section_ids_get_unique_handles() {
        let page = extract_page(
            "<html><body><section data-mw-section-id=\"-1\"><h2>First</h2><p>A</p></section><section data-mw-section-id=\"-1\"><h2>Second</h2><p>B</p></section></body></html>",
        )
        .unwrap();
        assert_eq!(
            page.sections
                .iter()
                .map(|section| section.index)
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
    }
}
