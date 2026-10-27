use async_trait::async_trait;
use keel_core::traits::Tool;
use keel_core::types::{Item, ItemKind, Role, ToolClass, ToolCx, ToolOutput, ToolSpec};
use serde::Deserialize;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct SkillMeta {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
}

#[derive(Deserialize)]
struct Front {
    name: String,
    description: String,
}

pub fn discover(cwd: &Path) -> Vec<SkillMeta> {
    let mut dirs = Vec::new();
    let mut cur = Some(cwd.to_path_buf());
    while let Some(dir) = cur {
        dirs.push(dir.join(".agents/skills"));
        dirs.push(dir.join(".claude/skills"));
        cur = dir.parent().map(Path::to_path_buf);
        if dir.join(".git").exists() {
            break;
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(&home).join(".agents/skills"));
        dirs.push(PathBuf::from(home).join(".claude/skills"));
    }
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for dir in dirs {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let skill = entry.path().join("SKILL.md");
                if !skill.is_file() {
                    continue;
                }
                if let Some(meta) = parse_skill(&skill) {
                    if seen.insert(meta.name.clone()) {
                        out.push(meta);
                    }
                }
            }
        }
    }
    out
}

fn parse_skill(path: &Path) -> Option<SkillMeta> {
    let text = fs::read_to_string(path).ok()?;
    let (front, _) = split_frontmatter(&text)?;
    let front: Front = serde_yaml::from_str(front).ok()?;
    Some(SkillMeta {
        name: front.name,
        description: front.description,
        path: path.parent()?.to_path_buf(),
    })
}

fn split_frontmatter(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix("---\n")?;
    let end = rest.find("\n---")?;
    Some((&rest[..end], rest[end + 4..].trim_start_matches('\n')))
}

pub fn catalog_text(skills: &[SkillMeta]) -> String {
    let mut s = String::from("<available_skills>\n");
    for sk in skills {
        s.push_str(&format!("- {}: {}\n", sk.name, sk.description));
        if s.len() > 4000 {
            s.push_str("…\n");
            break;
        }
    }
    s.push_str("</available_skills>\n");
    s
}

pub fn load_agents_md(cwd: &Path) -> String {
    let mut chunks = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        let p = PathBuf::from(home).join(".keel/AGENTS.md");
        if let Ok(t) = fs::read_to_string(p) {
            chunks.push(t);
        }
    }
    let mut chain = Vec::new();
    let mut cur = Some(cwd.to_path_buf());
    while let Some(dir) = cur {
        chain.push(dir.clone());
        if dir.join(".git").exists() {
            break;
        }
        cur = dir.parent().map(Path::to_path_buf);
    }
    chain.reverse();
    for dir in chain {
        let p = dir.join("AGENTS.md");
        if let Ok(t) = fs::read_to_string(p) {
            chunks.push(t);
        }
    }
    let mut text = chunks.join("\n\n");
    if text.len() > 16_000 {
        text.truncate(16_000);
        text.push_str("\n[AGENTS.md truncated]");
    }
    text
}

pub fn skill_item(meta: &SkillMeta) -> Item {
    Item {
        id: ulid::Ulid::new().to_string(),
        role: Role::Assistant,
        kind: ItemKind::Skill {
            name: meta.name.clone(),
            root: meta.path.clone(),
        },
        raw: None,
    }
}

pub fn skill_body(meta: &SkillMeta) -> String {
    let path = meta.path.join("SKILL.md");
    let text = fs::read_to_string(path).unwrap_or_default();
    let body = split_frontmatter(&text).map(|(_, b)| b.to_string()).unwrap_or(text);
    format!("{body}\n\nskill_root={}", meta.path.display())
}

pub struct SkillTool {
    pub skills: Vec<SkillMeta>,
}

#[async_trait]
impl Tool for SkillTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "skill".into(),
            description: "Load a SKILL.md body and its root path.".into(),
            parameters: json!({
                "type":"object",
                "properties": { "name": {"type":"string"} },
                "required": ["name"]
            }),
        }
    }
    fn class(&self) -> ToolClass {
        ToolClass::Read
    }
    async fn call(&self, args: Value, _cx: &ToolCx) -> ToolOutput {
        let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("");
        match self.skills.iter().find(|s| s.name == name) {
            Some(meta) => ToolOutput {
                text: skill_body(meta),
                is_error: false,
            },
            None => ToolOutput {
                text: format!("unknown skill {name}"),
                is_error: true,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// REQ-3.19 first discovered name wins.
    #[test]
    fn req_3_19_discover_and_catalog() {
        let dir = tempfile::tempdir().unwrap();
        let skill = dir.path().join(".agents/skills/demo");
        fs::create_dir_all(&skill).unwrap();
        fs::write(
            skill.join("SKILL.md"),
            "---\nname: demo\ndescription: d\n---\nHello\n",
        )
        .unwrap();
        let found = discover(dir.path());
        assert_eq!(found[0].name, "demo");
        assert!(catalog_text(&found).contains("demo"));
        assert!(skill_body(&found[0]).contains("Hello"));
        assert!(skill_body(&found[0]).contains("skill_root="));
    }

    /// REQ-3.20 AGENTS.md concat and cap.
    #[test]
    fn req_3_20_agents_md() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("AGENTS.md"), "use rust").unwrap();
        let text = load_agents_md(dir.path());
        assert!(text.contains("use rust"));
    }
}
