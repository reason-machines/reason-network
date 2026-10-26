//! Durable forwards and their receipts. A completed receipt outlives its payload so a lost acceptance ack cannot
//! resurrect mail that was delivered or dropped.

use super::Store;
use crate::proto::Message;
use anyhow::{Result, bail};
use rusqlite::{OptionalExtension, params};

pub const TTL_MS: i64 = 7 * 86_400_000;
const CLOCK_SKEW_MS: i64 = 5 * 60_000;
const MAX_PENDING: i64 = 1_000;
const MAX_BYTES: i64 = 64 * 1024 * 1024;
const MAX_RECEIPTS: i64 = 10_000;

pub struct AnchorMail {
    pub source: String,
    pub target: String,
    pub msg: Message,
    pub cancelled: bool,
}

impl Store {
    pub fn init_anchor(&self) -> Result<()> {
        self.db.execute_batch(
            "CREATE TABLE IF NOT EXISTS anchor_mail (
                id TEXT PRIMARY KEY,
                source TEXT NOT NULL,
                target TEXT NOT NULL,
                digest TEXT NOT NULL,
                msg TEXT,
                cancelled INTEGER NOT NULL DEFAULT 0,
                expires_at INTEGER NOT NULL
            );",
        )?;
        Ok(())
    }

    /// True on the first acceptance, false on an identical retry (including one after completion).
    pub fn anchor_accept(&self, source: &str, target: &str, msg: &Message, now: i64) -> Result<bool> {
        if msg.sent_at <= 0 || msg.sent_at > now.saturating_add(CLOCK_SKEW_MS) {
            bail!("invalid original send time");
        }
        let expires = msg.sent_at.saturating_add(TTL_MS);
        if expires <= now {
            bail!("handoff expired seven days after its original send");
        }
        let encoded = serde_json::to_string(msg)?;
        let digest = blake3::hash(encoded.as_bytes()).to_hex().to_string();
        let tx = self.db.unchecked_transaction()?;
        let previous: Option<(String, String, String)> = tx
            .query_row("SELECT source, target, digest FROM anchor_mail WHERE id = ?1", [&msg.id], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .optional()?;
        if let Some((old_source, old_target, old_digest)) = previous {
            if old_source != source || old_target != target || old_digest != digest {
                bail!("this message id was already accepted with different contents or machines");
            }
            return Ok(false);
        }
        tx.execute("DELETE FROM anchor_mail WHERE msg IS NULL AND expires_at <= ?1", [now])?;
        let (receipts, pending, bytes): (i64, i64, i64) = tx.query_row(
            "SELECT COUNT(*), COUNT(msg), COALESCE(SUM(length(CAST(msg AS BLOB))), 0) FROM anchor_mail",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        if receipts >= MAX_RECEIPTS || pending >= MAX_PENDING || encoded.len() as i64 > MAX_BYTES - bytes {
            bail!("anchor is full; the sender must keep its copy");
        }
        tx.execute(
            "INSERT INTO anchor_mail (id, source, target, digest, msg, expires_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![msg.id, source, target, digest, encoded, expires],
        )?;
        tx.commit()?;
        Ok(true)
    }

    pub fn anchor_pending(&self) -> Result<Vec<AnchorMail>> {
        let mut statement = self.db.prepare_cached(
            "SELECT source, target, msg, cancelled FROM anchor_mail WHERE msg IS NOT NULL ORDER BY rowid",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, bool>(3)?))
        })?;
        rows.map(|row| {
            let (source, target, encoded, cancelled) = row?;
            Ok(AnchorMail { source, target, msg: serde_json::from_str(&encoded)?, cancelled })
        })
        .collect()
    }

    /// Only the intended recipient's acknowledgment can release this payload.
    pub fn anchor_for_ack(&self, id: &str, target: &str) -> Result<Option<AnchorMail>> {
        let row: Option<(String, String, bool)> = self
            .db
            .query_row(
                "SELECT source, msg, cancelled FROM anchor_mail WHERE id = ?1 AND target = ?2 AND msg IS NOT NULL",
                params![id, target],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        row.map(|(source, encoded, cancelled)| {
            Ok(AnchorMail { source, target: target.to_string(), msg: serde_json::from_str(&encoded)?, cancelled })
        })
        .transpose()
    }

    /// The optional owner notice and release commit together; a storage error leaves the last copy intact.
    pub fn anchor_finish(&self, id: &str, notice: Option<&Message>) -> Result<()> {
        let tx = self.db.unchecked_transaction()?;
        self.finish_anchor(id, notice)?;
        tx.commit()?;
        Ok(())
    }

    /// Commit invalidation before attempting notices: failed notices must never make removed mail deliverable again.
    pub fn anchor_update_route(&self, key: &str, value: Option<&str>, drops: &[(String, Message)]) -> Result<()> {
        let tx = self.db.unchecked_transaction()?;
        match value {
            Some(value) => self.set(key, value)?,
            None => {
                self.db.execute("DELETE FROM kv WHERE key = ?1", [key])?;
            }
        }
        for (id, _) in drops {
            self.anchor_cancel(id)?;
        }
        tx.commit()?;
        for (id, notice) in drops {
            self.anchor_finish(id, Some(notice))?;
        }
        Ok(())
    }

    pub fn anchor_cancel(&self, id: &str) -> Result<()> {
        self.db.execute("UPDATE anchor_mail SET cancelled = 1 WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Mark every affected payload before the first fallible notice write. None means this machine left the team.
    pub fn anchor_cancel_machine(&self, machine: Option<&str>) -> Result<()> {
        self.db.execute(
            "UPDATE anchor_mail SET cancelled = 1 WHERE ?1 IS NULL OR source = ?1 OR target = ?1",
            [machine],
        )?;
        Ok(())
    }

    fn finish_anchor(&self, id: &str, notice: Option<&Message>) -> Result<()> {
        let pending = self
            .db
            .query_row("SELECT 1 FROM anchor_mail WHERE id = ?1 AND msg IS NOT NULL", [id], |_| Ok(()))
            .optional()?
            .is_some();
        if pending {
            if let Some(notice) = notice {
                if !notice.files.is_empty() {
                    bail!("anchor notices cannot contain files");
                }
                self.insert(&mut notice.clone(), false)?;
            }
            self.db.execute("UPDATE anchor_mail SET msg = NULL WHERE id = ?1", [id])?;
        }
        Ok(())
    }

    /// Pending expiry is handled by the daemon first, so dropping mail can leave an owner-visible notice.
    pub fn anchor_prune(&self, now: i64) -> Result<()> {
        self.db.execute("DELETE FROM anchor_mail WHERE msg IS NULL AND expires_at <= ?1", [now])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::Attachment;
    use std::path::Path;

    const NOW: i64 = 1_793_000_000_000;

    fn message(id: &str) -> Message {
        Message {
            id: id.into(),
            from: "alex/terminal".into(),
            from_agent: Some("cli".into()),
            from_repo: None,
            to: "alex/inbox".into(),
            via: None,
            reply_to: None,
            body: "handoff".into(),
            wake: true,
            notify: false,
            sent_at: NOW,
            approval: None,
            files: Vec::new(),
        }
    }

    fn store() -> Store {
        let inbox = std::env::temp_dir().join(format!("reason-network-anchor-{}", ulid::Ulid::generate()));
        let store = Store::open(Path::new(":memory:"), inbox).unwrap();
        store.init_anchor().unwrap();
        store
    }

    #[test]
    fn retries_keep_one_complete_copy_and_completion_keeps_its_receipt() {
        let store = store();
        let mut msg = message("mail");
        msg.files.push(Attachment { name: "task.md".into(), content: "do this".into(), path: None });
        assert!(store.anchor_accept("source", "target", &msg, NOW).unwrap());
        assert!(!store.anchor_accept("source", "target", &msg, NOW + 1).unwrap());
        let pending = store.anchor_pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].msg.files[0].content, "do this");
        assert!(store.anchor_for_ack("mail", "another-machine").unwrap().is_none());
        assert_eq!(store.anchor_for_ack("mail", "target").unwrap().unwrap().source, "source");
        store.anchor_finish("mail", None).unwrap();
        assert!(store.anchor_pending().unwrap().is_empty());
        assert!(!store.anchor_accept("source", "target", &msg, NOW + 2).unwrap());
        assert!(store.anchor_pending().unwrap().is_empty());
        assert!(store.anchor_for_ack("mail", "target").unwrap().is_none());
    }

    #[test]
    fn a_message_id_cannot_change_its_payload_source_target_or_expiry() {
        let store = store();
        let msg = message("mail");
        store.anchor_accept("source", "target", &msg, NOW).unwrap();
        for altered in
            [Message { body: "different".into(), ..msg.clone() }, Message { sent_at: NOW + 1, ..msg.clone() }]
        {
            assert!(store.anchor_accept("source", "target", &altered, NOW + 1).is_err());
        }
        assert!(store.anchor_accept("other-source", "target", &msg, NOW).is_err());
        assert!(store.anchor_accept("source", "other-target", &msg, NOW).is_err());
        assert_eq!(store.anchor_pending().unwrap()[0].msg.body, "handoff");
        store.anchor_finish("mail", None).unwrap();
        assert!(store.anchor_accept("other-source", "target", &msg, NOW).is_err());
    }

    #[test]
    fn expiry_is_inclusive_and_never_extended_by_a_retry() {
        let store = store();
        let msg = message("mail");
        store.anchor_accept("source", "target", &msg, NOW).unwrap();
        assert!(!store.anchor_accept("source", "target", &msg, NOW + TTL_MS - 1).unwrap());
        assert!(store.anchor_accept("source", "target", &msg, NOW + TTL_MS).is_err());
        store.anchor_prune(NOW + TTL_MS).unwrap();
        assert_eq!(store.anchor_pending().unwrap().len(), 1, "pending mail needs a drop notice first");
        store.anchor_finish("mail", None).unwrap();
        store.anchor_prune(NOW + TTL_MS - 1).unwrap();
        let receipts: i64 = store.db.query_row("SELECT COUNT(*) FROM anchor_mail", [], |r| r.get(0)).unwrap();
        assert_eq!(receipts, 1);
        store.anchor_prune(NOW + TTL_MS).unwrap();
        let receipts: i64 = store.db.query_row("SELECT COUNT(*) FROM anchor_mail", [], |r| r.get(0)).unwrap();
        assert_eq!(receipts, 0);
        assert!(store.anchor_accept("source", "target", &msg, NOW + TTL_MS + 1).is_err());
    }

    #[test]
    fn invalid_and_far_future_send_times_are_refused() {
        let store = store();
        for sent_at in [i64::MIN, 0, NOW + CLOCK_SKEW_MS + 1, i64::MAX] {
            let msg = Message { sent_at, ..message("mail") };
            assert!(store.anchor_accept("source", "target", &msg, NOW).is_err());
        }
        let msg = Message { sent_at: NOW + CLOCK_SKEW_MS, ..message("mail") };
        assert!(store.anchor_accept("source", "target", &msg, NOW).unwrap());
    }

    #[test]
    fn pending_count_is_bounded_but_identical_retries_still_succeed_when_full() {
        let store = store();
        for n in 0..MAX_PENDING {
            store.anchor_accept("source", "target", &message(&n.to_string()), NOW).unwrap();
        }
        assert!(store.anchor_accept("source", "target", &message("extra"), NOW).is_err());
        assert!(!store.anchor_accept("source", "target", &message("0"), NOW).unwrap());
        store.anchor_finish("0", None).unwrap();
        assert!(store.anchor_accept("source", "target", &message("extra"), NOW).unwrap());
        assert_eq!(store.anchor_pending().unwrap().len(), MAX_PENDING as usize);
    }

    #[test]
    fn quota_counts_serialized_bytes_including_multibyte_text() {
        let store = store();
        let mut msg = message("mail");
        msg.body = "界".repeat((MAX_BYTES / 3) as usize);
        assert!(store.anchor_accept("source", "target", &msg, NOW).is_err());
        assert!(store.anchor_pending().unwrap().is_empty());
        msg.body = "界".repeat((MAX_BYTES / 3 - 1024) as usize);
        store.anchor_accept("source", "target", &msg, NOW).unwrap();
        let next = Message { body: "x".repeat(4_000), ..message("next") };
        assert!(store.anchor_accept("source", "target", &next, NOW).is_err());
        assert_eq!(store.anchor_pending().unwrap().len(), 1);
    }

    #[test]
    fn completed_receipts_are_bounded_and_expired_ones_make_room() {
        let store = store();
        let tx = store.db.unchecked_transaction().unwrap();
        for n in 0..MAX_RECEIPTS {
            tx.execute(
                "INSERT INTO anchor_mail (id,source,target,digest,msg,expires_at) VALUES (?1, 'source', 'target', 'digest', NULL, ?2)",
                params![n.to_string(), NOW + 1],
            )
            .unwrap();
        }
        tx.commit().unwrap();
        assert!(store.anchor_accept("source", "target", &message("extra"), NOW).is_err());
        assert!(store.anchor_accept("source", "target", &message("extra"), NOW + 1).unwrap());
        let receipts: i64 = store.db.query_row("SELECT COUNT(*) FROM anchor_mail", [], |r| r.get(0)).unwrap();
        assert_eq!(receipts, 1);
    }

    #[test]
    fn a_failed_notice_keeps_the_payload_and_a_successful_notice_is_saved_once() {
        let store = store();
        store.anchor_accept("source", "target", &message("mail"), NOW).unwrap();
        let notice = Message { to: "alex/terminal".into(), ..message("notice") };
        store
            .db
            .execute_batch(
                "CREATE TRIGGER fail_notice BEFORE INSERT ON messages BEGIN SELECT RAISE(FAIL, 'disk failure'); END;",
            )
            .unwrap();
        assert!(store.anchor_finish("mail", Some(&notice)).is_err());
        assert_eq!(store.anchor_pending().unwrap().len(), 1);
        assert!(!store.has("notice").unwrap());
        store.db.execute_batch("DROP TRIGGER fail_notice").unwrap();
        store.anchor_finish("mail", Some(&notice)).unwrap();
        store.anchor_finish("mail", Some(&Message { id: "second-notice".into(), ..notice })).unwrap();
        assert!(store.anchor_pending().unwrap().is_empty());
        assert_eq!(store.unread("alex/terminal").unwrap().len(), 1);
    }

    #[test]
    fn a_failed_release_rolls_back_the_notice_too() {
        let store = store();
        store.anchor_accept("source", "target", &message("mail"), NOW).unwrap();
        store.db.execute_batch(
            "CREATE TRIGGER fail_release BEFORE UPDATE ON anchor_mail BEGIN SELECT RAISE(FAIL, 'disk failure'); END;",
        ).unwrap();
        assert!(store.anchor_finish("mail", Some(&message("notice"))).is_err());
        assert_eq!(store.anchor_pending().unwrap().len(), 1);
        assert!(!store.has("notice").unwrap());
    }

    #[test]
    fn a_notice_with_files_is_refused_without_writing_files_or_releasing_mail() {
        let store = store();
        store.anchor_accept("source", "target", &message("mail"), NOW).unwrap();
        let mut notice = message("notice");
        notice.files.push(Attachment { name: "unexpected.md".into(), content: "x".into(), path: None });
        assert!(store.anchor_finish("mail", Some(&notice)).is_err());
        assert_eq!(store.anchor_pending().unwrap().len(), 1);
        assert!(!store.inbox.exists());
    }

    #[test]
    fn pending_payload_and_completed_receipt_survive_reopen() {
        let dir = std::env::temp_dir().join(format!("reason-network-anchor-reopen-{}", ulid::Ulid::generate()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("network.db");
        let msg = message("mail");
        {
            let store = Store::open(&path, dir.join("inbox")).unwrap();
            store.init_anchor().unwrap();
            store.anchor_accept("source", "target", &msg, NOW).unwrap();
        }
        {
            let store = Store::open(&path, dir.join("inbox")).unwrap();
            store.init_anchor().unwrap();
            assert_eq!(store.anchor_pending().unwrap()[0].msg.body, msg.body);
            store.anchor_finish("mail", Some(&message("notice"))).unwrap();
        }
        {
            let store = Store::open(&path, dir.join("inbox")).unwrap();
            store.init_anchor().unwrap();
            assert!(store.anchor_pending().unwrap().is_empty());
            assert!(!store.anchor_accept("source", "target", &msg, NOW + 1).unwrap());
            assert!(store.has("notice").unwrap());
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
