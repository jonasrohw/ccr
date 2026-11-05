//! Antigravity CLI (`agy`) SQLite sessions. All database access is read-only.
//! The protobuf field numbers are an observed storage format, not a public API.
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local};
use rusqlite::{Connection, OpenFlags};
use std::{
    path::{Path, PathBuf},
    process::Command,
};

use crate::{
    backends::Backend,
    session::{
        PLACEHOLDER_TITLE, PREVIEW_TURNS, Role, Session, TITLE_MAX, Turn, append_searchable,
    },
    util::{file_mtime, is_possibly_live, pgrep_session_flags, truncate},
};

pub struct AntigravityBackend;

fn open(path: &Path) -> Result<Connection> {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("read Antigravity database {}", path.display()))
}

impl AntigravityBackend {
    fn root() -> Result<PathBuf> {
        if let Some(path) = std::env::var_os("CCR_ANTIGRAVITY_DIR") {
            return Ok(path.into());
        }
        Ok(dirs::home_dir()
            .context("no home dir")?
            .join(".gemini/antigravity-cli"))
    }

    fn scan_root(root: &Path) -> Result<Vec<Session>> {
        let index = root.join("conversation_summaries.db");
        if !index.exists() {
            return Ok(Vec::new());
        }
        let db = open(&index)?;
        let mut stmt = db.prepare("SELECT conversation_id, title, workspace_uris, last_modified_time FROM conversation_summaries")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        let mut sessions = Vec::new();
        for row in rows {
            let (id, title, uris, timestamp) = row?;
            // IDs become filenames; never allow an index entry to escape the store.
            if id.is_empty() || !id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-') {
                continue;
            }
            let origin = root.join("conversations").join(format!("{id}.db"));
            if !origin.is_file() {
                continue;
            }
            let cwd = serde_json::from_str::<Vec<String>>(&uris)
                .ok()
                .and_then(|uris| uris.iter().find_map(|uri| file_uri(uri)))
                .unwrap_or_else(|| PathBuf::from("(unknown workspace)"));
            let Ok(turns) = Self.all_turns_at(&origin) else {
                continue;
            };
            let last_activity = DateTime::parse_from_rfc3339(&timestamp.replace(' ', "T"))
                .map(|t| t.with_timezone(&Local))
                .unwrap_or_else(|_| file_mtime(&origin));
            let mut searchable = String::new();
            for turn in &turns {
                append_searchable(&mut searchable, &turn.text);
            }
            let title = if title.trim().is_empty() {
                turns
                    .iter()
                    .rev()
                    .find(|t| t.role == Role::User)
                    .map(|t| truncate(&t.text, TITLE_MAX))
                    .unwrap_or_else(|| PLACEHOLDER_TITLE.into())
            } else {
                truncate(&title, TITLE_MAX)
            };
            let message_count = Some(turns.len());
            let preview = turns
                .into_iter()
                .rev()
                .take(PREVIEW_TURNS)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            sessions.push(Session {
                backend: "antigravity",
                id,
                cwd,
                title,
                last_activity,
                message_count,
                preview,
                possibly_live: is_possibly_live(last_activity),
                origin,
                searchable,
            });
        }
        Ok(sessions)
    }
}

