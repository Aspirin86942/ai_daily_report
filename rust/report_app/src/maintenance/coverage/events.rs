use super::super::files::{read_bytes, write_immutable};
use crate::{
    snapshots::{canonical_json, fingerprint, reject_redirects},
    ReportError,
};
use serde_json::{json, Value};
use std::{
    collections::BTreeSet,
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

pub(super) struct Events {
    path: PathBuf,
    job: String,
    plan: String,
    items: Vec<Value>,
    pub state: Value,
}
impl Events {
    pub fn open(dir: &Path, job: &str, plan: &str) -> Result<Self, ReportError> {
        let path = dir.join("events.jsonl");
        reject_redirects(&path, "COVERAGE_JOB_PATH_REDIRECTED")?;
        let mut result = Self {
            path,
            job: job.into(),
            plan: plan.into(),
            items: vec![],
            state: json!({"dates":{}}),
        };
        if !result.path.exists() {
            return Ok(result);
        }
        let data = read_bytes(&result.path, 32 * 1024 * 1024)?;
        let mut offset = 0;
        let mut ids = BTreeSet::new();
        for line in data.split_inclusive(|b| *b == b'\n') {
            if !line.ends_with(b"\n") {
                let hash = crate::snapshots::digest(line);
                write_immutable(&dir.join(format!("events.tail.{hash}.bin")), line)?;
                let file = File::options().write(true).open(&result.path)?;
                file.set_len(offset)?;
                file.sync_all()?;
                result.append(
                    "tail_recovered",
                    None,
                    json!({}),
                    json!({"tail_sha256":hash}),
                )?;
                break;
            }
            if line.len() > 64 * 1024 {
                return Err(corrupt());
            }
            let event: Value = serde_json::from_slice(line).map_err(|_| corrupt())?;
            let id = fingerprint(&event["body"])?;
            if event["seq"] != json!(result.items.len() + 1)
                || event["event_id"] != id
                || !ids.insert(id)
                || event["body"]["job_id"] != job
                || event["body"]["plan_sha256"] != plan
            {
                return Err(corrupt());
            }
            result.apply(&event)?;
            result.items.push(event);
            offset += line.len() as u64;
        }
        Ok(result)
    }
    fn apply(&mut self, event: &Value) -> Result<(), ReportError> {
        if let Some(date) = event["body"]["date"].as_str() {
            crate::types::parse_date(date)?;
            if self.state["dates"].get(date).is_none() {
                self.state["dates"][date] = json!({});
            }
            let updates = event["body"]["updates"].as_object().ok_or_else(corrupt)?;
            for (key, value) in updates {
                self.state["dates"][date][key] = value.clone();
            }
        }
        Ok(())
    }
    pub fn append(
        &mut self,
        action: &str,
        date: Option<&str>,
        updates: Value,
        details: Value,
    ) -> Result<(), ReportError> {
        let body = json!({"job_id":self.job,"plan_sha256":self.plan,"date":date,"run_id":updates["run_id"],"action":action,"phase":updates.get("status").cloned().unwrap_or(json!(action)),"updates":updates,"details":details,"before_quality":null,"after_quality":null});
        let id = fingerprint(&body)?;
        if self.items.iter().any(|event| event["event_id"] == id) {
            return Ok(());
        }
        let event = json!({"seq":self.items.len()+1,"event_id":id,"body":body,"at":chrono::Utc::now().to_rfc3339()});
        let mut bytes = canonical_json(&event)?;
        bytes.push(b'\n');
        if bytes.len() > 64 * 1024
            || self.path.metadata().map(|m| m.len()).unwrap_or(0) + bytes.len() as u64
                > 32 * 1024 * 1024
        {
            return Err(corrupt());
        }
        let mut stream = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        stream.write_all(&bytes)?;
        stream.sync_all()?;
        self.apply(&event)?;
        self.items.push(event);
        Ok(())
    }
}
fn corrupt() -> ReportError {
    ReportError::new("COVERAGE_EVENTS_CORRUPT", "覆盖修复事件记录无效")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn torn_tail_is_saved_and_complete_event_tampering_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut events = Events::open(dir.path(), "job", "plan").unwrap();
        events
            .append(
                "prepared",
                Some("2026-10-11"),
                json!({"run_id":"one","status":"prepared"}),
                json!({}),
            )
            .unwrap();
        OpenOptions::new()
            .append(true)
            .open(&events.path)
            .unwrap()
            .write_all(b"{partial")
            .unwrap();
        let restored = Events::open(dir.path(), "job", "plan").unwrap();
        assert_eq!(restored.state["dates"]["2026-10-11"]["run_id"], "one");
        assert!(dir
            .path()
            .join(format!(
                "events.tail.{}.bin",
                crate::snapshots::digest(b"{partial")
            ))
            .exists());
        assert_eq!(
            Events::open(dir.path(), "job", "plan").unwrap().items.len(),
            2
        );
        std::fs::write(&events.path, b"{\"seq\":1}\n").unwrap();
        assert!(Events::open(dir.path(), "job", "plan").is_err());
    }
}
