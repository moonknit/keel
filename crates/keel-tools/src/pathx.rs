use keel_core::types::ToolCx;
use std::path::{Path, PathBuf};

pub fn resolve(cx: &ToolCx, rel: &str) -> Result<PathBuf, String> {
    let raw = Path::new(rel);
    let joined = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        cx.cwd.join(raw)
    };
    let canon = joined.canonicalize().unwrap_or(joined);
    let cwd = cx.cwd.canonicalize().unwrap_or(cx.cwd.clone());
    if canon.starts_with(&cwd) {
        return Ok(canon);
    }
    for root in &cx.read_roots {
        let root = root.canonicalize().unwrap_or(root.clone());
        if canon.starts_with(&root) {
            return Ok(canon);
        }
    }
    Err("path outside allowed roots".into())
}

pub fn resolve_write(cx: &ToolCx, rel: &str) -> Result<PathBuf, String> {
    let raw = Path::new(rel);
    let cwd = cx.cwd.canonicalize().map_err(|e| e.to_string())?;
    let joined = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        cwd.join(raw)
    };
    let (existing, rest) = existing_dir_prefix(&joined);
    let parent_canon = existing.canonicalize().map_err(|e| e.to_string())?;
    let target = parent_canon.join(rest);
    if !target.starts_with(&cwd) {
        return Err("write outside cwd".into());
    }
    Ok(target)
}

fn existing_dir_prefix(path: &Path) -> (PathBuf, PathBuf) {
    let mut cur = path.to_path_buf();
    let mut rest = PathBuf::new();
    loop {
        if cur.is_dir() {
            return (cur, rest);
        }
        match cur.file_name() {
            Some(name) => {
                rest = if rest.as_os_str().is_empty() {
                    PathBuf::from(name)
                } else {
                    Path::new(name).join(rest)
                };
                match cur.parent() {
                    Some(parent) => cur = parent.to_path_buf(),
                    None => return (cur, rest),
                }
            }
            None => return (cur, rest),
        }
    }
}