impl Backend for AntigravityBackend {
    fn name(&self) -> &'static str {
        "antigravity"
    }
    fn scan(&self) -> Result<Vec<Session>> {
        Self::scan_root(&Self::root()?)
    }
    fn resume(&self, s: &Session) -> Command {
        let mut cmd = Command::new("agy");
        cmd.args(["--conversation", &s.id]).current_dir(&s.cwd);
        cmd
    }
    fn open_with_prompt(&self, cwd: &Path, prompt: &str) -> Command {
        let mut cmd = Command::new("agy");
        cmd.args(["--prompt-interactive", prompt]).current_dir(cwd);
        cmd
    }
    fn running(&self, s: &Session) -> Vec<String> {
        pgrep_session_flags(&s.id, &["--conversation"])
    }
    fn all_turns_at(&self, origin: &Path) -> Result<Vec<Turn>> {
        let db = open(origin)?;
        let mut stmt = db.prepare(
            "SELECT step_type, step_payload FROM steps WHERE step_type IN (14,15) ORDER BY idx",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?)))?;
        let mut turns = Vec::new();
        for row in rows {
            let (kind, payload) = row?;
            // User input: Step.19.text (2). Response: Step.20.response (1).
            // Do not expose thinking, signatures, tool arguments, or metadata.
            let (outer, inner, role) = if kind == 14 {
                (19, 2, Role::User)
            } else {
                (20, 1, Role::Assistant)
            };
            for field in bytes_fields(&payload, outer)? {
                for text in bytes_fields(field, inner)? {
                    let text =
                        std::str::from_utf8(text).context("invalid Antigravity message text")?;
                    if !text.trim().is_empty() {
                        turns.push(Turn {
                            role,
                            text: text.into(),
                        });
                    }
                }
            }
        }
        Ok(turns)
    }
    fn show(&self, s: &Session) -> Result<String> {
        let db = open(&s.origin)?;
        let mut stmt =
            db.prepare("SELECT idx, step_type, hex(step_payload) FROM steps ORDER BY idx")?;
        let rows = stmt.query_map([], |r| Ok(serde_json::json!({"idx": r.get::<_, i64>(0)?, "step_type": r.get::<_, i64>(1)?, "step_payload_hex": r.get::<_, String>(2)?})))?;
        let mut out = String::new();
        for row in rows {
            out.push_str(&serde_json::to_string(&row?)?);
            out.push('\n');
        }
        Ok(out)
    }
}

fn file_uri(uri: &str) -> Option<PathBuf> {
    let path = uri.strip_prefix("file://")?;
    let path = path
        .strip_prefix("localhost/")
        .map(|p| format!("/{p}"))
        .unwrap_or_else(|| path.into());
    if !path.starts_with('/') {
        return None;
    }
    let mut bytes = Vec::new();
    let mut input = path.bytes();
    while let Some(b) = input.next() {
        bytes.push(if b == b'%' {
            let hi = (input.next()? as char).to_digit(16)?;
            let lo = (input.next()? as char).to_digit(16)?;
            (hi * 16 + lo) as u8
        } else {
            b
        });
    }
    let path = String::from_utf8(bytes).ok()?;
    if path.contains('\0') {
        return None;
    }
    Some(path.into())
}

fn varint(input: &mut &[u8]) -> Result<u64> {
    let mut value = 0u64;
    for shift in (0..70).step_by(7) {
        let (&byte, rest) = input.split_first().context("truncated protobuf varint")?;
        *input = rest;
        if shift == 63 && byte > 1 {
            bail!("protobuf varint overflow");
        }
        value |= u64::from(byte & 127) << shift;
        if byte & 128 == 0 {
            return Ok(value);
        }
    }
    bail!("protobuf varint overflow")
}

