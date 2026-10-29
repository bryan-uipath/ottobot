//! `$OTTO_LEDGER` (`<memory>/ledger.json`): `{"items": {"<id>": {"at": "<RFC3339>", ...}}}`, ids a skill already
//! handled. Skills read and write it; ottobot only prunes `items` past `ledger_ttl`. Other keys/fields are preserved.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub fn path(memory: &Path) -> PathBuf {
    memory.join("ledger.json")
}

/// Parsed file; missing → empty. Errors on bad JSON or non-object `items` so callers never overwrite it.
pub fn load(path: &Path) -> Result<Value> {
    let v: Value = match std::fs::read_to_string(path) {
        Ok(s) => serde_json::from_str(&s).with_context(|| format!("parse {}", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Value::Object(Map::new()),
        Err(e) => return Err(e.into()),
    };
    if !v.is_object() || v.get("items").is_some_and(|i| !i.is_object()) {
        anyhow::bail!("{}: expected {{\"items\": {{...}}}}", path.display());
    }
    Ok(v)
}

/// `items`, created if absent. None only for a value `load` would have rejected.
pub fn items(v: &mut Value) -> Option<&mut Map<String, Value>> {
    v.as_object_mut()?.entry("items").or_insert_with(|| Value::Object(Map::new())).as_object_mut()
}

/// Temp file + rename, so a concurrent reader never sees a half-written ledger.
pub fn save(path: &Path, v: &Value) -> Result<()> {
    let tmp = path.with_extension("json.otto-tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(v)?)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

/// Drop items whose `at` is older than `ttl`; items without a parseable `at` stay. Returns how many were removed.
pub fn prune(path: &Path, ttl: Duration, now: DateTime<Utc>) -> Result<usize> {
    if !path.exists() {
        return Ok(0);
    }
    let Some(cutoff) = chrono::Duration::from_std(ttl).ok().and_then(|d| now.checked_sub_signed(d)) else { return Ok(0) };
    let mut v = load(path)?;
    let items = items(&mut v).context("no items")?;
    let before = items.len();
    items.retain(|_, item| {
        let at = item.get("at").and_then(Value::as_str).and_then(|s| DateTime::parse_from_rfc3339(s).ok());
        at.is_none_or(|t| t >= cutoff)
    });
    let removed = before - items.len();
    if removed > 0 {
        save(path, &v)?;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("otto-ledger-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        path(&d)
    }

    #[test]
    fn prunes_old_keeps_fresh_undated_and_extra_keys() {
        let p = tmp("prune");
        let now: DateTime<Utc> = "2026-10-29T12:00:00Z".parse().unwrap();
        std::fs::write(
            &p,
            r#"{"items": {
                "old": {"at": "2026-09-01T00:00:00Z", "pr": "a#1"},
                "fresh": {"at": "2026-10-28T00:00:00+02:00", "pr": "a#2"},
                "no-at": {"pr": "a#3"},
                "bad-at": {"at": "yesterday"}
            }, "prs": {"a#1": {"conflict_head": "abc"}}}"#,
        )
        .unwrap();
        assert_eq!(prune(&p, Duration::from_secs(30 * 86400), now).unwrap(), 1);
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        let ids: Vec<&str> = v["items"].as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(ids, ["bad-at", "fresh", "no-at"]);
        assert_eq!(v["items"]["fresh"]["pr"], "a#2");
        assert_eq!(v["prs"]["a#1"]["conflict_head"], "abc");
    }

    #[test]
    fn unparseable_or_missing_untouched() {
        let p = tmp("bad");
        std::fs::write(&p, "{not json").unwrap();
        assert!(prune(&p, Duration::from_secs(1), Utc::now()).is_err());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "{not json");
        std::fs::write(&p, r#"{"items": []}"#).unwrap();
        assert!(prune(&p, Duration::from_secs(1), Utc::now()).is_err());
        std::fs::remove_file(&p).unwrap();
        assert_eq!(prune(&p, Duration::from_secs(1), Utc::now()).unwrap(), 0);
        assert!(!p.exists());
    }
}
