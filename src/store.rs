use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::path::Path;
use std::sync::Mutex;

pub struct Store {
    conn: Mutex<Connection>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Session {
    pub id: String,
    pub title: String,
    pub agent: String,
    pub cwd: String,
    pub vendor_id: Option<String>,
    pub status: String,
    pub updated: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Message {
    pub role: String,
    pub content: String,
    pub ts: i64,
}

pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    #[cfg(test)]
    pub fn memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS sessions(
               id TEXT PRIMARY KEY, title TEXT NOT NULL, agent TEXT NOT NULL, cwd TEXT NOT NULL,
               vendor_id TEXT, status TEXT NOT NULL, created INTEGER NOT NULL, updated INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS messages(
               id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL REFERENCES sessions(id),
               role TEXT NOT NULL, content TEXT NOT NULL, ts INTEGER NOT NULL);
             CREATE INDEX IF NOT EXISTS messages_session ON messages(session_id, id);
             CREATE TABLE IF NOT EXISTS plans(
               id TEXT PRIMARY KEY, json TEXT NOT NULL, updated INTEGER NOT NULL);",
        )?;
        // A previous run that died mid-turn leaves sessions marked running.
        conn.execute("UPDATE sessions SET status='idle' WHERE status='running'", [])?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn c(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn create_session(&self, title: &str, agent: &str, cwd: &str) -> Result<Session> {
        let id = uuid::Uuid::new_v4().to_string()[..8].to_string();
        let t = now();
        self.c().execute(
            "INSERT INTO sessions(id,title,agent,cwd,status,created,updated) VALUES(?1,?2,?3,?4,'idle',?5,?5)",
            params![id, title, agent, cwd, t],
        )?;
        Ok(Session {
            id,
            title: title.into(),
            agent: agent.into(),
            cwd: cwd.into(),
            vendor_id: None,
            status: "idle".into(),
            updated: t,
        })
    }

    pub fn sessions(&self) -> Result<Vec<Session>> {
        let c = self.c();
        let mut st = c.prepare(
            "SELECT id,title,agent,cwd,vendor_id,status,updated FROM sessions ORDER BY updated DESC",
        )?;
        let rows = st.query_map([], |r| {
            Ok(Session {
                id: r.get(0)?,
                title: r.get(1)?,
                agent: r.get(2)?,
                cwd: r.get(3)?,
                vendor_id: r.get(4)?,
                status: r.get(5)?,
                updated: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn session(&self, id: &str) -> Result<Option<Session>> {
        Ok(self.sessions()?.into_iter().find(|s| s.id == id))
    }

    pub fn set_session_status(&self, id: &str, status: &str) -> Result<()> {
        self.c().execute(
            "UPDATE sessions SET status=?2, updated=?3 WHERE id=?1",
            params![id, status, now()],
        )?;
        Ok(())
    }

    /// Switching agent drops the vendor session id: it belongs to the old agent.
    pub fn set_session_agent(&self, id: &str, agent: &str) -> Result<()> {
        self.c().execute(
            "UPDATE sessions SET agent=?2, vendor_id=NULL, updated=?3 WHERE id=?1",
            params![id, agent, now()],
        )?;
        Ok(())
    }

    pub fn set_vendor_id(&self, id: &str, vendor_id: &str) -> Result<()> {
        self.c().execute("UPDATE sessions SET vendor_id=?2 WHERE id=?1", params![id, vendor_id])?;
        Ok(())
    }

    pub fn add_message(&self, session_id: &str, role: &str, content: &str) -> Result<()> {
        let t = now();
        let c = self.c();
        c.execute(
            "INSERT INTO messages(session_id,role,content,ts) VALUES(?1,?2,?3,?4)",
            params![session_id, role, content, t],
        )?;
        c.execute("UPDATE sessions SET updated=?2 WHERE id=?1", params![session_id, t])?;
        Ok(())
    }

    pub fn messages(&self, session_id: &str) -> Result<Vec<Message>> {
        let c = self.c();
        let mut st =
            c.prepare("SELECT role,content,ts FROM messages WHERE session_id=?1 ORDER BY id")?;
        let rows = st.query_map([session_id], |r| {
            Ok(Message { role: r.get(0)?, content: r.get(1)?, ts: r.get(2)? })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn save_plan(&self, id: &str, json: &str) -> Result<()> {
        self.c().execute(
            "INSERT INTO plans(id,json,updated) VALUES(?1,?2,?3)
             ON CONFLICT(id) DO UPDATE SET json=excluded.json, updated=excluded.updated",
            params![id, json, now()],
        )?;
        Ok(())
    }

    /// Plan JSON documents, newest first.
    pub fn plans(&self) -> Result<Vec<String>> {
        let c = self.c();
        let mut st = c.prepare("SELECT json FROM plans ORDER BY updated DESC")?;
        let rows = st.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn plan(&self, id: &str) -> Result<Option<String>> {
        Ok(self
            .c()
            .query_row("SELECT json FROM plans WHERE id=?1", [id], |r| r.get(0))
            .optional()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_roundtrip() {
        let s = Store::memory().unwrap();
        let a = s.create_session("t", "claude", "C:/x").unwrap();
        s.set_vendor_id(&a.id, "v1").unwrap();
        s.add_message(&a.id, "user", "hi").unwrap();
        s.add_message(&a.id, "assistant", "yo").unwrap();
        let m = s.messages(&a.id).unwrap();
        assert_eq!(m.len(), 2);
        assert_eq!(m[1].content, "yo");
        assert_eq!(s.session(&a.id).unwrap().unwrap().vendor_id.as_deref(), Some("v1"));
        s.set_session_agent(&a.id, "codex").unwrap();
        let got = s.session(&a.id).unwrap().unwrap();
        assert_eq!(got.agent, "codex");
        assert!(got.vendor_id.is_none());
    }

    #[test]
    fn plan_upsert() {
        let s = Store::memory().unwrap();
        s.save_plan("p1", "{}").unwrap();
        s.save_plan("p1", "{\"a\":1}").unwrap();
        assert_eq!(s.plans().unwrap().len(), 1);
        assert_eq!(s.plan("p1").unwrap().unwrap(), "{\"a\":1}");
    }
}
