//! SQLite: messages, and the sessions this machine has seen: live ones, ones gone for less than a day, and the
//! person's own mailboxes (`terminal`, `listen --as` names). Files that came with messages are saved in the inbox folder, one
//! folder per message, and go with their message. Mail dropped unread tells its sender (see [`notice`]).

use crate::{
    files,
    proto::{Attachment, DayCount, Message, Tier},
    render, util,
};
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use std::{
    cell::RefCell,
    collections::HashSet,
    path::{Path, PathBuf},
};

pub struct Store {
    db: Connection,
    inbox: PathBuf,
    /// Notices stored for senders here since the daemon last took them, for it to deliver.
    notices: RefCell<Vec<Message>>,
}

/// The agent a notice that mail was dropped comes from: Reason Network itself. A message from it never gets a notice.
pub const NOTICE_AGENT: &str = "reason-network";

/// What the `files` column keeps of a message's files: their folder in the inbox folder, named by an id made here
/// (never the sender's message id, which a teammate could pick to name another's folder, in another case, or a device
/// name), their size, and their names as saved (and their text while the message is held).
#[derive(Serialize, Deserialize)]
struct Kept {
    dir: String,
    bytes: usize,
    files: Vec<Attachment>,
}

/// What [`Store::count`] counts: a message a session here sent, once the receiving machine took it, or one that reached
/// a mailbox here (counted by whoever stores it, or once approved if it was held).
#[derive(Clone, Copy)]
pub enum Counted {
    Sent,
    Received,
}

/// The days whose message counts are shown: today, the six before it, and tomorrow (SQLite date modifiers).
const COUNTS_FROM: &str = "-6 days";
const COUNTS_TO: &str = "+1 day";
/// Days of counts kept: the last ones started, in the order they started rather than by date, so a clock that jumps
/// doesn't push out the real ones.
const COUNTS_KEPT: i64 = 8;
const RECEIPT_KEEP_MS: i64 = 7 * 86_400_000;

/// A held message that can't be approved now, and why (it stays held).
pub type Unsaved = (String, String);

#[derive(Debug, Clone)]
pub struct Remembered {
    pub name: String,
    pub key: String,
    pub agent: String,
    pub cwd: String,
    pub repo: Option<String>,
    pub branch: Option<String>,
    pub doing: Option<String>,
    pub tier: Tier,
    pub last_seen: i64,
    /// Whether it could ever be reached beyond its own hooks (see `Live::attached`).
    pub attached: bool,
    /// One of the person's own mailboxes (`terminal`, a `listen --as` name): kept for good, never forgotten.
    pub mailbox: bool,
}

impl Remembered {
    /// Whether the name stays this session's while it is offline: it is an agent's conversation, by the id its agent
    /// gave it (`<agent>:<id>`), so the same conversation can come back to it, or one of the person's own mailboxes
    /// (`cli:<name>`); and it could be reached while it ran. A session known only by its process (`proc:<pid>`) has
    /// nothing that could come back, and one only its hooks ever knew (a one-shot run) nothing anyone could reach.
    pub fn kept(&self) -> bool {
        self.attached && !self.key.starts_with("proc:")
    }
}

