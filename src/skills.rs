//! SKILL.md discovery. A skill is a folder holding SKILL.md with YAML-ish frontmatter
//! (`name:`, `description:`) and any supporting files.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub dir: PathBuf,
    pub body: String,
}

pub fn discover(dirs: &[PathBuf]) -> Vec<Skill> {
    let mut out: Vec<Skill> = Vec::new();
    for d in dirs {
        let Ok(entries) = std::fs::read_dir(d) else { continue };
        let mut paths: Vec<_> = entries.flatten().map(|e| e.path()).collect();
        paths.sort();
        for p in paths {
            let file = p.join("SKILL.md");
            let Ok(text) = std::fs::read_to_string(&file) else { continue };
            let Some(skill) = parse(&text, &p) else { continue };
            // Earlier directories win, so the project's copy overrides a global one.
            if !out.iter().any(|s| s.name == skill.name) {
                out.push(skill);
            }
        }
    }
    out
}

pub fn parse(text: &str, dir: &Path) -> Option<Skill> {
    let text = text.trim_start_matches('\u{feff}');
    let rest = text.strip_prefix("---")?;
    let end = rest.find("\n---")?;
    let (front, body) = (&rest[..end], &rest[end + 4..]);
    let mut name = None;
    let mut description = String::new();
    for line in front.lines() {
        if let Some(v) = line.strip_prefix("name:") {
            name = Some(unquote(v));
        } else if let Some(v) = line.strip_prefix("description:") {
            description = unquote(v);
        }
    }
    let name = name.filter(|n| !n.is_empty()).or_else(|| dir.file_name()?.to_str().map(String::from))?;
    Some(Skill { name, description, dir: dir.to_path_buf(), body: body.trim().to_string() })
}

fn unquote(v: &str) -> String {
    v.trim().trim_matches('"').trim_matches('\'').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_frontmatter() {
        let s = parse("---\nname: replica-entrepreneur\ndescription: \"Rank reviews\"\n---\n# Body\nstep", Path::new("x")).unwrap();
        assert_eq!(s.name, "replica-entrepreneur");
        assert_eq!(s.description, "Rank reviews");
        assert!(s.body.starts_with("# Body"));
    }

    #[test]
    fn first_dir_wins() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        for (root, desc) in [(&a, "project"), (&b, "global")] {
            let d = root.path().join("s");
            std::fs::create_dir(&d).unwrap();
            std::fs::write(d.join("SKILL.md"), format!("---\nname: s\ndescription: {desc}\n---\nx")).unwrap();
        }
        let found = discover(&[a.path().into(), b.path().into()]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].description, "project");
    }
}