/// Select length-delimited fields while validating bounds and skipping unknown fields.
fn bytes_fields(mut input: &[u8], wanted: u64) -> Result<Vec<&[u8]>> {
    let mut out = Vec::new();
    while !input.is_empty() {
        let key = varint(&mut input)?;
        if key >> 3 == 0 {
            bail!("invalid protobuf field");
        }
        let wire = key & 7;
        let len = match wire {
            0 => {
                varint(&mut input)?;
                continue;
            }
            1 => 8,
            2 => usize::try_from(varint(&mut input)?)?,
            5 => 4,
            _ => bail!("unsupported protobuf wire type"),
        };
        let field = input.get(..len).context("truncated protobuf field")?;
        input = &input[len..];
        if wire == 2 && key >> 3 == wanted {
            out.push(field);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    struct Fixture(PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn field(number: u64, value: &[u8]) -> Vec<u8> {
        fn encode(mut n: u64, out: &mut Vec<u8>) {
            while n >= 128 {
                out.push((n as u8 & 127) | 128);
                n >>= 7;
            }
            out.push(n as u8);
        }
        let mut out = Vec::new();
        encode(number * 8 + 2, &mut out);
        encode(value.len() as u64, &mut out);
        out.extend(value);
        out
    }
    fn fixture() -> Fixture {
        let root = std::env::temp_dir().join(format!(
            "ccr-agy-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(root.join("conversations")).unwrap();
        let index = Connection::open(root.join("conversation_summaries.db")).unwrap();
        index.execute_batch("CREATE TABLE conversation_summaries (conversation_id TEXT, title TEXT, workspace_uris TEXT, last_modified_time TEXT);
            INSERT INTO conversation_summaries VALUES ('test-id', 'A session', '[\"file:///tmp/my%20project\"]', '2026-10-26 12:34:56.123456+00:00');
            INSERT INTO conversation_summaries VALUES ('missing', '', '[]', 'bad');
            INSERT INTO conversation_summaries VALUES ('../escape', '', '[]', 'bad');").unwrap();
        let db = Connection::open(root.join("conversations/test-id.db")).unwrap();
        db.execute_batch("CREATE TABLE steps (idx INTEGER, step_type INTEGER, step_payload BLOB)")
            .unwrap();
        let mut response = field(3, b"private thinking");
        response.extend(field(1, "Hello, 世界".as_bytes()));
        // Insert out of order to exercise chronological reading.
        db.execute(
            "INSERT INTO steps VALUES (2, 15, ?)",
            [field(20, &response)],
        )
        .unwrap();
        db.execute(
            "INSERT INTO steps VALUES (1, 14, ?)",
            [field(19, &field(2, b"Fix this"))],
        )
        .unwrap();
        db.execute(
            "INSERT INTO steps VALUES (3, 101, ?)",
            [field(19, &field(2, b"system instruction"))],
        )
        .unwrap();
        Fixture(root)
    }
    #[test]
    fn scans_reads_and_shows_sqlite_without_modifying_it() {
        let f = fixture();
        let path = f.0.join("conversations/test-id.db");
        let before = std::fs::read(&path).unwrap();
        let sessions = AntigravityBackend::scan_root(&f.0).unwrap();
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!(s.backend, "antigravity");
        assert_eq!(s.cwd, Path::new("/tmp/my project"));
        assert_eq!(s.message_count, Some(2));
        assert_eq!(s.last_activity.timestamp(), 1793018096);
        assert_eq!(s.preview[0].text, "Fix this");
        assert_eq!(s.preview[1].text, "Hello, 世界");
        assert!(!s.searchable.contains("thinking"));
        assert!(!s.searchable.contains("system"));
        let dump = AntigravityBackend.show(s).unwrap();
        assert_eq!(dump.lines().count(), 3);
        for line in dump.lines() {
            assert!(
                serde_json::from_str::<serde_json::Value>(line).unwrap()["step_payload_hex"]
                    .is_string()
            );
        }
        assert_eq!(std::fs::read(path).unwrap(), before);
        let cmd = AntigravityBackend.resume(s);
        assert_eq!(cmd.get_program(), "agy");
        assert_eq!(
            cmd.get_args().collect::<Vec<_>>(),
            ["--conversation", "test-id"]
        );
        assert_eq!(cmd.get_current_dir(), Some(s.cwd.as_path()));
    }
    #[test]
    fn prompt_is_a_single_argument() {
        let prompt = "--flag 'quote' $(touch nope)\nnext line";
        let cmd = AntigravityBackend.open_with_prompt(Path::new("/tmp"), prompt);
        assert_eq!(
            cmd.get_args().collect::<Vec<_>>(),
            ["--prompt-interactive", prompt]
        );
    }
    #[test]
    fn missing_and_corrupt_stores() {
        let f = fixture();
        assert!(
            AntigravityBackend::scan_root(&f.0.join("absent"))
                .unwrap()
                .is_empty()
        );
        std::fs::write(f.0.join("conversations/test-id.db"), b"invalid sqlite").unwrap();
        assert!(AntigravityBackend::scan_root(&f.0).unwrap().is_empty());
    }
    #[test]
    fn malformed_protobuf_never_panics() {
        for bytes in [
            vec![0],
            vec![0x80],
            vec![0xff; 20],
            vec![0x0a, 0xff, 0x01],
            vec![0x0b],
        ] {
            assert!(bytes_fields(&bytes, 1).is_err());
        }
        assert_eq!(bytes_fields(&[8, 1, 0x12, 1, b'x'], 2).unwrap(), vec![b"x"]);
    }
    #[test]
    fn workspace_uri_decoding() {
        assert_eq!(
            file_uri("file://localhost/tmp/a%20b"),
            Some("/tmp/a b".into())
        );
        for uri in [
            "https://host/tmp",
            "file://remote/tmp",
            "file:///tmp/%zz",
            "file:///tmp/%00",
        ] {
            assert!(file_uri(uri).is_none());
        }
    }
}