const SCHEMA: &str = "
PRAGMA journal_mode = WAL;
CREATE TABLE IF NOT EXISTS messages (
    id TEXT PRIMARY KEY,
    sender TEXT NOT NULL,
    sender_agent TEXT,
    sender_repo TEXT,
    recipient TEXT NOT NULL,
    via TEXT,
    reply_to TEXT,
    body TEXT NOT NULL,
    wake INTEGER NOT NULL,
    sent_at INTEGER NOT NULL,
    read_at INTEGER,
    held INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS messages_unread ON messages (recipient, read_at);
CREATE TABLE IF NOT EXISTS outbox (
    id TEXT PRIMARY KEY,
    target TEXT NOT NULL,
    msg TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS kv (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS send_keys (
    key TEXT PRIMARY KEY,
    fingerprint TEXT NOT NULL,
    receipt TEXT,
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS sent_receipts (
    id TEXT PRIMARY KEY, sender_key TEXT NOT NULL, sender TEXT NOT NULL, recipient TEXT NOT NULL,
    stage TEXT NOT NULL, sent_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS send_keys_created_at ON send_keys(created_at);
CREATE INDEX IF NOT EXISTS sent_receipts_sent_at ON sent_receipts(sent_at);
CREATE TABLE IF NOT EXISTS sessions (
    name TEXT PRIMARY KEY,
    key TEXT NOT NULL,
    agent TEXT NOT NULL,
    cwd TEXT NOT NULL,
    repo TEXT,
    branch TEXT,
    doing TEXT,
    tier TEXT NOT NULL,
    last_seen INTEGER NOT NULL,
    attached INTEGER NOT NULL DEFAULT 1,
    mailbox INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS sessions_key ON sessions (key);
CREATE TABLE IF NOT EXISTS counts (
    day TEXT PRIMARY KEY,
    sent INTEGER NOT NULL DEFAULT 0,
    received INTEGER NOT NULL DEFAULT 0
);
";

const MESSAGE_COLS: &str =
    "id, sender, sender_agent, sender_repo, recipient, via, reply_to, body, wake, sent_at, approval, files";
const SESSION_COLS: &str = "name, key, agent, cwd, repo, branch, doing, tier, last_seen, attached, mailbox";

impl Store {
    pub fn record_sent(&self, key: &str, message: &Message, recipient: &str, state: &str) -> Result<()> {
        let stage = match state {
            "refused" => "rejected",
            "held" => "held",
            "queued" => "queued",
            _ => "accepted",
        };
        self.prune_receipts(crate::util::now_ms())?;
        self.db.execute(
            "INSERT OR IGNORE INTO sent_receipts VALUES (?1,?2,?3,?4,?5,?6,?6)",
            params![message.id, key, message.from, recipient, stage, message.sent_at],
        )?;
        Ok(())
    }
    pub fn handoff(&self, ids: &[String], now: i64) -> Result<()> {
        for id in ids {
            self.db.execute(
                "UPDATE sent_receipts SET stage='handed_to_agent',updated_at=?2 WHERE id=?1 AND stage <> 'replied'",
                params![id, now],
            )?;
        }
        Ok(())
    }

    pub fn send_outcome(&self, id: &str, state: &str) -> Result<()> {
        let stage = match state {
            "refused" => "rejected",
            "held" => "held",
            "queued" => "queued",
            _ => "accepted",
        };
        self.db.execute("UPDATE sent_receipts SET stage=?2,updated_at=?3 WHERE id=?1 AND stage NOT IN ('handed_to_agent','replied')", params![id,stage,crate::util::now_ms()])?;
        Ok(())
    }

    pub fn queue_again(&self, ids: &[String]) -> Result<()> {
        for id in ids {
            self.db.execute("UPDATE sent_receipts SET stage='queued',updated_at=?2 WHERE id=?1 AND stage NOT IN ('handed_to_agent','replied')", params![id,crate::util::now_ms()])?;
        }
        Ok(())
    }
    pub fn delivery_status(&self, key: &str, ids: &[String]) -> Result<Vec<serde_json::Value>> {
        anyhow::ensure!(ids.len() <= 100, "request at most 100 delivery receipts");
        let mut out = Vec::new();
        for id in ids {
            let found = self.db.query_row("SELECT recipient,stage,sent_at,updated_at FROM sent_receipts WHERE id=?1 AND sender_key=?2 AND sent_at>=?3", params![id,key,crate::util::now_ms()-RECEIPT_KEEP_MS], |r| Ok(serde_json::json!({"id":id,"recipient":r.get::<_,String>(0)?,"stage":r.get::<_,String>(1)?,"sentAt":r.get::<_,i64>(2)?,"updatedAt":r.get::<_,i64>(3)?}))).optional()?;
            out.push(found.unwrap_or_else(|| serde_json::json!({"id":id,"stage":"unknown"})));
        }
        Ok(out)
    }
    /// Returns the nonce, saved receipt, and whether this call inserted the key.
    /// No freshness claim escapes a failed fingerprint check or receipt lookup.
    pub fn begin_send(
        &self,
        key: &str,
        fingerprint: &str,
        now: i64,
    ) -> Result<(String, Option<crate::proto::SendResult>, bool)> {
        self.prune_receipts(now)?;
        let inserted = self.db.execute(
            "INSERT OR IGNORE INTO send_keys(key,fingerprint,created_at,nonce) VALUES(?1,?2,?3,?4)",
            params![key, fingerprint, now, ulid::Ulid::generate().to_string()],
        )?;
        let (old, receipt, nonce): (String, Option<String>, String) =
            self.db.query_row("SELECT fingerprint,receipt,nonce FROM send_keys WHERE key=?1", [key], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?;
        anyhow::ensure!(
            old == fingerprint,
            "idempotency key already used for a different message; use a new key for a corrected or different message"
        );
        Ok((nonce, receipt.map(|v| serde_json::from_str(&v)).transpose()?, inserted == 1))
    }

    pub fn finish_send(&self, key: &str, receipt: &crate::proto::SendResult) -> Result<()> {
        self.db
            .execute("UPDATE send_keys SET receipt=?2 WHERE key=?1", params![key, serde_json::to_string(receipt)?])?;
        Ok(())
    }

    fn prune_receipts(&self, now: i64) -> Result<()> {
        self.db.execute("DELETE FROM sent_receipts WHERE sent_at < ?1", [now - RECEIPT_KEEP_MS])?;
        self.db.execute("DELETE FROM send_keys WHERE created_at < ?1", [now - RECEIPT_KEEP_MS])?;
        Ok(())
    }

    fn discard_receipt(&self, id: &str, stage: &str, now: i64) -> Result<()> {
        self.db.execute("UPDATE sent_receipts SET stage=?2,updated_at=?3 WHERE id=?1 AND stage NOT IN ('handed_to_agent','replied')",params![id,stage,now])?;
        Ok(())
    }
    pub fn queue_health(&self, address: &str) -> Result<(u32, Option<i64>)> {
        Ok(self.db.query_row(
            "SELECT count(*), min(sent_at) FROM messages WHERE recipient=?1 AND read_at IS NULL AND held=0",
            [address],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?)
    }
    /// The database at `path`, and the folder `inbox` that messages' files are saved in.
    pub fn open(path: &Path, inbox: PathBuf) -> Result<Self> {
        let db = Connection::open(path)?;
        db.execute_batch(SCHEMA)?;
        // Deleted rows are overwritten, not just let go (see `scrub`).
        db.execute_batch("PRAGMA secure_delete = ON;")?;
        // Only this user reads it, like machine.key: it holds the team link. (SQLite gives the log files the mode the
        // database has, but older installs made them with the default.)
        #[cfg(unix)]
        for suffix in ["", "-wal", "-shm"] {
            let mut p = path.as_os_str().to_owned();
            p.push(suffix);
            let p = PathBuf::from(p);
            if p.exists() {
                crate::util::chmod(&p, 0o600)?;
            }
        }
        // Added in 0.10.7.
        if !db.prepare("SELECT 1 FROM pragma_table_info('messages') WHERE name = 'approval'")?.exists([])? {
            db.execute("ALTER TABLE messages ADD COLUMN approval TEXT", [])?;
        }
        // Added with files: their names, and their text while the message is held (once saved, it is in the inbox
        // folder instead).
        if !db.prepare("SELECT 1 FROM pragma_table_info('messages') WHERE name = 'files'")?.exists([])? {
            db.execute("ALTER TABLE messages ADD COLUMN files TEXT", [])?;
        }
        // Added with names kept for their sessions: whether a session could be reached beyond its own hooks. Sessions
        // remembered from before count as reachable, so their names stay theirs.
        if !db.prepare("SELECT 1 FROM pragma_table_info('sessions') WHERE name = 'attached'")?.exists([])? {
            db.execute("ALTER TABLE sessions ADD COLUMN attached INTEGER NOT NULL DEFAULT 1", [])?;
        }
        // Added when the person's own mailboxes were told from other `cli` sessions. Before, every `cli` session was
        // kept for good; of those remembered, the Reason CLI's one-off ask sessions (`ask-` and six hex digits, as it
        // names them) are sessions like any other from now on.
        if !db.prepare("SELECT 1 FROM pragma_table_info('sessions') WHERE name = 'mailbox'")?.exists([])? {
            let tx = db.unchecked_transaction()?;
            tx.execute("ALTER TABLE sessions ADD COLUMN mailbox INTEGER NOT NULL DEFAULT 0", [])?;
            tx.execute(
                "UPDATE sessions SET mailbox = 1 WHERE agent = 'cli'
                 AND NOT (length(name) = 10 AND name GLOB 'ask-[0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f]')",
                [],
            )?;
            tx.commit()?;
        }
        for (column, definition) in [
            ("native_app", "INTEGER NOT NULL DEFAULT 0"),
            ("native_hook_at", "INTEGER"),
            ("native_dirs", "TEXT NOT NULL DEFAULT '[]'"),
        ] {
            if !db.prepare("SELECT 1 FROM pragma_table_info('sessions') WHERE name=?1")?.exists([column])? {
                db.execute(&format!("ALTER TABLE sessions ADD COLUMN {column} {definition}"), [])?;
            }
        }
        if !db.prepare("SELECT 1 FROM pragma_table_info('send_keys') WHERE name='nonce'")?.exists([])? {
            // Empty preserves the identity derivation of an unfinished send from an earlier engine. New keys get a nonce.
            db.execute("ALTER TABLE send_keys ADD COLUMN nonce TEXT NOT NULL DEFAULT ''", [])?;
        }
        // Added with notices: the teammate's machine a message came from, which its notice goes back to, or empty for
        // one sent here. Mail kept from before has none (see `sourceless`).
        if !db.prepare("SELECT 1 FROM pragma_table_info('messages') WHERE name = 'source'")?.exists([])? {
            db.execute("ALTER TABLE messages ADD COLUMN source TEXT", [])?;
        }
        Ok(Store { db, inbox, notices: RefCell::default() })
    }

    /// A folder named in the `files` column: only one of the ids made here (a ULID), so always just inside the inbox.
    fn dir(&self, kept: &Kept) -> Option<PathBuf> {
        ulid::Ulid::from_string(&kept.dir)
            .ok()
            .filter(|u| u.to_string() == kept.dir)
            .map(|_| self.inbox.join(&kept.dir))
    }

    fn kept(&self, id: &str) -> Result<Option<Kept>> {
        let col: Option<Option<String>> =
            self.db.query_row("SELECT files FROM messages WHERE id = ?1", [id], |r| r.get(0)).optional()?;
        Ok(col.flatten().and_then(|c| serde_json::from_str(&c).ok()))
    }

    /// Stores a message; false if one with this id is already here (a retried delivery). Unless it is held, its files
    /// are saved in a new folder of their own first, and `m` then lists them by path.
    pub fn insert(&self, m: &mut Message, held: bool) -> Result<bool> {
        self.insert_from(m, held, None)
    }

    /// The same, for a message from a teammate's machine (`source`, its endpoint id; `None`: sent here).
    pub fn insert_from(&self, m: &mut Message, held: bool, source: Option<&str>) -> Result<bool> {
        if self.has(&m.id)? {
            return Ok(false);
        }
        let mut kept = (!m.files.is_empty()).then(|| Kept {
            dir: ulid::Ulid::generate().to_string(),
            bytes: files::size(&m.files),
            files: m.files.iter().map(|f| Attachment { path: None, ..f.clone() }).collect(),
        });
        let mut saved = None;
        if let Some(k) = kept.as_mut().filter(|_| !held) {
            let dir = self.inbox.join(&k.dir);
            k.files = files::save(&dir, &k.files)?;
            saved = Some(dir);
        }
        let n = self.db.execute(
            &format!(
                "INSERT OR IGNORE INTO messages ({MESSAGE_COLS}, held, source) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)"
            ),
            params![
                m.id,
                m.from,
                m.from_agent,
                m.from_repo,
                m.to,
                m.via,
                m.reply_to,
                m.body,
                m.wake,
                m.sent_at,
                m.approval.as_ref().map(|a| serde_json::to_string(a).expect("approvals serialize")),
                kept.as_ref().map(|k| serde_json::to_string(k).expect("files serialize")),
                held,
                source.unwrap_or_default()
            ],
        );
        if n.as_ref().is_ok_and(|n| *n == 1) && !held {
            let _ = self.observe_reply(m);
        }
        match (n, saved, kept) {
            (Ok(n), Some(dir), Some(k)) => {
                m.files = files::located(&dir, &k.files);
                Ok(n == 1)
            }
            (Ok(n), ..) => Ok(n == 1),
            (Err(e), dir, _) => {
                if let Some(dir) = dir {
                    let _ = std::fs::remove_dir_all(dir);
                }
                Err(e.into())
            }
        }
    }

    pub fn message(&self, id: &str) -> Result<Option<Message>> {
        let sql = format!("SELECT {MESSAGE_COLS}, held FROM messages WHERE id = ?1");
        Ok(self.db.query_row(&sql, [id], |r| self.row(r)).optional()?)
    }

    fn observe_reply(&self, message: &Message) -> Result<()> {
        // A notice names the message it is about as the one it answers, but the mail was dropped, not replied to.
        if message.from_agent.as_deref() == Some(NOTICE_AGENT) {
            return Ok(());
        }
        if let Some(reply) = &message.reply_to {
            self.db.execute("UPDATE sent_receipts SET stage='replied',updated_at=?4 WHERE id=?1 AND recipient=?2 AND sender=?3 AND stage <> 'replied'", params![reply,message.from,message.to,crate::util::now_ms()])?;
        }
        Ok(())
    }

    pub fn has(&self, id: &str) -> Result<bool> {
        Ok(self.db.query_row("SELECT 1 FROM messages WHERE id = ?1", [id], |_| Ok(())).optional()?.is_some())
    }

    /// A mailbox's unread messages, in the order they were sent: those sent the same millisecond in the order they came
    /// (their ids, random within a millisecond, say nothing of it).
    pub fn unread(&self, recipient: &str) -> Result<Vec<Message>> {
        let mut st = self.db.prepare_cached(&format!(
            "SELECT {MESSAGE_COLS}, held FROM messages WHERE recipient = ?1 AND read_at IS NULL AND held = 0 ORDER BY sent_at, rowid"
        ))?;
        let rows = st.query_map([recipient], |r| self.row(r))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Whether a mailbox has unread mail that wakes its session.
    pub fn unread_wakes(&self, recipient: &str) -> Result<bool> {
        Ok(self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM messages WHERE recipient = ?1 AND read_at IS NULL AND held = 0 AND wake != 0)",
            [recipient],
            |r| r.get(0),
        )?)
    }

    pub fn unread_count(&self, recipient: &str) -> Result<u32> {
        Ok(self.db.query_row(
            "SELECT COUNT(*) FROM messages WHERE recipient = ?1 AND read_at IS NULL AND held = 0",
            [recipient],
            |r| r.get(0),
        )?)
    }

    pub fn held(&self) -> Result<Vec<Message>> {
        let mut st = self.db.prepare_cached(&format!(
            "SELECT {MESSAGE_COLS}, held FROM messages WHERE held = 1 ORDER BY sent_at, rowid"
        ))?;
        let rows = st.query_map([], |r| self.row(r))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The bytes of teammates' files this machine keeps: saved, or held for approval.
    pub fn files_kept(&self) -> Result<usize> {
        let sql = "SELECT COALESCE(SUM(json_extract(files, '$.bytes')), 0) FROM messages WHERE files IS NOT NULL";
        Ok(self.db.query_row(sql, [], |r| r.get::<_, i64>(0))?.max(0) as usize)
    }

    /// The same, of files from `person`'s sessions.
    pub fn files_kept_from(&self, person: &str) -> Result<usize> {
        let sql = "SELECT COALESCE(SUM(json_extract(files, '$.bytes')), 0) FROM messages
                   WHERE files IS NOT NULL AND substr(sender, 1, length(?1) + 1) = ?1 || '/'";
        Ok(self.db.query_row(sql, [person], |r| r.get::<_, i64>(0))?.max(0) as usize)
    }

    /// Lets files go: a day after their message was read, a week after they came while it is unread (the message stays,
    /// without them), and a message held a week with files goes altogether. Then deletes every folder in the inbox
    /// folder that no kept message names.
    pub fn sweep_files(&self, now: i64) -> Result<()> {
        self.prune_receipts(now)?;
        let mut st = self.db.prepare("SELECT id, files, read_at, held FROM messages WHERE files IS NOT NULL")?;
        let rows: Vec<(String, String, Option<i64>, bool)> =
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<rusqlite::Result<_>>()?;
        let mut keep = HashSet::new();
        for (id, col, read_at, held) in rows {
            let kept: Option<Kept> = serde_json::from_str(&col).ok();
            let came =
                kept.as_ref().and_then(|k| ulid::Ulid::from_string(&k.dir).ok()).map(|u| u.timestamp_ms() as i64);
            let fresh = came.is_some_and(|t| t > now - files::KEEP_UNREAD_MS);
            if held {
                if !fresh {
                    self.drop_unread(&id, "expired")?;
                }
            } else if (read_at.is_none() && fresh) || read_at.is_some_and(|t| t > now - files::KEEP_MS) {
                if let Some(k) = &kept {
                    keep.insert(k.dir.clone());
                }
            } else {
                self.db.execute("UPDATE messages SET files = NULL WHERE id = ?1", [&id])?;
            }
        }
        if let Ok(entries) = std::fs::read_dir(&self.inbox) {
            for e in entries.flatten() {
                if !keep.contains(&*e.file_name().to_string_lossy()) {
                    let _ = std::fs::remove_dir_all(e.path());
                }
            }
        }
        Ok(())
    }

    /// Approves (`release`) or drops held messages; returns the ones affected, and those whose files couldn't be saved
    /// (which stay held). An approved message's files are saved now; a dropped one's never are.
    pub fn settle_held(&self, ids: &[String], release: bool) -> Result<(Vec<Message>, Vec<Unsaved>)> {
        let held = self.held()?;
        let picked: Vec<Message> = held.into_iter().filter(|m| ids.is_empty() || ids.contains(&m.id)).collect();
        let (mut done, mut unsaved) = (Vec::new(), Vec::new());
        for mut m in picked {
            if !release {
                self.drop_unread(&m.id, "rejected")?;
                done.push(m);
                continue;
            }
            let mut col = None;
            let mut saved = None;
            if let Some(mut k) = self.kept(&m.id)? {
                let Some(dir) = self.dir(&k) else {
                    unsaved.push((m.id.clone(), "its files' record is damaged".into()));
                    continue;
                };
                match files::save(&dir, &k.files) {
                    Ok(names) => k.files = names,
                    Err(e) => {
                        unsaved.push((m.id.clone(), format!("{e:#}")));
                        continue;
                    }
                }
                m.files = files::located(&dir, &k.files);
                col = Some(serde_json::to_string(&k).expect("files serialize"));
                saved = Some(dir);
            }
            if let Err(e) =
                self.db.execute("UPDATE messages SET held = 0, files = ?2 WHERE id = ?1", params![m.id, col])
            {
                if let Some(dir) = saved {
                    let _ = std::fs::remove_dir_all(dir);
                }
                return Err(e.into());
            }
            let _ = self.count(crate::util::now_ms(), Counted::Received);
            let _ = self.observe_reply(&m);
            done.push(m);
        }
        Ok((done, unsaved))
    }

    pub fn outbox_add(&self, target: &str, m: &Message) -> Result<()> {
        self.db.execute(
            "INSERT OR REPLACE INTO outbox (id, target, msg, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![m.id, target, serde_json::to_string(m)?, m.sent_at],
        )?;
        Ok(())
    }

    /// Undelivered messages for a machine; older than a week are given up on.
    pub fn outbox_for(&self, target: &str, now: i64) -> Result<(Vec<Message>, usize)> {
        let mut st = self.db.prepare_cached("SELECT id FROM outbox WHERE created_at < ?1")?;
        let old: Vec<String> = st.query_map([now - 7 * 86_400_000], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        let mut expired = 0;
        for id in old {
            expired += usize::from(self.drop_outbox(&id, None)?);
        }
        let mut st = self.db.prepare_cached("SELECT msg FROM outbox WHERE target = ?1 ORDER BY created_at, rowid")?;
        let rows = st.query_map([target], |r| r.get::<_, String>(0))?;
        Ok((rows.filter_map(|r| r.ok().and_then(|s| serde_json::from_str(&s).ok())).collect(), expired))
    }

    /// The receiving machine answered for a message: it leaves the outbox, and counts as sent if that machine took it
    /// (once, however many answers come). One it refused that its sender didn't see refused (`seen`, in the reply to
    /// sending it) gets a notice. True if it was still in the outbox.
    pub fn outbox_done(&self, id: &str, taken: bool, seen: bool) -> Result<bool> {
        if !taken && !seen {
            return self.drop_outbox(id, Some("rejected"));
        }
        let transaction = self.db.unchecked_transaction()?;
        let removed = self.db.execute("DELETE FROM outbox WHERE id = ?1", [id])? == 1;
        if removed {
            self.db.execute(
                "UPDATE sent_receipts SET stage=?3,updated_at=?2 WHERE id=?1 AND stage NOT IN ('handed_to_agent','replied')",
                params![id, crate::util::now_ms(), if taken {"accepted"} else {"rejected"}],
            )?;
            if taken {
                let _ = self.count(crate::util::now_ms(), Counted::Sent);
            }
        }
        transaction.commit()?;
        Ok(removed)
    }

    /// Deletes a message that was never read, and tells its sender: both or neither. A receipt for it, kept here if it
    /// was sent here, says `stage`.
    fn drop_unread(&self, id: &str, stage: &str) -> Result<()> {
        let tx = self.db.unchecked_transaction()?;
        let told = self.drop_unread_in(id, stage)?;
        tx.commit()?;
        self.notices.borrow_mut().extend(told);
        Ok(())
    }

    /// The same, in the caller's transaction: returns the notice stored here, to take once that commits.
    fn drop_unread_in(&self, id: &str, stage: &str) -> Result<Option<Message>> {
        let source: Option<Option<String>> =
            self.db.query_row("SELECT source FROM messages WHERE id = ?1", [id], |r| r.get(0)).optional()?;
        let mut told = None;
        // (Mail from before notices whose machine wasn't found, see `sourceless`, goes without one.)
        if let (Some(Some(source)), Some(m)) = (source, self.message(id)?) {
            // Mail here is for this machine's person.
            let person = m.to.split_once('/').map_or(m.to.as_str(), |(p, _)| p);
            told = self.tell(&m, Some(source.as_str()).filter(|s| !s.is_empty()), person)?;
        }
        self.discard_receipt(id, stage, util::now_ms())?;
        self.db.execute("DELETE FROM messages WHERE id = ?1", [id])?;
        Ok(told)
    }

    /// Gives up on a message in the outbox, and tells its sender here: both or neither. Its receipt says `stage`, if
    /// given. True if it was still there.
    fn drop_outbox(&self, id: &str, stage: Option<&str>) -> Result<bool> {
        let tx = self.db.unchecked_transaction()?;
        let msg: Option<String> =
            self.db.query_row("SELECT msg FROM outbox WHERE id = ?1", [id], |r| r.get(0)).optional()?;
        let mut told = None;
        if let Some(m) = msg.and_then(|s| serde_json::from_str::<Message>(&s).ok()) {
            let person = m.from.split_once('/').map_or(m.from.as_str(), |(p, _)| p);
            told = self.tell(&m, None, person)?;
        }
        let removed = self.db.execute("DELETE FROM outbox WHERE id = ?1", [id])? == 1;
        if removed && let Some(stage) = stage {
            self.discard_receipt(id, stage, util::now_ms())?;
        }
        tx.commit()?;
        self.notices.borrow_mut().extend(told);
        Ok(removed)
    }

    /// Keeps the notice that `m` was dropped on `person`'s machine: in the outbox for the teammate's machine it came
    /// from (`source`), or, sent here, in its sender's inbox, if that is a session remembered here. Returns that one.
    fn tell(&self, m: &Message, source: Option<&str>, person: &str) -> Result<Option<Message>> {
        let Some(mut n) = notice(m, &format!("{person}/terminal"), util::now_ms()) else { return Ok(None) };
        if let Some(machine) = source {
            // That machine knows its terminal without its own name on it.
            n.to = render::without_machine(&n.to).to_string();
            self.outbox_add(machine, &n)?;
            return Ok(None);
        }
        // A sender here has an address without a machine on it.
        let here = n.to.split_once('/').filter(|(p, name)| *p == person && !name.contains('@')).map(|(_, name)| name);
        if let Some(name) = here
            && self.session_by_name(name)?.is_some()
            && self.insert(&mut n, false)?
        {
            return Ok(Some(n));
        }
        Ok(None)
    }

    /// Unread and held mail kept from before notices, which doesn't say which machine it came from: ids and senders.
    pub fn sourceless(&self) -> Result<Vec<(String, String)>> {
        let mut st =
            self.db.prepare_cached("SELECT id, sender FROM messages WHERE source IS NULL AND read_at IS NULL")?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Where a message came from, found later (see `insert_from`).
    pub fn set_source(&self, id: &str, source: &str) -> Result<()> {
        self.db.execute("UPDATE messages SET source = ?2 WHERE id = ?1", params![id, source])?;
        Ok(())
    }

    /// The notices stored for senders here since the last call.
    pub fn take_notices(&self) -> Vec<Message> {
        self.notices.take()
    }

    /// Adds one to today's count (by UTC day). Only numbers are kept, and only for the last few days.
    pub fn count(&self, now: i64, what: Counted) -> Result<()> {
        let col = match what {
            Counted::Sent => "sent",
            Counted::Received => "received",
        };
        self.db.execute(
            &format!(
                "INSERT INTO counts (day, {col}) VALUES (date(?1 / 1000, 'unixepoch'), 1)
                 ON CONFLICT (day) DO UPDATE SET {col} = {col} + 1"
            ),
            [now],
        )?;
        self.db.execute(
            "DELETE FROM counts WHERE rowid NOT IN (SELECT rowid FROM counts ORDER BY rowid DESC LIMIT ?1)",
            [COUNTS_KEPT],
        )?;
        Ok(())
    }

    /// The days shown that had messages, oldest first.
    pub fn counts(&self, now: i64) -> Result<Vec<DayCount>> {
        let mut st = self.db.prepare_cached(
            "SELECT day, sent, received FROM counts
             WHERE day BETWEEN date(?1 / 1000, 'unixepoch', ?2) AND date(?1 / 1000, 'unixepoch', ?3) ORDER BY day",
        )?;
        // SQLite integers are i64; a count is never negative.
        let count = |r: &rusqlite::Row, i: usize| r.get::<_, i64>(i).map(|n| n.max(0) as u64);
        let rows = st.query_map(params![now, COUNTS_FROM, COUNTS_TO], |r| {
            Ok(DayCount { day: r.get(0)?, sent: count(r, 1)?, received: count(r, 2)? })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn get(&self, key: &str) -> Result<Option<String>> {
        Ok(self.db.query_row("SELECT value FROM kv WHERE key = ?1", [key], |r| r.get(0)).optional()?)
    }

    pub fn set(&self, key: &str, value: &str) -> Result<()> {
        self.db.execute("INSERT OR REPLACE INTO kv (key, value) VALUES (?1, ?2)", params![key, value])?;
        Ok(())
    }

    pub fn prefixed(&self, prefix: &str) -> Result<Vec<(String, String)>> {
        let mut st = self.db.prepare_cached("SELECT key, value FROM kv WHERE key >= ?1 AND key < ?1 || x'ff'")?;
        let rows = st.query_map([prefix], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Writes everything in the log into the database and empties the log, so what was deleted (with `secure_delete`,
    /// overwritten) isn't still in it.
    pub fn scrub(&self) -> Result<()> {
        self.db.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
        Ok(())
    }

    pub fn delete_prefix(&self, prefix: &str) -> Result<()> {
        self.db.execute("DELETE FROM kv WHERE key >= ?1 AND key < ?1 || x'ff'", [prefix])?;
        Ok(())
    }

    pub fn mark_read(&self, ids: &[String], now: i64) -> Result<Vec<String>> {
        let mut st = self.db.prepare_cached("UPDATE messages SET read_at = ?2 WHERE id = ?1 AND read_at IS NULL")?;
        let mut changed = Vec::new();
        for id in ids {
            if st.execute(params![id, now])? > 0 {
                changed.push(id.clone());
            }
        }
        Ok(changed)
    }

    /// A renamed session keeps its unread mail.
    pub fn rename(&self, old_name: &str, new_name: &str, old_addr: &str, new_addr: &str) -> Result<()> {
        self.db.execute(
            "UPDATE messages SET recipient = ?2 WHERE recipient = ?1 AND read_at IS NULL",
            params![old_addr, new_addr],
        )?;
        self.db.execute("DELETE FROM sessions WHERE name = ?1", [new_name])?;
        self.db.execute("UPDATE sessions SET name = ?2 WHERE name = ?1", params![old_name, new_name])?;
        Ok(())
    }

    /// Remembers a session under its name. A session has one name: if it had another before (its own was taken by a
    /// live session meanwhile), that one goes, and its unread mail comes along to the new one.
    pub fn upsert_session(&self, s: &Remembered, person: &str) -> Result<()> {
        let before: Vec<String> = {
            let mut st = self.db.prepare_cached("SELECT name FROM sessions WHERE key = ?1 AND name != ?2")?;
            st.query_map(params![s.key, s.name], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        for old in before {
            self.rename(&old, &s.name, &format!("{person}/{old}"), &format!("{person}/{}", s.name))?;
        }
        // Once reachable, a session stays so for as long as it is remembered.
        self.db.execute(
            &format!(
                "INSERT INTO sessions ({SESSION_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 ON CONFLICT (name) DO UPDATE SET key = ?2, agent = ?3, cwd = ?4, repo = ?5, branch = ?6,
                 doing = ?7, tier = ?8, last_seen = ?9,
                 attached = CASE WHEN key = ?2 THEN max(attached, ?10) ELSE ?10 END,
                 mailbox = CASE WHEN key = ?2 THEN max(mailbox, ?11) ELSE ?11 END,
                 native_app = CASE WHEN key = ?2 THEN native_app ELSE 0 END,
                 native_hook_at = CASE WHEN key = ?2 THEN native_hook_at ELSE NULL END,
                 native_dirs = CASE WHEN key = ?2 THEN native_dirs ELSE '[]' END"
            ),
            params![
                s.name,
                s.key,
                s.agent,
                s.cwd,
                s.repo,
                s.branch,
                s.doing,
                s.tier.as_str(),
                s.last_seen,
                s.attached,
                s.mailbox
            ],
        )?;
        Ok(())
    }

    pub fn touch_session(&self, name: &str, now: i64) -> Result<()> {
        self.db.execute("UPDATE sessions SET last_seen = ?2 WHERE name = ?1", params![name, now])?;
        Ok(())
    }

    /// Forgets the sessions not seen since `before`, other than `live` ones and the person's own mailboxes (`mailbox`), and
    /// deletes the mail they never read, so a session that takes the name later doesn't get it (see `forget_session`).
    pub fn forget_sessions(&self, before: i64, live: &HashSet<&str>, person: &str) -> Result<()> {
        let mut st = self
            .db
            .prepare_cached(&format!("SELECT {SESSION_COLS} FROM sessions WHERE last_seen < ?1 AND mailbox = 0"))?;
        let gone: Vec<Remembered> = st.query_map([before], remembered)?.collect::<rusqlite::Result<_>>()?;
        for r in gone.iter().filter(|r| !live.contains(r.name.as_str())) {
            self.forget_session(r, person)?;
        }
        Ok(())
    }

    /// Forgets one session, with the mail it never read and that mail's files. Mail held for the person goes too when
    /// the name was kept for the session: it was that conversation's, never the next one's. Otherwise it stays the
    /// person's to settle, for whoever has the name.
    pub fn forget_session(&self, r: &Remembered, person: &str) -> Result<()> {
        let transaction = self.db.unchecked_transaction()?;
        let unread = if r.kept() {
            "FROM messages WHERE recipient = ?1 AND read_at IS NULL"
        } else {
            "FROM messages WHERE recipient = ?1 AND read_at IS NULL AND held = 0"
        };
        let name = r.name.as_str();
        let to = format!("{person}/{name}");
        let mut st = self.db.prepare_cached(&format!("SELECT id, files {unread}"))?;
        let rows: Vec<(String, Option<String>)> =
            st.query_map([&to], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        let mut told = Vec::new();
        for (id, _) in &rows {
            told.extend(self.drop_unread_in(id, "expired")?);
        }
        self.db.execute("DELETE FROM sessions WHERE name = ?1", [name])?;
        transaction.commit()?;
        self.notices.borrow_mut().extend(told);
        for kept in rows.iter().filter_map(|(_, c)| serde_json::from_str::<Kept>(c.as_deref()?).ok()) {
            if let Some(dir) = self.dir(&kept) {
                let _ = std::fs::remove_dir_all(dir);
            }
        }
        Ok(())
    }

    pub fn session_by_key(&self, key: &str) -> Result<Option<Remembered>> {
        let sql = format!("SELECT {SESSION_COLS} FROM sessions WHERE key = ?1 ORDER BY last_seen DESC LIMIT 1");
        Ok(self.db.query_row(&sql, [key], remembered).optional()?)
    }

    pub fn session_by_name(&self, name: &str) -> Result<Option<Remembered>> {
        let sql = format!("SELECT {SESSION_COLS} FROM sessions WHERE name = ?1");
        Ok(self.db.query_row(&sql, [name], remembered).optional()?)
    }

    pub fn sessions(&self) -> Result<Vec<Remembered>> {
        let mut st = self.db.prepare_cached(&format!("SELECT {SESSION_COLS} FROM sessions ORDER BY name"))?;
        let rows = st.query_map([], remembered)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn remember_native_app(&self, key: &str, hook_at: Option<i64>, dirs: &[String]) -> Result<()> {
        self.db.execute(
            "UPDATE sessions SET native_app=?2,native_hook_at=?3,native_dirs=?4 WHERE key=?1 AND agent='antigravity'",
            params![key, hook_at.is_some(), hook_at, serde_json::to_string(dirs)?],
        )?;
        Ok(())
    }

    pub fn native_app_roots(&self, key: &str) -> Result<Option<Vec<String>>> {
        let roots: Option<String> = self
            .db
            .query_row(
                "SELECT native_dirs FROM sessions WHERE key=?1 AND agent='antigravity' AND native_app=1",
                [key],
                |row| row.get(0),
            )
            .optional()?;
        Ok(roots.map(|value| serde_json::from_str::<Vec<String>>(&value)).transpose()?.filter(|dirs| !dirs.is_empty()))
    }

    pub fn native_app_sessions(&self, since: i64) -> Result<Vec<(Remembered, i64)>> {
        let mut statement=self.db.prepare(&format!("SELECT {SESSION_COLS},native_hook_at FROM sessions WHERE native_app=1 AND agent='antigravity' AND last_seen>=?1 AND native_hook_at IS NOT NULL"))?;
        Ok(statement
            .query_map([since], |row| Ok((remembered(row)?, row.get(11)?)))?
            .collect::<rusqlite::Result<_>>()?)
    }
}

impl Store {
    fn row(&self, r: &Row) -> rusqlite::Result<Message> {
        let kept: Option<Kept> = r.get::<_, Option<String>>(11)?.and_then(|f| serde_json::from_str(&f).ok());
        // A held message's files aren't saved yet: their text is here. Once released, they are in their folder.
        let files = match kept {
            Some(k) if r.get::<_, bool>(12)? => k.files,
            Some(k) => self.dir(&k).map(|dir| files::located(&dir, &k.files)).unwrap_or_default(),
            None => Vec::new(),
        };
        message(r).map(|m| Message { files, ..m })
    }
}

/// The notice to `m`'s sender, from `from`, that it was dropped unread: who it was for, when it was sent, and its first
/// line, to tell which it was. An ordinary message, which an engine from before notices shows like any other. None for
/// a notice: a dropped one never makes another.
pub fn notice(m: &Message, from: &str, now: i64) -> Option<Message> {
    // Only to stop a loop of notices. Anyone can put this in a message (a teammate's machine passes its agent on as
    // it says), so nothing may be granted or trusted on it.
    if m.from_agent.as_deref() == Some(NOTICE_AGENT) {
        return None;
    }
    let first = util::one_line(m.body.lines().next().unwrap_or_default(), 100);
    Some(Message {
        id: ulid::Ulid::generate().to_string(),
        from: from.to_string(),
        from_agent: Some(NOTICE_AGENT.into()),
        from_repo: None,
        // As it shows here, a teammate's terminal with its machine, until it is known where the notice goes.
        to: m.from.clone(),
        via: None,
        reply_to: Some(m.id.clone()),
        body: format!(
            "Not delivered: your message to {} (sent {}) was dropped unread: \"{first}\"",
            render::without_machine(&m.to),
            util::utc(m.sent_at)
        ),
        wake: m.wake,
        notify: false,
        sent_at: now,
        approval: None,
        files: Vec::new(),
    })
}

fn message(r: &Row) -> rusqlite::Result<Message> {
    Ok(Message {
        id: r.get(0)?,
        from: r.get(1)?,
        from_agent: r.get(2)?,
        from_repo: r.get(3)?,
        to: r.get(4)?,
        via: r.get(5)?,
        reply_to: r.get(6)?,
        body: r.get(7)?,
        wake: r.get(8)?,
        notify: false,
        sent_at: r.get(9)?,
        approval: r.get::<_, Option<String>>(10)?.and_then(|a| serde_json::from_str(&a).ok()),
        files: Vec::new(),
    })
}

fn remembered(r: &Row) -> rusqlite::Result<Remembered> {
    Ok(Remembered {
        name: r.get(0)?,
        key: r.get(1)?,
        agent: r.get(2)?,
        cwd: r.get(3)?,
        repo: r.get(4)?,
        branch: r.get(5)?,
        doing: r.get(6)?,
        tier: Tier::parse(&r.get::<_, String>(7)?),
        last_seen: r.get(8)?,
        attached: r.get(9)?,
        mailbox: r.get(10)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util;
    use std::collections::HashMap;

    #[test]
    #[cfg(unix)]
    fn the_database_is_this_users_and_keeps_nothing_it_deleted() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("reason-network-store-{}", ulid::Ulid::generate()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("network.db");
        // A database made before, with the default mode.
        drop(Store::open(&db, temp_inbox()).unwrap());
        util::chmod(&db, 0o644).unwrap();
        let store = Store::open(&db, temp_inbox()).unwrap();
        let secret = "5ec7e75ec7e75ec7e75ec7e75ec7e75e";
        store.set("team", &format!(r#"{{"link":{{"body":{{"secret":"{secret}"}}}}}}"#)).unwrap();
        for f in ["network.db", "network.db-wal", "network.db-shm"] {
            let mode = std::fs::metadata(dir.join(f)).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{f}");
        }
        store.delete_prefix("team").unwrap();
        store.scrub().unwrap();
        for f in ["network.db", "network.db-wal"] {
            let bytes = std::fs::read(dir.join(f)).unwrap_or_default();
            assert!(!bytes.windows(secret.len()).any(|w| w == secret.as_bytes()), "{f} still holds it");
        }
        drop(store);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn messages_are_counted_by_day_for_a_week_and_kept_across_restarts() {
        let dir = std::env::temp_dir().join(format!("reason-network-counts-{}", ulid::Ulid::generate()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("network.db");
        let day = 86_400_000;
        let oct20 = 1_792_454_400_000; // 2026-10-20T00:00:00Z
        let store = Store::open(&db, temp_inbox()).unwrap();
        assert_eq!(store.counts(oct20).unwrap(), []);
        for (at, what) in [
            (oct20 - 7 * day, Counted::Sent),
            (oct20 - 6 * day, Counted::Received),
            (oct20 - 1, Counted::Sent),
            (oct20, Counted::Sent),
            (oct20 + day - 1, Counted::Sent),
            (oct20 + 5, Counted::Received),
        ] {
            store.count(at, what).unwrap();
        }
        drop(store);
        let store = Store::open(&db, temp_inbox()).unwrap();
        let count = |day: &str, sent, received| DayCount { day: day.into(), sent, received };
        assert_eq!(
            store.counts(oct20 + 1).unwrap(),
            [count("2026-10-14", 0, 1), count("2026-10-19", 1, 0), count("2026-10-20", 2, 1)]
        );
        // The next day, the oldest goes.
        store.count(oct20 + day, Counted::Received).unwrap();
        assert_eq!(
            store.counts(oct20 + day).unwrap(),
            [count("2026-10-19", 1, 0), count("2026-10-20", 2, 1), count("2026-10-21", 0, 1)]
        );
        drop(store);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn counts_are_shown_to_tomorrow_and_a_clock_that_jumps_loses_no_real_days() {
        let store = Store::open(Path::new(":memory:"), temp_inbox()).unwrap();
        let day = 86_400_000;
        let oct20 = 1_792_454_400_000; // 2026-10-20T00:00:00Z
        let count = |day: &str, sent| DayCount { day: day.into(), sent, received: 0 };
        for at in [oct20 - 6 * day, oct20, oct20 + day, oct20 + 2 * day] {
            store.count(at, Counted::Sent).unwrap();
        }
        let week = [count("2026-10-14", 1), count("2026-10-20", 1), count("2026-10-21", 1)];
        assert_eq!(store.counts(oct20).unwrap(), week, "not the day after tomorrow");
        // The clock is a year ahead for a while, then right again.
        store.count(oct20 + 365 * day, Counted::Sent).unwrap();
        assert_eq!(store.counts(oct20).unwrap(), week);
        assert_eq!(store.counts(oct20 + 365 * day).unwrap(), [count("2027-10-20", 1)]);
        // Only the last days begun are kept.
        for i in 3..20 {
            store.count(oct20 + i * day, Counted::Sent).unwrap();
        }
        let kept: i64 = store.db.query_row("SELECT COUNT(*) FROM counts", [], |r| r.get(0)).unwrap();
        assert_eq!(kept, COUNTS_KEPT);
        assert_eq!(store.counts(oct20 + 19 * day).unwrap().len(), 7);
    }

    #[test]
    fn a_message_counts_as_sent_once_taken_and_as_received_once_in_a_mailbox() {
        let inbox = temp_inbox();
        let store = Store::open(Path::new(":memory:"), inbox.clone()).unwrap();
        let totals = |store: &Store| {
            let days = store.counts(util::now_ms()).unwrap();
            (days.iter().map(|d| d.sent).sum::<u64>(), days.iter().map(|d| d.received).sum::<u64>())
        };
        // Sent: once the other machine takes it (answered twice, after a lost answer), not when it refuses it or it
        // expires in the outbox.
        for id in ["01TAKEN", "01REFUSED", "01EXPIRED"] {
            store.outbox_add("peer", &with_files(id, "sven/api")).unwrap();
        }
        store.outbox_done("01TAKEN", true, true).unwrap();
        store.outbox_done("01TAKEN", true, false).unwrap();
        store.outbox_done("01REFUSED", false, true).unwrap();
        assert!(store.outbox_for("peer", util::now_ms() + 8 * 86_400_000).unwrap().0.is_empty());
        assert_eq!(totals(&store), (1, 0));
        // Received: held, it counts once approved. Dropped, then sent again after a lost answer, it counts once.
        store.insert(&mut with_files("01HELD", "lars/api"), true).unwrap();
        store.settle_held(&["01HELD".into()], false).unwrap();
        store.insert(&mut with_files("01HELD", "lars/api"), true).unwrap();
        assert_eq!(totals(&store), (1, 0));
        store.settle_held(&["01HELD".into()], true).unwrap();
        assert!(!store.insert(&mut with_files("01HELD", "lars/api"), true).unwrap());
        assert_eq!(totals(&store), (1, 1));
        // Storing one that isn't held counts nothing: the caller counts it if it is a message.
        store.insert(&mut with_files("01NEW", "lars/api"), false).unwrap();
        assert_eq!(totals(&store), (1, 1));
        std::fs::remove_dir_all(&inbox).unwrap();
    }

    #[test]
    fn a_session_gone_a_day_is_forgotten_with_the_mail_it_never_read() {
        // (Not one that is live, nor the person's own mailbox.)
        let store = Store::open(Path::new(":memory:"), temp_inbox()).unwrap();
        let day = 86_400_000;
        let session = |name: &str, agent: &str, last_seen: i64| Remembered {
            mailbox: agent == "cli",
            name: name.into(),
            key: name.into(),
            agent: agent.into(),
            cwd: "~".into(),
            repo: None,
            branch: None,
            doing: None,
            tier: Tier::Push,
            last_seen,
            attached: true,
        };
        for s in [
            session("gone", "claude-code", 0),
            session("recent", "claude-code", 2 * day),
            session("still-up", "claude-code", 0),
            session("terminal", "cli", 0),
            Remembered { key: "proc:7".into(), ..session("no-id", "zed", 0) },
        ] {
            store.upsert_session(&s, "lars").unwrap();
        }
        let to = |id: &str, name: &str| Message {
            id: id.into(),
            from: "sven/claude".into(),
            from_agent: None,
            from_repo: None,
            to: format!("lars/{name}"),
            via: None,
            reply_to: None,
            body: "hi".into(),
            wake: false,
            notify: false,
            sent_at: 0,
            approval: None,
            files: Vec::new(),
        };
        store.insert(&mut to("1", "gone"), false).unwrap();
        store.insert(&mut to("2", "recent"), false).unwrap();
        store.insert(&mut to("3", "terminal"), false).unwrap();
        store.insert(&mut to("4", "gone"), true).unwrap();
        store.insert(&mut to("5", "no-id"), true).unwrap();
        store.forget_sessions(day + 1, &HashSet::from(["still-up"]), "lars").unwrap();
        let names: Vec<String> = store.sessions().unwrap().into_iter().map(|s| s.name).collect();
        assert_eq!(names, ["recent", "still-up", "terminal"]);
        assert_eq!(store.unread_count("lars/gone").unwrap(), 0);
        assert_eq!(store.unread_count("lars/recent").unwrap(), 1);
        assert_eq!(store.unread_count("lars/terminal").unwrap(), 1);
        // Mail held for a conversation goes with it; for a session without an id it stays the person's to settle.
        assert_eq!(store.held().unwrap().iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["5"]);
    }

    #[test]
    fn a_session_has_one_name_and_one_an_older_engine_left_under_two_is_folded_into_its_latest() {
        let store = Store::open(Path::new(":memory:"), temp_inbox()).unwrap();
        let session = |name: &str, last_seen: i64| Remembered {
            name: name.into(),
            key: "claude-code:a".into(),
            agent: "claude-code".into(),
            cwd: "~".into(),
            repo: None,
            branch: None,
            doing: None,
            tier: Tier::Push,
            last_seen,
            attached: true,
            mailbox: false,
        };
        // As older engines could: the same session under two names, mail waiting under each.
        for (name, seen) in [("api-claude", 1), ("api-claude-2", 2)] {
            let s = session(name, seen);
            store
                .db
                .execute(
                    &format!(
                        "INSERT INTO sessions ({SESSION_COLS}) VALUES (?1, ?2, ?3, ?4, NULL, NULL, NULL, ?5, ?6, 1, 0)"
                    ),
                    params![s.name, s.key, s.agent, s.cwd, s.tier.as_str(), s.last_seen],
                )
                .unwrap();
        }
        for (id, name) in [("1", "api-claude"), ("2", "api-claude-2")] {
            store.insert(&mut message(id, &format!("lars/{name}")), false).unwrap();
        }
        assert_eq!(store.session_by_key("claude-code:a").unwrap().unwrap().name, "api-claude-2");
        store.upsert_session(&session("api-claude-2", 3), "lars").unwrap();
        let names: Vec<String> = store.sessions().unwrap().into_iter().map(|s| s.name).collect();
        assert_eq!(names, ["api-claude-2"]);
        assert_eq!(store.unread_count("lars/api-claude-2").unwrap(), 2);
        // Under a new name, the old one goes, and its mail comes along.
        store.upsert_session(&session("auth", 4), "lars").unwrap();
        let names: Vec<String> = store.sessions().unwrap().into_iter().map(|s| s.name).collect();
        assert_eq!(names, ["auth"]);
        assert_eq!(store.unread_count("lars/auth").unwrap(), 2);
    }

    fn message(id: &str, to: &str) -> Message {
        Message {
            id: id.into(),
            from: "sven/claude".into(),
            from_agent: None,
            from_repo: None,
            to: to.into(),
            via: None,
            reply_to: None,
            body: "hi".into(),
            wake: false,
            notify: false,
            sent_at: 0,
            approval: None,
            files: Vec::new(),
        }
    }

    #[test]
    fn receipts_track_remote_refusal_and_only_the_approved_matching_terminal_reply() {
        let store = Store::open(Path::new(":memory:"), temp_inbox()).unwrap();
        let mut outgoing = message("refused", "bob/terminal");
        outgoing.sent_at = crate::util::now_ms();
        store.record_sent("sender", &outgoing, "bob/terminal@laptop", "queued").unwrap();
        store.outbox_add("peer", &outgoing).unwrap();
        store.outbox_done("refused", false, true).unwrap();
        assert_eq!(store.delivery_status("sender", &["refused".into()]).unwrap()[0]["stage"], "rejected");
        outgoing.id = "accepted".into();
        store.record_sent("sender", &outgoing, "bob/terminal@laptop", "queued").unwrap();
        store.outbox_add("peer", &outgoing).unwrap();
        store.outbox_done("accepted", true, true).unwrap();
        let stage = || store.delivery_status("sender", &["accepted".into()]).unwrap()[0]["stage"].clone();
        let mut reply = message("wrong-machine", &outgoing.from);
        reply.from = "bob/terminal@desktop".into();
        reply.reply_to = Some(outgoing.id.clone());
        store.insert(&mut reply, false).unwrap();
        assert_eq!(stage(), "accepted");
        reply.id = "held-reply".into();
        reply.from = "bob/terminal@laptop".into();
        store.insert(&mut reply, true).unwrap();
        assert_eq!(stage(), "accepted");
        store.settle_held(&[reply.id], true).unwrap();
        assert_eq!(stage(), "replied");
        assert_eq!(store.delivery_status("other-session", &["accepted".into()]).unwrap()[0]["stage"], "unknown");
    }

    #[test]
    fn a_notice_for_a_message_is_not_taken_for_its_reply() {
        let store = Store::open(Path::new(":memory:"), temp_inbox()).unwrap();
        let mut outgoing = message("01SENT", "bob/terminal");
        outgoing.sent_at = crate::util::now_ms();
        store.record_sent("sender", &outgoing, "bob/terminal@laptop", "queued").unwrap();
        // Held on bob's machine and turned down there: the notice comes back from that machine's terminal.
        let mut n = notice(&outgoing, "bob/terminal", crate::util::now_ms()).unwrap();
        n.from = "bob/terminal@laptop".into();
        store.insert(&mut n, false).unwrap();
        assert_eq!(store.delivery_status("sender", &["01SENT".into()]).unwrap()[0]["stage"], "queued");
    }

    #[test]
    fn an_expired_idempotency_key_gets_a_new_generation() {
        let store = Store::open(Path::new(":memory:"), temp_inbox()).unwrap();
        let now = crate::util::now_ms();
        let (first, receipt, fresh) = store.begin_send("key", "original", now).unwrap();
        assert!(!first.is_empty());
        assert!(fresh);
        assert!(receipt.is_none());
        let (pending, receipt, fresh) = store.begin_send("key", "original", now + 1000).unwrap();
        assert_eq!(pending, first);
        assert!(!fresh);
        assert!(receipt.is_none());
        let saved = crate::proto::SendResult { ids: vec!["message-id".into()], recipients: vec![] };
        store.finish_send("key", &saved).unwrap();
        let (cached, receipt, fresh) = store.begin_send("key", "original", now + 2000).unwrap();
        assert_eq!(cached, first);
        assert!(!fresh);
        assert_eq!(serde_json::to_value(receipt.unwrap()).unwrap(), serde_json::to_value(saved).unwrap());
        assert!(store.begin_send("key", "different", now + 1000).is_err());
        let (next, receipt, fresh) = store.begin_send("key", "different", now + 8 * 86_400_000).unwrap();
        assert_ne!(next, first);
        assert!(fresh);
        assert!(receipt.is_none());
    }

    #[test]
    fn discarded_queues_and_retention_do_not_remain_pending() {
        let store = Store::open(Path::new(":memory:"), temp_inbox()).unwrap();
        let now = crate::util::now_ms();
        let mut pending = message("pending", "sven/recipient");
        pending.sent_at = now;
        store.record_sent("sender", &pending, "sven/recipient", "queued").unwrap();
        store.insert(&mut pending, false).unwrap();
        let recipient = Remembered {
            name: "recipient".into(),
            key: "agent:recipient".into(),
            agent: "agent".into(),
            cwd: "/work".into(),
            repo: None,
            branch: None,
            doing: None,
            tier: Tier::Pull,
            last_seen: now - 2 * 86_400_000,
            attached: true,
            mailbox: false,
        };
        store.upsert_session(&recipient, "sven").unwrap();
        store.forget_session(&recipient, "sven").unwrap();
        assert!(store.message("pending").unwrap().is_none());
        assert_eq!(store.delivery_status("sender", &["pending".into()]).unwrap()[0]["stage"], "expired");
        pending.id = "old".into();
        pending.sent_at = now - 8 * 86_400_000;
        store.record_sent("sender", &pending, "sven/recipient", "queued").unwrap();
        assert_eq!(store.delivery_status("sender", &["old".into()]).unwrap()[0]["stage"], "unknown");
        assert!(store.db.prepare("SELECT 1 FROM sent_receipts WHERE id='old'").unwrap().exists([]).unwrap());
        store.begin_send("old-key", "body", now - 8 * 86_400_000).unwrap();
        store.sweep_files(now).unwrap();
        assert!(!store.db.prepare("SELECT 1 FROM sent_receipts WHERE id='old'").unwrap().exists([]).unwrap());
        assert!(!store.db.prepare("SELECT 1 FROM send_keys WHERE key='old-key'").unwrap().exists([]).unwrap());
    }

    #[test]
    fn a_notice_says_who_a_dropped_message_was_for_when_it_was_sent_and_its_first_line() {
        let m = Message {
            from: "sven/terminal@desktop".into(),
            body: "  please   rebase\non main".into(),
            sent_at: 1_793_243_096_000,
            wake: true,
            ..message("01DROPPED", "lars/api")
        };
        let n = notice(&m, "lars/terminal", 5).unwrap();
        assert_eq!(
            n.body,
            "Not delivered: your message to lars/api (sent 2026-10-29 03:04 UTC) was dropped unread: \"please rebase\""
        );
        // To the sender as it shows here, from Reason Network, as an answer to the message it names.
        assert_eq!((n.from.as_str(), n.to.as_str()), ("lars/terminal", "sven/terminal@desktop"));
        assert_eq!((n.from_agent.as_deref(), n.reply_to.as_deref()), (Some(NOTICE_AGENT), Some("01DROPPED")));
        assert!(n.wake && !n.notify && n.files.is_empty() && n.approval.is_none() && n.sent_at == 5);
        assert!(ulid::Ulid::from_string(&n.id).is_ok());
        // An FYI makes an FYI notice; a notice makes none.
        assert!(!notice(&Message { wake: false, ..m }, "lars/terminal", 5).unwrap().wake);
        assert!(notice(&n, "sven/terminal", 6).is_none());
    }

    #[test]
    fn mail_dropped_unread_tells_its_sender_once_wherever_it_is() {
        let store = Store::open(Path::new(":memory:"), temp_inbox()).unwrap();
        let session = |name: &str| Remembered {
            name: name.into(),
            key: format!("codex:{name}"),
            agent: "codex".into(),
            cwd: "~".into(),
            repo: None,
            branch: None,
            doing: None,
            tier: Tier::Pull,
            last_seen: 0,
            attached: true,
            mailbox: false,
        };
        for name in ["gone", "api"] {
            store.upsert_session(&session(name), "lars").unwrap();
        }
        let notices = |store: &Store, target: &str| -> Vec<String> {
            store.outbox_for(target, 0).unwrap().0.into_iter().filter_map(|m| m.reply_to).collect()
        };
        // From a teammate's machine: the notice waits in the outbox for that machine.
        store.insert_from(&mut message("01REMOTE", "lars/gone"), false, Some("peer")).unwrap();
        // From a session here: it goes to that session's inbox, for the daemon to deliver.
        store.insert(&mut Message { from: "lars/api".into(), ..message("01LOCAL", "lars/gone") }, false).unwrap();
        // From before notices, its machine named later (see `sourceless`): back to it, as that machine knows its
        // terminal. One whose machine wasn't found goes without a notice.
        for id in ["01OLD", "01LOST"] {
            let m = Message { from: "sven/terminal@desktop".into(), ..message(id, "lars/gone") };
            store.insert(&mut m.clone(), false).unwrap();
            store.db.execute("UPDATE messages SET source = NULL WHERE id = ?1", [id]).unwrap();
        }
        let old: Vec<String> =
            store.sourceless().unwrap().into_iter().map(|(id, from)| format!("{id} {from}")).collect();
        assert_eq!(old, ["01OLD sven/terminal@desktop", "01LOST sven/terminal@desktop"]);
        store.set_source("01OLD", "peer").unwrap();
        store.forget_session(&session("gone"), "lars").unwrap();
        assert_eq!(notices(&store, "peer"), ["01REMOTE", "01OLD"]);
        assert!(
            store.outbox_for("peer", 0).unwrap().0.iter().all(|n| n.to == "sven/claude" || n.to == "sven/terminal")
        );
        let here: Vec<Message> = store.take_notices();
        assert_eq!(here.iter().map(|m| m.reply_to.as_deref()).collect::<Vec<_>>(), [Some("01LOCAL")]);
        assert_eq!(store.unread("lars/api").unwrap()[0].id, here[0].id);
        assert!(store.take_notices().is_empty());
        // Held and turned down: the same.
        store.insert_from(&mut message("01HELD", "lars/api"), true, Some("peer")).unwrap();
        store.settle_held(&["01HELD".into()], false).unwrap();
        assert_eq!(notices(&store, "peer"), ["01REMOTE", "01OLD", "01HELD"]);
        // Sent from here: refused once it was queued, or expired, the sender gets a notice; refused in the reply to
        // sending it, it already knows.
        for id in ["01REFUSED", "01SEEN", "01EXPIRED"] {
            store.outbox_add("sven-box", &Message { from: "lars/api".into(), ..message(id, "sven/web") }).unwrap();
        }
        store.outbox_done("01REFUSED", false, false).unwrap();
        store.outbox_done("01SEEN", false, true).unwrap();
        assert!(store.outbox_for("sven-box", 8 * 86_400_000).unwrap().0.is_empty());
        let told: Vec<Option<String>> = store.take_notices().into_iter().map(|m| m.reply_to).collect();
        assert_eq!(told, [Some("01REFUSED".into()), Some("01EXPIRED".into())]);
        // A notice dropped in turn makes no other: neither expired in the outbox nor forgotten unread.
        assert!(store.outbox_for("peer", util::now_ms() + 8 * 86_400_000).unwrap().0.is_empty());
        store.forget_session(&session("api"), "lars").unwrap();
        assert!(store.take_notices().is_empty());
        assert!(
            store.outbox_for("peer", 0).unwrap().0.is_empty() && store.outbox_for("sven-box", 0).unwrap().0.is_empty()
        );
    }

    #[test]
    fn a_store_from_before_keeps_the_persons_mailboxes_but_not_the_clis_ask_sessions() {
        let dir = std::env::temp_dir().join(format!("reason-network-mailbox-{}", ulid::Ulid::generate()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("network.db");
        {
            // The sessions table as 0.12.7 left it.
            let old = Connection::open(&db).unwrap();
            old.execute_batch(
                "CREATE TABLE sessions (name TEXT PRIMARY KEY, key TEXT NOT NULL, agent TEXT NOT NULL, cwd TEXT NOT NULL,
                 repo TEXT, branch TEXT, doing TEXT, tier TEXT NOT NULL, last_seen INTEGER NOT NULL,
                 attached INTEGER NOT NULL DEFAULT 1);
                 INSERT INTO sessions (name, key, agent, cwd, tier, last_seen) VALUES
                 ('terminal', 'cli:terminal', 'cli', '~', 'pull', 0),
                 ('review', 'cli:review', 'cli', '~', 'pull', 0),
                 ('ask-1a2b3c', 'cli:ask-1a2b3c', 'cli', '~', 'pull', 0),
                 ('ask-lunch', 'cli:ask-lunch', 'cli', '~', 'pull', 0),
                 ('api-claude', 'claude-code:a', 'claude-code', '~', 'push', 0);",
            )
            .unwrap();
        }
        let mailboxes = |store: &Store| -> Vec<String> {
            store.sessions().unwrap().into_iter().filter(|s| s.mailbox).map(|s| s.name).collect()
        };
        let store = Store::open(&db, temp_inbox()).unwrap();
        assert_eq!(mailboxes(&store), ["ask-lunch", "review", "terminal"]);
        drop(store);
        // Opened again, nothing changes.
        let store = Store::open(&db, temp_inbox()).unwrap();
        assert_eq!(mailboxes(&store), ["ask-lunch", "review", "terminal"]);
        drop(store);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn messages_sent_the_same_millisecond_come_out_in_the_order_they_came_in() {
        let store = Store::open(Path::new(":memory:"), temp_inbox()).unwrap();
        // Ids within one millisecond are random: here the later message has the smaller one.
        let msg = |id: &str, body: &str| Message { body: body.into(), sent_at: 1_000, ..message(id, "lars/api") };
        for (id, body) in [("01M5Z", "do X"), ("01M5A", "never mind X")] {
            store.insert(&mut msg(id, body), false).unwrap();
            store.insert(&mut msg(&format!("{id}H"), body), true).unwrap();
            store.outbox_add("peer", &msg(id, body)).unwrap();
        }
        let bodies = |ms: Vec<Message>| ms.into_iter().map(|m| m.body).collect::<Vec<_>>();
        assert_eq!(bodies(store.unread("lars/api").unwrap()), ["do X", "never mind X"]);
        assert_eq!(bodies(store.held().unwrap()), ["do X", "never mind X"]);
        assert_eq!(bodies(store.outbox_for("peer", 1_000).unwrap().0), ["do X", "never mind X"]);
    }

    fn temp_inbox() -> PathBuf {
        std::env::temp_dir().join(format!("reason-network-inbox-{}", ulid::Ulid::generate()))
    }

    fn file(name: &str, content: &str) -> Attachment {
        Attachment { name: name.into(), content: content.into(), path: None }
    }

    fn with_files(id: &str, to: &str) -> Message {
        Message {
            id: id.into(),
            from: "sven/claude".into(),
            from_agent: None,
            from_repo: None,
            to: to.into(),
            via: None,
            reply_to: None,
            body: "see the files".into(),
            wake: true,
            notify: false,
            sent_at: 0,
            approval: None,
            files: vec![file("notes.md", "# notes\n"), file("run.log", "ok\n")],
        }
    }

    /// The folder a stored message's files are in (from the path it lists).
    fn folder(m: &Message) -> PathBuf {
        PathBuf::from(m.files[0].path.as_ref().expect("saved")).parent().unwrap().to_path_buf()
    }

    /// The files in a folder: name, text, and whether anyone may execute it.
    fn saved(dir: &Path) -> Vec<(String, String, bool)> {
        let mut out: Vec<(String, String, bool)> = std::fs::read_dir(dir)
            .map(|d| {
                d.flatten()
                    .map(|e| {
                        #[cfg(unix)]
                        let exec =
                            std::os::unix::fs::PermissionsExt::mode(&e.metadata().unwrap().permissions()) & 0o111 != 0;
                        #[cfg(not(unix))]
                        let exec = false;
                        (e.file_name().to_string_lossy().into_owned(), std::fs::read_to_string(e.path()).unwrap(), exec)
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.sort();
        out
    }

    fn folders(inbox: &Path) -> HashSet<String> {
        std::fs::read_dir(inbox)
            .map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
            .unwrap_or_default()
    }

    fn stored_text(store: &Store, id: &str) -> Option<String> {
        store.db.query_row("SELECT files FROM messages WHERE id = ?1", [id], |r| r.get(0)).unwrap()
    }

    #[test]
    fn files_are_saved_in_a_folder_named_here_and_kept_out_of_the_database() {
        let inbox = temp_inbox();
        let store = Store::open(Path::new(":memory:"), inbox.clone()).unwrap();
        let mut m = with_files("01A", "lars/api");
        assert!(store.insert(&mut m, false).unwrap());
        let dir = folder(&m);
        let want =
            vec![("notes.md".to_string(), "# notes\n".to_string(), false), ("run.log".into(), "ok\n".into(), false)];
        assert_eq!(saved(&dir), want);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(dir.join("notes.md")).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        }
        // The message lists them by path, here and when read back; the database keeps only their names.
        assert!(
            m.files
                .iter()
                .all(|f| f.content.is_empty() && f.path.as_deref().unwrap().starts_with(&*dir.display().to_string()))
        );
        assert_eq!(store.unread("lars/api").unwrap()[0].files, m.files);
        assert!(!stored_text(&store, "01A").unwrap().contains("# notes"));
        // A retried delivery changes nothing.
        std::fs::write(dir.join("run.log"), "kept").unwrap();
        assert!(!store.insert(&mut with_files("01A", "lars/api"), false).unwrap());
        assert_eq!(std::fs::read_to_string(dir.join("run.log")).unwrap(), "kept");
        std::fs::remove_dir_all(&inbox).unwrap();
    }

    #[test]
    fn a_teammate_cant_name_another_teammates_folder() {
        let inbox = temp_inbox();
        let store = Store::open(Path::new(":memory:"), inbox.clone()).unwrap();
        let mut alice = with_files("01ALICEMSG", "lars/api");
        store.insert(&mut alice, false).unwrap();
        // The same id in another case (one name on macOS and Windows), and a device name: folders of their own.
        let mut bob = with_files("01alicemsg", "lars/api");
        bob.files = vec![file("notes.md", "bob's")];
        store.insert(&mut bob, false).unwrap();
        store.insert(&mut with_files("CON", "lars/api"), false).unwrap();
        assert_eq!(std::fs::read_to_string(folder(&alice).join("notes.md")).unwrap(), "# notes\n");
        assert_eq!(std::fs::read_to_string(folder(&bob).join("notes.md")).unwrap(), "bob's");
        let names = folders(&inbox);
        assert_eq!(names.len(), 3);
        assert!(names.iter().all(|n| ulid::Ulid::from_string(n).is_ok_and(|u| u.to_string() == *n)), "{names:?}");
        std::fs::remove_dir_all(&inbox).unwrap();
    }

    #[test]
    fn a_held_messages_files_are_saved_only_once_approved() {
        let inbox = temp_inbox();
        let store = Store::open(Path::new(":memory:"), inbox.clone()).unwrap();
        store.insert(&mut with_files("01H", "lars/api"), true).unwrap();
        store.insert(&mut with_files("01D", "lars/api"), true).unwrap();
        assert!(folders(&inbox).is_empty());
        // Held, the text is kept to save later (and shown to the person by name, with no path).
        let held = store.held().unwrap();
        assert_eq!(held[0].files[0].content, "# notes\n");
        assert_eq!(held[0].files[0].path, None);
        // Dropped: nothing is ever written.
        assert_eq!(store.settle_held(&["01D".into()], false).unwrap().0.len(), 1);
        assert!(folders(&inbox).is_empty() && store.message("01D").unwrap().is_none());
        // Approved: saved now, and listed by path.
        let (released, unsaved) = store.settle_held(&["01H".into()], true).unwrap();
        assert!(unsaved.is_empty());
        assert_eq!(saved(&folder(&released[0])).len(), 2);
        assert_eq!(store.unread("lars/api").unwrap()[0].files, released[0].files);
        assert!(!stored_text(&store, "01H").unwrap().contains("# notes"));
        std::fs::remove_dir_all(&inbox).unwrap();
    }

    #[test]
    fn a_held_message_whose_files_cant_be_saved_stays_held_and_the_rest_are_approved() {
        let inbox = temp_inbox();
        let store = Store::open(Path::new(":memory:"), inbox.clone()).unwrap();
        // A name no file system takes (it never passes `check`, but a damaged record could hold one).
        let mut bad = with_files("01BAD", "lars/api");
        bad.files = vec![file("no/such/folder.md", "x")];
        store.insert(&mut bad, true).unwrap();
        store.insert(&mut with_files("01GOOD", "lars/api"), true).unwrap();
        let (released, unsaved) = store.settle_held(&[], true).unwrap();
        assert_eq!(released.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["01GOOD"]);
        assert_eq!(unsaved.len(), 1);
        assert_eq!(unsaved[0].0, "01BAD");
        assert_eq!(store.held().unwrap().iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["01BAD"]);
        std::fs::remove_dir_all(&inbox).unwrap();
    }

    #[test]
    fn files_go_with_their_message_and_none_stay_long() {
        let inbox = temp_inbox();
        let store = Store::open(Path::new(":memory:"), inbox.clone()).unwrap();
        let day = 86_400_000;
        let session = |name: &str| Remembered {
            name: name.into(),
            key: name.into(),
            agent: "claude-code".into(),
            cwd: "~".into(),
            repo: None,
            branch: None,
            doing: None,
            tier: Tier::Push,
            last_seen: 0,
            attached: true,
            mailbox: false,
        };
        store.upsert_session(&session("gone"), "lars").unwrap();
        store.upsert_session(&session("api"), "lars").unwrap();
        let mut gone = with_files("01GONE", "lars/gone");
        store.insert(&mut gone, false).unwrap();
        let mut stored = HashMap::new();
        for id in ["01UNREAD", "01RECENT", "01OLD"] {
            let mut m = with_files(id, "lars/api");
            store.insert(&mut m, false).unwrap();
            stored.insert(id, folder(&m).file_name().unwrap().to_string_lossy().into_owned());
        }
        store.insert(&mut with_files("01HELD", "lars/api"), true).unwrap();
        std::fs::create_dir_all(inbox.join("01ORPHAN")).unwrap();
        // What they take counts against the quota: saved or held.
        let one = files::size(&with_files("x", "y").files);
        assert_eq!(store.files_kept().unwrap(), 5 * one);
        // Counted per sending person too (`sven/claude` sent them all; `sv` is someone else).
        assert_eq!((store.files_kept_from("sven").unwrap(), store.files_kept_from("sv").unwrap()), (5 * one, 0));
        // The unread mail of a session forgotten after a day goes, and its files with it.
        store.forget_sessions(i64::MAX, &HashSet::from(["api"]), "lars").unwrap();
        assert!(!folder(&gone).exists());
        // Read files stay a day; unread ones until read; a folder no message names goes.
        let now = util::now_ms();
        store.mark_read(&["01RECENT".into()], now - day / 2).unwrap();
        store.mark_read(&["01OLD".into()], now - day - 1).unwrap();
        store.sweep_files(now).unwrap();
        assert_eq!(folders(&inbox), HashSet::from([stored["01UNREAD"].clone(), stored["01RECENT"].clone()]));
        assert_eq!(store.files_kept().unwrap(), 3 * one);
        // A week on: unread files go (the message stays, without them), and so does a message held with files.
        store.sweep_files(now + files::KEEP_UNREAD_MS + 1).unwrap();
        assert!(folders(&inbox).is_empty());
        assert!(store.unread("lars/api").unwrap().iter().all(|m| m.files.is_empty()));
        assert_eq!(store.unread_count("lars/api").unwrap(), 1);
        assert!(store.held().unwrap().is_empty());
        assert_eq!(store.files_kept().unwrap(), 0);
        std::fs::remove_dir_all(&inbox).unwrap();
    }
}
