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
const MAX_SOURCE_PENDING: i64 = MAX_PENDING / 4;
const MAX_SOURCE_BYTES: i64 = MAX_BYTES / 4;
const MAX_SOURCE_RECEIPTS: i64 = MAX_RECEIPTS / 4;
pub const RESULT_KEEP_MS: i64 = TTL_MS;

pub struct AnchorMail {
    pub source: String,
    pub target: String,
    pub msg: Message,
    pub cancelled: bool,
}

pub struct PendingAnchor {
    pub id: String,
    pub source: String,
    pub target: String,
    pub cancelled: bool,
    pub expires_at: i64,
}

impl Store {
    /// A keyed retry must not rebuild an envelope whose outbox payload or custody already exists.
    pub fn retry_remote_send(&self, id: &str) -> Result<Option<crate::proto::Recipient>> {
        let receipt: Option<(String, String)> = self
            .db
            .query_row("SELECT recipient,stage FROM sent_receipts WHERE id=?1", [id], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        let Some((address, stage)) = receipt else { return Ok(None) };
        let queued = self.db.prepare_cached("SELECT 1 FROM outbox WHERE id=?1")?.exists([id])?;
        let anchored = self
            .db
            .prepare_cached("SELECT 1 FROM anchor_sends WHERE id=?1 AND retain_until>?2")?
            .exists(params![id, crate::util::now_ms()])?;
        if !queued
            && !anchored
            && !matches!(stage.as_str(), "accepted" | "held" | "handed_to_agent" | "replied" | "rejected" | "expired")
        {
            return Ok(None);
        }
        let state = match stage.as_str() {
            "anchored" => "stored",
            "rejected" | "expired" => "refused",
            "handed_to_agent" | "replied" => "accepted",
            _ => &stage,
        };
        Ok(Some(crate::proto::Recipient {
            address,
            state: state.into(),
            note: format!("original message retained with status {stage}; retry does not create another handoff"),
        }))
    }

    pub fn init_anchor(&self) -> Result<()> {
        self.db.execute_batch(
            "CREATE TABLE IF NOT EXISTS anchor_mail (
                id TEXT PRIMARY KEY,
                source TEXT NOT NULL,
                target TEXT NOT NULL,
                digest TEXT NOT NULL,
                msg TEXT,
                cancelled INTEGER NOT NULL DEFAULT 0,
                expires_at INTEGER NOT NULL,
                final_stage TEXT NOT NULL DEFAULT 'stored'
            );
            CREATE INDEX IF NOT EXISTS anchor_source ON anchor_mail(source);
            CREATE TABLE IF NOT EXISTS anchor_sends (
                id TEXT PRIMARY KEY, anchor TEXT NOT NULL, target TEXT NOT NULL,
                retain_until INTEGER NOT NULL, complete INTEGER NOT NULL DEFAULT 0
            );",
        )?;
        if !self.db.prepare("SELECT 1 FROM pragma_table_info('anchor_mail') WHERE name='final_stage'")?.exists([])? {
            let tx = self.db.unchecked_transaction()?;
            tx.execute("ALTER TABLE anchor_mail ADD COLUMN final_stage TEXT NOT NULL DEFAULT 'stored'", [])?;
            tx.execute("UPDATE anchor_mail SET final_stage='unknown' WHERE msg IS NULL", [])?;
            tx.commit()?;
        }
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
        tx.execute("DELETE FROM anchor_mail WHERE msg IS NULL AND expires_at <= ?1", [now - RESULT_KEEP_MS])?;
        let (receipts, pending, bytes): (i64, i64, i64) = tx.query_row(
            "SELECT COUNT(*), COUNT(msg), COALESCE(SUM(length(CAST(msg AS BLOB))), 0) FROM anchor_mail",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        if receipts >= MAX_RECEIPTS || pending >= MAX_PENDING || encoded.len() as i64 > MAX_BYTES - bytes {
            bail!("anchor is full; the sender must keep its copy");
        }
        let (receipts, pending, bytes): (i64, i64, i64) = tx.query_row(
            "SELECT COUNT(*), COUNT(msg), COALESCE(SUM(length(CAST(msg AS BLOB))), 0) FROM anchor_mail WHERE source=?1",
            [source],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        if receipts >= MAX_SOURCE_RECEIPTS
            || pending >= MAX_SOURCE_PENDING
            || encoded.len() as i64 > MAX_SOURCE_BYTES - bytes
        {
            bail!("this sender's anchor share is full; the sender must keep its copy");
        }
        tx.execute(
            "INSERT INTO anchor_mail (id, source, target, digest, msg, expires_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![msg.id, source, target, digest, encoded, expires],
        )?;
        tx.commit()?;
        Ok(true)
    }

    pub fn anchor_holds_mail(&self) -> Result<bool> {
        Ok(self
            .db
            .prepare_cached("SELECT EXISTS(SELECT 1 FROM anchor_mail WHERE msg IS NOT NULL)")?
            .query_row([], |row| row.get(0))?)
    }

    /// Completed receipts still need pruning when the feature is switched off.
    pub fn anchor_has_work(&self) -> Result<bool> {
        Ok(self
            .db
            .prepare_cached("SELECT EXISTS(SELECT 1 FROM anchor_mail) OR EXISTS(SELECT 1 FROM anchor_sends)")?
            .query_row([], |row| row.get(0))?)
    }

    pub fn anchor_pending_meta(&self) -> Result<Vec<PendingAnchor>> {
        let mut statement = self.db.prepare_cached(
            "SELECT id, source, target, cancelled, expires_at FROM anchor_mail WHERE msg IS NOT NULL ORDER BY rowid",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(PendingAnchor {
                id: row.get(0)?,
                source: row.get(1)?,
                target: row.get(2)?,
                cancelled: row.get(3)?,
                expires_at: row.get(4)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
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
            #[cfg(test)]
            self.anchor_payload_reads.set(self.anchor_payload_reads.get() + 1);
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
            #[cfg(test)]
            self.anchor_payload_reads.set(self.anchor_payload_reads.get() + 1);
            Ok(AnchorMail { source, target: target.to_string(), msg: serde_json::from_str(&encoded)?, cancelled })
        })
        .transpose()
    }

    /// The optional owner notice and release commit together; a storage error leaves the last copy intact.
    pub fn anchor_finish(&self, id: &str, notice: Option<&Message>) -> Result<()> {
        self.anchor_finish_as(id, notice, if notice.is_some() { "rejected" } else { "accepted" })
    }

    pub fn anchor_finish_as(&self, id: &str, notice: Option<&Message>, stage: &str) -> Result<()> {
        anyhow::ensure!(matches!(stage, "accepted" | "rejected" | "expired"), "invalid anchor result");
        let tx = self.db.unchecked_transaction()?;
        self.finish_anchor(id, notice, stage)?;
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

    fn finish_anchor(&self, id: &str, notice: Option<&Message>, stage: &str) -> Result<()> {
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
            self.db.execute("UPDATE anchor_mail SET msg = NULL, final_stage=?2 WHERE id = ?1", params![id, stage])?;
        }
        Ok(())
    }

    /// Pending expiry is handled by the daemon first, so dropping mail can leave an owner-visible notice.
    pub fn anchor_prune(&self, now: i64) -> Result<()> {
        self.db.execute("DELETE FROM anchor_mail WHERE msg IS NULL AND expires_at <= ?1", [now - RESULT_KEEP_MS])?;
        self.db.execute("DELETE FROM anchor_sends WHERE retain_until <= ?1", [now])?;
        Ok(())
    }

    /// The source saves a compact receipt route before releasing its payload, in the same transaction.
    pub fn outbox_anchored(&self, anchor: &str, target: &str, id: &str) -> Result<bool> {
        let tx = self.db.unchecked_transaction()?;
        let sent: Option<i64> = self
            .db
            .query_row("SELECT created_at FROM outbox WHERE id=?1 AND target=?2", params![id, target], |r| r.get(0))
            .optional()?;
        let Some(sent) = sent else { return Ok(false) };
        self.db.execute(
            "INSERT INTO anchor_sends(id,anchor,target,retain_until) VALUES(?1,?2,?3,?4)",
            params![id, anchor, target, sent.saturating_add(TTL_MS).saturating_add(RESULT_KEEP_MS)],
        )?;
        self.finish_outbox(id, true)?;
        self.db.execute(
            "UPDATE sent_receipts SET stage='anchored' WHERE id=?1 AND stage NOT IN ('handed_to_agent','replied')",
            [id],
        )?;
        tx.commit()?;
        Ok(true)
    }

    pub fn anchor_queries(&self, now: i64) -> Result<Vec<(String, String, String)>> {
        let mut st =
            self.db.prepare("SELECT id,anchor,target FROM anchor_sends WHERE complete=0 AND retain_until>?1")?;
        Ok(st.query_map([now], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?)
    }

    pub fn anchor_result(&self, source: &str, target: &str, id: &str, now: i64) -> Result<Option<String>> {
        Ok(self
            .db
            .query_row(
                "SELECT final_stage FROM anchor_mail WHERE id=?1 AND source=?2 AND target=?3 AND expires_at>?4",
                params![id, source, target, now - RESULT_KEEP_MS],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn anchor_record_result(&self, anchor: &str, target: &str, id: &str, stage: &str) -> Result<bool> {
        if !matches!(stage, "accepted" | "rejected" | "expired" | "unknown") {
            return Ok(false);
        }
        let tx = self.db.unchecked_transaction()?;
        let changed = self.db.execute("UPDATE anchor_sends SET complete=1 WHERE id=?1 AND anchor=?2 AND target=?3 AND complete=0 AND retain_until>?4",
            params![id,anchor,target,crate::util::now_ms()])?;
        if changed == 1 {
            self.db.execute("UPDATE sent_receipts SET stage=?2,updated_at=?3 WHERE id=?1 AND stage NOT IN ('handed_to_agent','replied')",
                params![id,stage,crate::util::now_ms()])?;
        }
        tx.commit()?;
        Ok(changed == 1)
    }

    /// A direct recipient ACK can race the anchor's acceptance or expiry; actual storage wins over a relay outcome.
    pub fn anchor_direct_done(&self, target: &str, id: &str) -> Result<bool> {
        let tx = self.db.unchecked_transaction()?;
        let valid = self.db.execute(
            "UPDATE anchor_sends SET complete=1 WHERE id=?1 AND target=?2 AND retain_until>?3",
            params![id, target, crate::util::now_ms()],
        )?;
        let changed = if valid == 1 {
            self.db.execute("UPDATE sent_receipts SET stage='accepted',updated_at=?2 WHERE id=?1 AND stage NOT IN ('accepted','handed_to_agent','replied')",
                params![id,crate::util::now_ms()])? > 0
        } else {
            false
        };
        tx.commit()?;
        Ok(changed)
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
    fn metadata_can_outlive_a_payload_released_by_an_ack() {
        let s = store();
        let msg = message("released");
        s.anchor_accept("source", "target", &msg, NOW).unwrap();
        let pending = s.anchor_pending_meta().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, msg.id);
        assert_eq!(pending[0].source, "source");
        assert_eq!(pending[0].target, "target");
        assert_eq!(pending[0].expires_at, msg.sent_at + TTL_MS);
        assert!(!pending[0].cancelled);
        assert_eq!(s.anchor_payload_reads.get(), 0);
        s.anchor_finish(&msg.id, None).unwrap();
        assert!(s.anchor_for_ack(&pending[0].id, &pending[0].target).unwrap().is_none());
        assert!(s.anchor_pending_meta().unwrap().is_empty());
        s.outbox_add("target", &msg).unwrap();
        assert_eq!(s.outbox_meta_for("target", NOW).unwrap().0, [(msg.id.clone(), None)]);
        assert!(s.outbox_message("other-target", &msg.id).unwrap().is_none());
        assert_eq!(s.outbox_reads.get(), 0);
        s.outbox_done_from("target", &msg.id, true).unwrap();
        assert!(s.outbox_message("target", &msg.id).unwrap().is_none());
    }

    #[test]
    fn completed_receipts_keep_cleanup_reachable_without_advertising_custody() {
        let s = store();
        let now = crate::util::now_ms();
        assert!(!s.anchor_has_work().unwrap());
        let mut msg = message("old-result");
        msg.sent_at = now - TTL_MS - RESULT_KEEP_MS - 1;
        s.anchor_accept("source", "target", &msg, msg.sent_at).unwrap();
        s.anchor_finish(&msg.id, None).unwrap();
        assert!(s.anchor_has_work().unwrap());
        assert!(!s.anchor_holds_mail().unwrap());
        s.anchor_prune(now).unwrap();
        assert!(!s.anchor_has_work().unwrap());
    }

    #[test]
    fn a_senders_share_leaves_room_for_other_machines_and_retry_receipts() {
        let store = store();
        for n in 0..MAX_SOURCE_PENDING {
            store.anchor_accept("busy", "target", &message(&n.to_string()), NOW).unwrap();
        }
        assert!(store.anchor_accept("busy", "target", &message("extra"), NOW).is_err());
        assert!(!store.anchor_accept("busy", "target", &message("0"), NOW).unwrap());
        assert!(store.anchor_accept("another", "target", &message("other"), NOW).unwrap());
        for n in 0..MAX_SOURCE_PENDING {
            store.anchor_finish(&n.to_string(), None).unwrap();
        }
        for n in MAX_SOURCE_PENDING..MAX_SOURCE_RECEIPTS {
            store.anchor_accept("busy", "target", &message(&n.to_string()), NOW).unwrap();
            store.anchor_finish(&n.to_string(), None).unwrap();
        }
        assert!(store.anchor_accept("busy", "target", &message("more"), NOW).is_err());
        assert!(store.anchor_accept("another", "target", &message("other2"), NOW).unwrap());
    }

    #[test]
    fn source_receipts_are_bound_to_the_original_custodian_and_final_outcome() {
        let source = store();
        let anchor = store();
        let now = crate::util::now_ms();
        let msg = Message { sent_at: now, ..message("mail") };
        source.record_sent("sender", &msg, "alex/inbox@laptop", "queued").unwrap();
        source.outbox_add("target", &msg).unwrap();
        anchor.anchor_accept("source", "target", &msg, now).unwrap();
        source.outbox_anchored("anchor", "target", &msg.id).unwrap();
        let stage = || source.delivery_status("sender", std::slice::from_ref(&msg.id)).unwrap()[0]["stage"].clone();
        assert_eq!(stage(), "anchored");
        assert!(source.outbox_for("target", now).unwrap().0.is_empty());
        assert!(anchor.anchor_result("stranger", "target", &msg.id, now).unwrap().is_none());
        assert!(!source.anchor_record_result("imposter", "target", &msg.id, "rejected").unwrap());
        assert!(!source.anchor_record_result("anchor", "other-target", &msg.id, "expired").unwrap());
        anchor.anchor_finish_as(&msg.id, None, "expired").unwrap();
        let outcome = anchor.anchor_result("source", "target", &msg.id, now + TTL_MS).unwrap().unwrap();
        assert_eq!(outcome, "expired");
        source.anchor_record_result("anchor", "target", &msg.id, &outcome).unwrap();
        assert_eq!(stage(), "expired");
        assert!(source.anchor_queries(now).unwrap().is_empty());
        assert!(!source.anchor_record_result("anchor", "target", &msg.id, "accepted").unwrap());
        assert_eq!(stage(), "expired");
        assert!(anchor.anchor_result("source", "target", &msg.id, now + TTL_MS + RESULT_KEEP_MS).unwrap().is_none());
    }

    #[test]
    fn interrupted_legacy_result_migration_rolls_back_and_recovers_as_unknown() {
        let store = store();
        store.db.execute_batch("DROP TABLE anchor_mail;
            CREATE TABLE anchor_mail(id TEXT PRIMARY KEY,source TEXT NOT NULL,target TEXT NOT NULL,digest TEXT NOT NULL,msg TEXT,cancelled INTEGER NOT NULL DEFAULT 0,expires_at INTEGER NOT NULL);
            INSERT INTO anchor_mail VALUES('legacy','source','target','digest',NULL,0,9999999999999);
            CREATE TRIGGER fail_result_upgrade BEFORE UPDATE ON anchor_mail BEGIN SELECT RAISE(FAIL,'disk failed'); END;").unwrap();
        assert!(store.init_anchor().is_err());
        assert!(
            !store
                .db
                .prepare("SELECT 1 FROM pragma_table_info('anchor_mail') WHERE name='final_stage'")
                .unwrap()
                .exists([])
                .unwrap()
        );
        store.db.execute_batch("DROP TRIGGER fail_result_upgrade").unwrap();
        store.init_anchor().unwrap();
        assert_eq!(store.anchor_result("source", "target", "legacy", NOW).unwrap().as_deref(), Some("unknown"));
        let now = crate::util::now_ms();
        let msg = Message { sent_at: now, ..message("legacy") };
        store.record_sent("sender", &msg, "alex/inbox", "queued").unwrap();
        store.outbox_add("target", &msg).unwrap();
        store.outbox_anchored("anchor", "target", "legacy").unwrap();
        assert!(store.anchor_record_result("anchor", "target", "legacy", "unknown").unwrap());
        assert_eq!(store.delivery_status("sender", &["legacy".into()]).unwrap()[0]["stage"], "unknown");
        assert!(store.anchor_queries(now).unwrap().is_empty());
    }

    #[test]
    fn source_expiry_results_remain_readable_after_the_original_send_week() {
        let store = store();
        let now = crate::util::now_ms();
        let msg = Message { sent_at: now - TTL_MS - 1, ..message("mail") };
        store.record_sent("sender", &msg, "alex/inbox", "queued").unwrap();
        store.outbox_add("target", &msg).unwrap();
        store.outbox_anchored("anchor", "target", "mail").unwrap();
        store.anchor_record_result("anchor", "target", "mail", "expired").unwrap();
        store.sweep_files(now).unwrap();
        assert_eq!(store.delivery_status("sender", &["mail".into()]).unwrap()[0]["stage"], "expired");
        store.anchor_prune(now + RESULT_KEEP_MS).unwrap();
        store.sweep_files(now + RESULT_KEEP_MS).unwrap();
        assert_eq!(store.delivery_status("sender", &["mail".into()]).unwrap()[0]["stage"], "unknown");
    }

    #[test]
    fn a_late_direct_storage_ack_wins_over_anchor_expiry_without_double_counting() {
        let store = store();
        let now = crate::util::now_ms();
        let msg = Message { sent_at: now, ..message("mail") };
        store.record_sent("sender", &msg, "alex/inbox", "queued").unwrap();
        store.outbox_add("target", &msg).unwrap();
        store.outbox_anchored("anchor", "target", "mail").unwrap();
        assert!(!store.outbox_done_from("target", "mail", true).unwrap());
        assert!(!store.anchor_direct_done("other", "mail").unwrap());
        store.anchor_record_result("anchor", "target", "mail", "expired").unwrap();
        assert!(store.anchor_direct_done("target", "mail").unwrap());
        assert!(!store.anchor_record_result("anchor", "target", "mail", "rejected").unwrap());
        assert!(!store.anchor_direct_done("target", "mail").unwrap());
        assert_eq!(store.delivery_status("sender", &["mail".into()]).unwrap()[0]["stage"], "accepted");
        assert_eq!(store.counts(now).unwrap().iter().map(|c| c.sent).sum::<u64>(), 1);
    }

    #[test]
    fn failed_source_acceptance_preserves_the_payload_and_retry_state() {
        let store = store();
        let msg = Message { sent_at: crate::util::now_ms(), ..message("mail") };
        store.record_sent("sender", &msg, "alex/inbox", "queued").unwrap();
        store.outbox_add("target", &msg).unwrap();
        store.db.execute_batch("CREATE TRIGGER fail_source_release BEFORE DELETE ON outbox BEGIN SELECT RAISE(FAIL,'disk failed'); END;").unwrap();
        assert!(store.outbox_anchored("anchor", "target", &msg.id).is_err());
        assert_eq!(store.outbox_for("target", crate::util::now_ms()).unwrap().0.len(), 1);
        assert!(store.anchor_queries(crate::util::now_ms()).unwrap().is_empty());
        assert_eq!(store.delivery_status("sender", &[msg.id]).unwrap()[0]["stage"], "queued");
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
        assert_eq!(receipts, 1, "compact outcomes survive payload expiry");
        store.anchor_prune(NOW + TTL_MS + RESULT_KEEP_MS).unwrap();
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
            store
                .anchor_accept(&format!("source{}", n / MAX_SOURCE_PENDING), "target", &message(&n.to_string()), NOW)
                .unwrap();
        }
        assert!(store.anchor_accept("source", "target", &message("extra"), NOW).is_err());
        assert!(!store.anchor_accept("source0", "target", &message("0"), NOW).unwrap());
        store.anchor_finish("0", None).unwrap();
        assert!(store.anchor_accept("source0", "target", &message("extra"), NOW).unwrap());
        assert_eq!(store.anchor_pending().unwrap().len(), MAX_PENDING as usize);
    }

    #[test]
    fn quota_counts_serialized_bytes_including_multibyte_text() {
        let store = store();
        let mut msg = message("mail");
        msg.body = "界".repeat((MAX_BYTES / 3) as usize);
        assert!(store.anchor_accept("source", "target", &msg, NOW).is_err());
        assert!(store.anchor_pending().unwrap().is_empty());
        msg.body = "界".repeat((MAX_SOURCE_BYTES / 3 - 1024) as usize);
        store.anchor_accept("source", "target", &msg, NOW).unwrap();
        let next = Message { body: "x".repeat(4_000), ..message("next") };
        assert!(store.anchor_accept("source", "target", &next, NOW).is_err());
        assert!(store.anchor_accept("another", "target", &message("other"), NOW).unwrap());
        assert_eq!(store.anchor_pending().unwrap().len(), 2);
    }

    #[test]
    fn completed_receipts_are_bounded_and_expired_ones_make_room() {
        let store = store();
        let tx = store.db.unchecked_transaction().unwrap();
        for n in 0..MAX_RECEIPTS {
            tx.execute(
                "INSERT INTO anchor_mail (id,source,target,digest,msg,expires_at) VALUES (?1, 'source', 'target', 'digest', NULL, ?2)",
                params![n.to_string(), NOW + 1 - RESULT_KEEP_MS],
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
