//! pi's record: one jsonl per session in `<agent dir>\sessions\<folder name>`.
//!
//! The file is a tree (`id`/`parentId`), but a live session only ever appends to its active branch,
//! so this reads it straight through and keeps the `message` entries.
//! shortcut: a session branched with /tree or /fork shows every branch, not just the active one;
//! upgrade if a member branches mid-session and the chat looks wrong.

use super::{media_paths, ChatEntry, Follower};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// pi's folder name for a working folder: the leading separator goes, then `/`, `\` and `:` become
/// `-`, and the whole is wrapped in `--`.
pub fn project_folder_name(folder: &str) -> String {
    let trimmed = folder.trim_start_matches(['\\', '/']);
    let body: String = trimmed
        .chars()
        .map(|c| if matches!(c, '/' | '\\' | ':') { '-' } else { c })
        .collect();
    format!("--{body}--")
}

/// Where pi keeps its sessions for this computer's user.
pub fn sessions_root() -> Option<PathBuf> {
    let dir = std::env::var_os("PI_CODING_AGENT_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(|home| PathBuf::from(home).join(".pi").join("agent")))?;
    Some(dir.join("sessions"))
}

/// The newest record of `folder`'s session written since `since` (a minute's grace for clocks).
pub fn newest_record(folder: &Path, since: SystemTime) -> Option<PathBuf> {
    newest_record_in(&sessions_root()?.join(project_folder_name(&folder.display().to_string())), since)
}

pub fn newest_record_in(records: &Path, since: SystemTime) -> Option<PathBuf> {
    let floor = since.checked_sub(Duration::from_secs(60)).unwrap_or(since);
    std::fs::read_dir(records)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("jsonl"))
        .filter_map(|p| p.metadata().and_then(|m| m.modified()).ok().map(|t| (t, p)))
        .filter(|(t, _)| *t >= floor)
        .max_by_key(|(t, _)| *t)
        .map(|(_, p)| p)
}

/// The text of a content value: a bare string, or the `text` of each text block in a list.
fn texts(content: Option<&Value>) -> Vec<String> {
    match content {
        Some(Value::String(text)) => vec![text.clone()],
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

/// What a tool call was aimed at, so Chat can say `read: D:\Castle\src\lib.rs` rather than just
/// `read`. The first of these arguments that is a string wins; otherwise the first string given.
const AIM: [&str; 7] = ["file_path", "path", "command", "url", "query", "pattern", "prompt"];

fn aim(arguments: Option<&Value>) -> Option<String> {
    let arguments = arguments?;
    for name in AIM {
        if let Some(value) = arguments.get(name).and_then(Value::as_str) {
            return Some(value.to_string());
        }
    }
    arguments
        .as_object()?
        .values()
        .find_map(Value::as_str)
        .map(str::to_string)
}

/// Turns one record line into the chat entries it holds (often none).
pub fn parse_line(line: &str) -> Vec<ChatEntry> {
    let Ok(record) = serde_json::from_str::<Value>(line) else {
        return Vec::new();
    };
    if record.get("type").and_then(Value::as_str) != Some("message") {
        return Vec::new();
    }
    let Some(message) = record.get("message") else { return Vec::new() };
    let role = message.get("role").and_then(Value::as_str).unwrap_or("");
    let id = record.get("id").and_then(Value::as_str).unwrap_or("").to_string();
    let at = record.get("timestamp").and_then(Value::as_str).map(str::to_string);
    let entry = |n: usize, role: &str, text: String| ChatEntry {
        id: format!("{id}-{n}"),
        role: role.to_string(),
        text,
        at: at.clone(),
        path: None,
        pictures: Vec::new(),
        state: None,
    };

    match role {
        // system messages carry the prompt and the tool loadout, never the conversation
        "user" => texts(message.get("content"))
            .into_iter()
            .enumerate()
            .filter(|(_, text)| {
                let text = text.trim();
                // a slash command or an injected tag is not something the member said
                !text.is_empty() && !text.starts_with('<')
            })
            .map(|(n, text)| entry(n, "member", text))
            .collect(),
        "assistant" => {
            let mut entries = Vec::new();
            let Some(Value::Array(blocks)) = message.get("content") else { return entries };
            for block in blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(text) = block.get("text").and_then(Value::as_str) {
                            if !text.trim().is_empty() {
                                entries.push(entry(entries.len(), "companion", text.to_string()));
                            }
                        }
                    }
                    // thinking stays private, as it does for Claude Code
                    Some("toolCall") => {
                        let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
                        let text = match aim(block.get("arguments")) {
                            Some(aim) => format!("{name}: {aim}"),
                            None => name.to_string(),
                        };
                        entries.push(entry(entries.len(), "work", text));
                    }
                    _ => {}
                }
            }
            entries
        }
        // the tool's own output is already shown by the call above it
        _ => Vec::new(),
    }
}

