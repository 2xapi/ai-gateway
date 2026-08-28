use serde_json::{json, Value};
use std::path::Path;

fn count_jsonl(root: &Path) -> usize {
    let mut count = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path
                .extension()
                .is_some_and(|extension| extension == "jsonl")
            {
                count += 1;
            }
        }
    }
    count
}

pub fn inspect(codex_home: &Path) -> Value {
    let active = count_jsonl(&codex_home.join("sessions"));
    let archived = count_jsonl(&codex_home.join("archived_sessions"));
    json!({
        "ok": true,
        "state": {
            "total": active + archived,
            "active": active,
            "archived": archived,
            "rolloutTotal": active + archived,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_nested_and_archived_rollouts() {
        let root = std::env::temp_dir().join(format!("2xapi-history-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("sessions/2026/08")).unwrap();
        std::fs::create_dir_all(root.join("archived_sessions")).unwrap();
        std::fs::write(root.join("sessions/2026/08/a.jsonl"), "{}").unwrap();
        std::fs::write(root.join("archived_sessions/b.jsonl"), "{}").unwrap();
        let value = inspect(&root);
        assert_eq!(value["state"]["active"], 1);
        assert_eq!(value["state"]["archived"], 1);
        assert_eq!(value["state"]["total"], 2);
        let _ = std::fs::remove_dir_all(root);
    }
}
