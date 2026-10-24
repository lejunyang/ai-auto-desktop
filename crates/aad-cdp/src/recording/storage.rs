use super::{Options, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

pub struct Storage {
    pub root: PathBuf,
    pub bytes: u64,
    limit: u64,
}
impl Storage {
    pub fn new(options: &Options) -> Result<Self> {
        // Never truncate an earlier recording or user directory.
        std::fs::create_dir(&options.out)?;
        std::fs::create_dir(options.out.join("images"))?;
        std::fs::create_dir(options.out.join("evidence"))?;
        Ok(Self {
            root: options.out.clone(),
            bytes: 0,
            limit: options.max_bytes,
        })
    }
    fn reserve(&mut self, size: usize) -> Result<()> {
        if self.bytes.saturating_add(size as u64) > self.limit {
            return Err("recording storage budget reached".into());
        }
        self.bytes += size as u64;
        Ok(())
    }
    pub fn append(&mut self, name: &str, value: &Value) -> Result<()> {
        let mut bytes = serde_json::to_vec(value)?;
        bytes.push(b'\n');
        self.reserve(bytes.len())?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.root.join(name))?;
        file.write_all(&bytes)?;
        file.flush()?;
        Ok(())
    }
    pub fn image(&mut self, bytes: &[u8]) -> Result<String> {
        let name = format!("images/{:x}.png", Sha256::digest(bytes));
        if !self.root.join(&name).exists() {
            self.reserve(bytes.len())?;
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(self.root.join(&name))?;
            file.write_all(bytes)?;
        }
        Ok(name)
    }
    // Final metadata is small and must remain writable after a budget stop.
    pub fn manifest(&self, value: &Value) -> Result<()> {
        let path = self.root.join("manifest.json");
        let temporary = self.root.join("manifest.json.tmp");
        let mut file = File::create(&temporary)?;
        file.write_all(serde_json::to_string_pretty(value)?.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(temporary, path)?;
        Ok(())
    }
}

pub fn control(out: &std::path::Path, kind: &str, note: Option<&str>) -> Result<()> {
    let manifest: Value = serde_json::from_slice(&std::fs::read(out.join("manifest.json"))?)?;
    if manifest["kind"] != "BrowserRecording" || manifest["status"] != "recording" {
        return Err("not an active browser recording".into());
    }
    if note.is_some_and(|note| note.len() > 4096) {
        return Err("note exceeds 4096 bytes".into());
    }
    let value = json!({"session_id":manifest["session_id"],"kind":kind,"note":note});
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(out.join("commands.jsonl"))?;
    writeln!(file, "{value}")?;
    file.flush()?;
    Ok(())
}