/// The paths in a record line that may name a file the companion made: what a tool call asked for
/// and what a tool answered with.
pub fn media_candidates(line: &str) -> Vec<String> {
    let Ok(record) = serde_json::from_str::<Value>(line) else {
        return Vec::new();
    };
    let Some(message) = record.get("message") else { return Vec::new() };
    let mut found = Vec::new();
    match message.get("content") {
        Some(Value::Array(blocks)) => {
            for block in blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("toolCall") => {
                        if let Some(arguments) = block.get("arguments").and_then(Value::as_object) {
                            for value in arguments.values().filter_map(Value::as_str) {
                                found.extend(media_paths(value));
                            }
                        }
                    }
                    Some("text") => {
                        if let Some(text) = block.get("text").and_then(Value::as_str) {
                            found.extend(media_paths(text));
                        }
                    }
                    _ => {}
                }
            }
        }
        Some(Value::String(text)) => found.extend(media_paths(text)),
        _ => {}
    }
    let mut seen = std::collections::HashSet::new();
    found.retain(|p| seen.insert(p.to_lowercase()));
    found
}

/// A follower for one pi record.
pub fn follower(path: PathBuf, since: SystemTime) -> Follower {
    Follower::new(path, since, parse_line, media_candidates)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_memory_folder_maps_to_pis_folder_name() {
        // the shape pi really writes for this folder on Windows
        assert_eq!(
            project_folder_name("C:\\Users\\ZEN\\Documents\\Project-AI-MemoryCore"),
            "--C--Users-ZEN-Documents-Project-AI-MemoryCore--"
        );
    }

    #[test]
    fn his_words_and_hers_become_entries_and_the_rest_stays_out() {
        let member = r#"{"type":"message","id":"u1","timestamp":"t","message":{"role":"user","content":[{"type":"text","text":"hi Lyra"}]}}"#;
        let entries = parse_line(member);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].role, "member");
        assert_eq!(entries[0].text, "hi Lyra");

        let companion = r#"{"type":"message","id":"a1","timestamp":"t","message":{"role":"assistant","content":[
            {"type":"thinking","thinking":"private"},
            {"type":"text","text":"Good evening, Rin."},
            {"type":"toolCall","id":"c1","name":"read","arguments":{"path":"D:\\Castle\\src\\lib.rs"}}]}}"#
            .replace('\n', "");
        let entries = parse_line(&companion);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].role, "companion");
        assert_eq!(entries[0].text, "Good evening, Rin.");
        assert_eq!(entries[1].role, "work");
        assert_eq!(entries[1].text, "read: D:\\Castle\\src\\lib.rs");

        // the tool's own output, the system prompt, a model switch and a broken line stay out
        let result = r#"{"type":"message","id":"r1","message":{"role":"toolResult","toolName":"read","content":[{"type":"text","text":"x"}]}}"#;
        assert!(parse_line(result).is_empty());
        let system = r#"{"type":"message","id":"s1","message":{"role":"system","content":"","sections":{}}}"#;
        assert!(parse_line(system).is_empty());
        assert!(parse_line(r#"{"type":"model_change","id":"m1","provider":"mistral","modelId":"x"}"#).is_empty());
        assert!(parse_line("{ broken").is_empty());
    }

    #[test]
    fn a_picture_the_companion_made_is_found_in_a_tool_call() {
        let line = r#"{"type":"message","id":"a1","message":{"role":"assistant","content":[{"type":"toolCall","name":"write","arguments":{"path":"D:\\Memory\\studio\\lyra.png"}}]}}"#;
        assert_eq!(media_candidates(line), vec!["D:\\Memory\\studio\\lyra.png".to_string()]);
    }
}
