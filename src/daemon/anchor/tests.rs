use super::*;
use crate::{
    config::{Config, Policy},
    daemon::{directory::Target, test_daemon},
    git::GitInfo,
    proto::{Approval, Attachment, SessionReg, Tier},
    store::{Remembered, Store},
    team::{Signed, TeamState},
};
use iroh::SecretKey;

struct Team {
    daemon: Arc<Daemon>,
    source: SecretKey,
    laptop: SecretKey,
    stranger: SecretKey,
}

fn team(anchor: bool) -> Team {
    configured(Config { person: Some("alex".into()), anchor, ..Default::default() })
}

fn configured(cfg: Config) -> Team {
    let daemon = Arc::new(test_daemon(cfg));
    let (source, laptop, stranger) = (SecretKey::generate(), SecretKey::generate(), SecretKey::generate());
    let mut roster = TeamState::found("acme", &daemon.person, &daemon.machine, &daemon.key);
    for (key, person, machine) in
        [(&source, "alex", "cloud"), (&laptop, "alex", "laptop"), (&stranger, "bob", "desktop")]
    {
        roster.put_member(&daemon.key, key.public(), person, machine, true);
    }
    daemon.inner.lock().unwrap().team = Some(roster);
    Team { daemon, source, laptop, stranger }
}

fn message() -> Message {
    Message {
        id: ulid::Ulid::generate().to_string(),
        from: "alex/terminal".into(),
        from_agent: Some("cli".into()),
        from_repo: None,
        to: "alex/inbox".into(),
        via: None,
        reply_to: None,
        body: "Please continue this task.".into(),
        wake: false,
        notify: false,
        sent_at: util::now_ms(),
        approval: None,
        files: Vec::new(),
    }
}

fn permanent(names: &[&str]) -> Presence {
    Presence {
        features: vec![FEATURE.into()],
        mailboxes: names.iter().map(|n| n.to_string()).collect(),
        ..Default::default()
    }
}

fn advertised(machine: EndpointId, names: &[&str]) -> Remote {
    Remote {
        presence: Presence {
            features: vec!["anchor".into(), FEATURE.into()],
            anchor_routes: vec![MailboxRoute { machine, names: names.iter().map(|n| n.to_string()).collect() }],
            ..Default::default()
        },
        last_seen: util::now_ms(),
        online: true,
    }
}

fn remember(d: &Daemon, name: &str, mailbox: bool) {
    let r = Remembered {
        name: name.into(),
        key: format!("cli:{name}"),
        agent: "cli".into(),
        cwd: "/".into(),
        repo: None,
        branch: None,
        doing: None,
        tier: Tier::Pull,
        last_seen: util::now_ms(),
        attached: true,
        mailbox,
    };
    d.inner.lock().unwrap().store.upsert_session(&r, &d.person).unwrap();
}

fn learn(t: &Team) {
    t.daemon.learn_mailboxes(&t.laptop.public(), &permanent(&["inbox"])).unwrap();
}

#[test]
fn anchor_advertisement_requires_local_opt_in() {
    for enabled in [false, true] {
        let t = team(enabled);
        let presence = t.daemon.presence_of(Vec::new());
        assert_eq!(presence.has("anchor"), enabled);
        assert!(presence.has(FEATURE), "receiving forwards is supported without offering custody");
        assert_eq!(t.daemon.anchor_local(), enabled);
    }
}

#[test]
fn only_active_own_machines_can_offer_or_receive_custody() {
    let t = team(true);
    learn(&t);
    let msg = message();
    for (source, target) in [
        (t.stranger.public(), t.laptop.public()),
        (t.source.public(), t.stranger.public()),
        (t.source.public(), t.daemon.key.public()),
        (SecretKey::generate().public(), t.laptop.public()),
    ] {
        assert!(t.daemon.accept_anchor(source, target, msg.clone()).is_err());
    }
    for altered in
        [Message { from: "bob/terminal".into(), ..msg.clone() }, Message { to: "bob/inbox".into(), ..msg.clone() }]
    {
        assert!(t.daemon.accept_anchor(t.source.public(), t.laptop.public(), altered).is_err());
    }
    t.daemon.inner.lock().unwrap().team.as_mut().unwrap().put_member(
        &t.daemon.key,
        t.source.public(),
        "alex",
        "cloud",
        false,
    );
    assert!(!t.daemon.same_owner(&t.source.public()));
    assert!(t.daemon.accept_anchor(t.source.public(), t.laptop.public(), msg).is_err());
    assert!(t.daemon.inner.lock().unwrap().store.anchor_pending().unwrap().is_empty());
}

#[test]
fn an_unconfigured_machine_does_not_accept_custody_or_learn_routes() {
    let t = team(false);
    learn(&t);
    assert!(t.daemon.saved_mailboxes(&t.daemon.inner.lock().unwrap()).is_empty());
    assert!(t.daemon.accept_anchor(t.source.public(), t.laptop.public(), message()).is_err());
}

#[test]
fn learning_keeps_explicit_permanent_mailboxes_and_ignores_other_sessions_and_anchors() {
    let t = team(true);
    let mut p = permanent(&["inbox"]);
    p.known = vec!["ordinary".into()];
    p.kept = vec!["ordinary".into()];
    p.anchor_routes = vec![MailboxRoute { machine: t.source.public(), names: vec!["invented".into()] }];
    t.daemon.learn_mailboxes(&t.laptop.public(), &p).unwrap();
    t.daemon.learn_mailboxes(&t.stranger.public(), &permanent(&["not-ours"])).unwrap();
    let g = t.daemon.inner.lock().unwrap();
    let routes = t.daemon.saved_mailboxes(&g);
    assert_eq!(routes.len(), 1);
    assert_eq!(routes[0].machine, t.laptop.public());
    assert_eq!(routes[0].names, ["inbox"]);
}

#[test]
fn live_and_offline_permanent_mailboxes_are_announced_without_ordinary_sessions() {
    let t = team(false);
    remember(&t.daemon, "inbox", true);
    remember(&t.daemon, "ordinary", false);
    let reg = SessionReg {
        key: "cli:terminal".into(),
        agent: "cli".into(),
        cwd: "/".into(),
        agent_session_id: None,
        pid: None,
        tier: Tier::Pull,
        name: Some("terminal".into()),
    };
    t.daemon.register(Some(1), reg, GitInfo::default()).unwrap();
    let row = t.daemon.row("terminal", "cli", "/", None);
    let p = t.daemon.presence_of(vec![row]);
    assert_eq!(p.mailboxes, ["inbox", "terminal"]);
    assert!(p.known.contains(&"inbox".into()));
    assert!(!p.mailboxes.contains(&"ordinary".into()));
    assert!(!p.known.contains(&"terminal".into()), "the terminal is live");
}

#[test]
fn a_fresh_workspace_resolves_a_saved_destination_only_from_its_own_online_anchor() {
    let t = team(false);
    let remotes = HashMap::from([(t.source.public(), advertised(t.laptop.public(), &["inbox"]))]);
    let g = t.daemon.inner.lock().unwrap();
    let targets = t.daemon.resolve(&g, &remotes, "alex/inbox@laptop", None).unwrap();
    assert!(
        matches!(targets.as_slice(), [Target::Remote { machine, online: false, .. }] if *machine == t.laptop.public())
    );
    drop(g);
    assert_eq!(t.daemon.anchor_peer(&remotes, &t.laptop.public()), Some(t.source.public()));
    let mut offline = remotes.clone();
    offline.get_mut(&t.source.public()).unwrap().online = false;
    assert!(t.daemon.anchor_peer(&offline, &t.laptop.public()).is_none());
    let foreign = HashMap::from([(t.stranger.public(), advertised(t.laptop.public(), &["inbox"]))]);
    let g = t.daemon.inner.lock().unwrap();
    assert!(t.daemon.resolve(&g, &foreign, "alex/inbox@laptop", None).is_err());
}

#[test]
fn direct_presence_wins_over_an_anchors_old_route_including_a_removed_mailbox() {
    let t = team(false);
    let mut remotes = HashMap::from([(t.source.public(), advertised(t.laptop.public(), &["inbox"]))]);
    remotes.insert(t.laptop.public(), Remote { presence: permanent(&[]), online: true, last_seen: util::now_ms() });
    let g = t.daemon.inner.lock().unwrap();
    assert!(t.daemon.resolve(&g, &remotes, "alex/inbox@laptop", None).is_err());
    drop(g);
    let mut p = permanent(&["inbox"]);
    let mut row = t.daemon.row("inbox", "cli", "/", None);
    row.tier = Tier::Pull;
    p.sessions.push(row);
    remotes.get_mut(&t.laptop.public()).unwrap().presence = p;
    let g = t.daemon.inner.lock().unwrap();
    let targets = t.daemon.resolve(&g, &remotes, "alex/inbox@laptop", None).unwrap();
    assert!(
        matches!(targets.as_slice(), [Target::Remote { machine, online: true, .. }] if *machine == t.laptop.public())
    );
}

#[test]
fn routes_to_a_teammates_or_revoked_machine_are_not_discovered() {
    let t = team(false);
    let mut remotes = HashMap::from([(t.source.public(), advertised(t.stranger.public(), &["inbox"]))]);
    {
        let g = t.daemon.inner.lock().unwrap();
        assert!(t.daemon.learned_routes(&g, &remotes).is_empty());
    }
    remotes.get_mut(&t.source.public()).unwrap().presence.anchor_routes[0].machine = t.laptop.public();
    t.daemon.inner.lock().unwrap().team.as_mut().unwrap().put_member(
        &t.daemon.key,
        t.laptop.public(),
        "alex",
        "laptop",
        false,
    );
    assert!(t.daemon.learned_routes(&t.daemon.inner.lock().unwrap(), &remotes).is_empty());
    assert!(t.daemon.anchor_peer(&remotes, &t.laptop.public()).is_none());
}

#[test]
fn an_unknown_mailbox_is_refused_and_the_senders_direct_copy_survives() {
    let t = team(true);
    let msg = message();
    t.daemon.inner.lock().unwrap().store.outbox_add(&t.laptop.public().to_string(), &msg).unwrap();
    let error = t.daemon.accept_anchor(t.source.public(), t.laptop.public(), msg.clone()).unwrap_err();
    assert!(error.to_string().contains("unknown permanent mailbox"), "{error}");
    let pending = t.daemon.outbox_for(&t.laptop.public());
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, msg.id);
}

#[test]
fn recipient_acknowledgments_are_bound_to_the_target_and_final_delivery_state() {
    let t = team(true);
    learn(&t);
    let msg = message();
    t.daemon.accept_anchor(t.source.public(), t.laptop.public(), msg.clone()).unwrap();
    t.daemon.forward_done(&t.source.public(), &msg.id, "queued", "").unwrap();
    for state in ["retry", "stored", "unknown"] {
        t.daemon.forward_done(&t.laptop.public(), &msg.id, state, "").unwrap();
    }
    assert_eq!(t.daemon.inner.lock().unwrap().store.anchor_pending().unwrap().len(), 1);
    t.daemon.forward_done(&t.laptop.public(), &msg.id, "queued", "saved").unwrap();
    assert!(t.daemon.inner.lock().unwrap().store.anchor_pending().unwrap().is_empty());
    t.daemon.accept_anchor(t.source.public(), t.laptop.public(), msg).unwrap();
    assert!(
        t.daemon.inner.lock().unwrap().store.anchor_pending().unwrap().is_empty(),
        "a lost acceptance ack cannot resurrect completed mail"
    );
}

#[test]
fn refusal_releases_the_payload_with_one_owner_visible_notice() {
    let t = team(true);
    learn(&t);
    let msg = message();
    t.daemon.accept_anchor(t.source.public(), t.laptop.public(), msg.clone()).unwrap();
    for _ in 0..2 {
        t.daemon.forward_done(&t.laptop.public(), &msg.id, "refused", "mailbox removed").unwrap();
    }
    let g = t.daemon.inner.lock().unwrap();
    assert!(g.store.anchor_pending().unwrap().is_empty());
    let notices = g.store.unread(&t.daemon.addr(NOTICE_INBOX)).unwrap();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].reply_to.as_deref(), Some(msg.id.as_str()));
    assert!(notices[0].body.contains("mailbox removed"));
}

#[test]
fn forwards_preserve_the_source_terminal_machine_and_its_signed_approval() {
    let t = team(false);
    remember(&t.daemon, "inbox", true);
    let mut msg = message();
    let approval = Signed::new(
        Approval {
            about: "01REQUEST".into(),
            asker: "alex/inbox".into(),
            approver: "alex".into(),
            approved: true,
            question: "Continue?".into(),
            request: "Please continue.".into(),
            note: "yes".into(),
            at: util::now_ms(),
        },
        &t.source,
        "acme",
        "approval",
    );
    msg.approval = Some(Box::new(approval.clone()));
    let (state, _) = t.daemon.receive_forward(t.laptop.public(), t.source.public(), msg.clone()).unwrap();
    assert_eq!(state, "queued");
    let kept = t.daemon.inner.lock().unwrap().store.message(&msg.id).unwrap().unwrap();
    assert_eq!(kept.from, "alex/terminal@cloud");
    assert_eq!(kept.approval.unwrap().body, approval.body);
    assert!(!kept.body.contains("didn't verify"));
}

#[test]
fn forwards_cannot_impersonate_other_owners_or_target_an_ordinary_session() {
    let t = team(false);
    remember(&t.daemon, "inbox", true);
    remember(&t.daemon, "ordinary", false);
    let msg = message();
    assert!(t.daemon.receive_forward(t.stranger.public(), t.source.public(), msg.clone()).is_err());
    assert!(t.daemon.receive_forward(t.laptop.public(), t.stranger.public(), msg.clone()).is_err());
    assert!(
        t.daemon
            .receive_forward(
                t.laptop.public(),
                t.source.public(),
                Message { from: "bob/terminal".into(), ..msg.clone() }
            )
            .is_err()
    );
    let (state, _) = t
        .daemon
        .receive_forward(t.laptop.public(), t.source.public(), Message { to: "alex/ordinary".into(), ..msg })
        .unwrap();
    assert_eq!(state, "refused");
    assert!(t.daemon.inner.lock().unwrap().store.unread("alex/inbox").unwrap().is_empty());
}

#[test]
fn forwarded_messages_expire_at_seven_days() {
    let t = team(false);
    remember(&t.daemon, "inbox", true);
    let mut msg = message();
    msg.sent_at -= ANCHOR_TTL_MS;
    assert!(t.daemon.receive_forward(t.laptop.public(), t.source.public(), msg).is_err());
    assert!(t.daemon.inner.lock().unwrap().store.unread("alex/inbox").unwrap().is_empty());
}

#[test]
fn forwarded_messages_cannot_put_their_original_send_time_far_in_the_future() {
    let t = team(false);
    remember(&t.daemon, "inbox", true);
    let mut msg = message();
    msg.sent_at = i64::MAX;
    assert!(t.daemon.receive_forward(t.laptop.public(), t.source.public(), msg).is_err());
}

#[test]
fn forwarded_mail_respects_destination_hold_and_refuse_policies() {
    for policy in [Policy::Hold, Policy::Refuse] {
        let mut cfg = Config { person: Some("alex".into()), ..Default::default() };
        cfg.inbound.default = policy;
        let t = configured(cfg);
        remember(&t.daemon, "inbox", true);
        let msg = message();
        let (state, _) = t.daemon.receive_forward(t.laptop.public(), t.source.public(), msg.clone()).unwrap();
        let g = t.daemon.inner.lock().unwrap();
        if policy == Policy::Hold {
            assert_eq!(state, "held");
            assert_eq!(g.store.held().unwrap()[0].id, msg.id);
        } else {
            assert_eq!(state, "refused");
            assert!(!g.store.has(&msg.id).unwrap());
        }
        assert!(g.store.unread("alex/inbox").unwrap().is_empty());
    }
}

#[test]
fn direct_and_forwarded_delivery_save_one_message_and_one_set_of_files() {
    let t = team(false);
    let inbox = std::env::temp_dir().join(format!("reason-network-forward-{}", ulid::Ulid::generate()));
    t.daemon.inner.lock().unwrap().store = Store::open(std::path::Path::new(":memory:"), inbox.clone()).unwrap();
    remember(&t.daemon, "inbox", true);
    let mut msg = message();
    msg.files.push(Attachment { name: "handoff.md".into(), content: "saved work".into(), path: None });
    t.daemon.receive(t.source.public(), msg.clone()).unwrap();
    t.daemon.receive_forward(t.laptop.public(), t.source.public(), msg.clone()).unwrap();
    let saved = t.daemon.inner.lock().unwrap().store.unread("alex/inbox").unwrap();
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].id, msg.id);
    assert_eq!(std::fs::read_to_string(saved[0].files[0].path.as_ref().unwrap()).unwrap(), "saved work");
    assert_eq!(std::fs::read_dir(&inbox).unwrap().count(), 1);
    let counts = t.daemon.inner.lock().unwrap().store.counts(util::now_ms()).unwrap();
    assert_eq!(counts.iter().map(|d| d.received).sum::<u64>(), 1);
    drop(t);
    std::fs::remove_dir_all(inbox).unwrap();
}

#[test]
fn mailbox_removal_discards_its_saved_route_and_pending_mail_with_a_notice() {
    let t = team(true);
    learn(&t);
    let msg = message();
    t.daemon.accept_anchor(t.source.public(), t.laptop.public(), msg.clone()).unwrap();
    t.daemon.learn_mailboxes(&t.laptop.public(), &permanent(&[])).unwrap();
    let g = t.daemon.inner.lock().unwrap();
    assert!(t.daemon.saved_mailboxes(&g).is_empty());
    assert!(g.store.anchor_pending().unwrap().is_empty());
    let notices = g.store.unread(&t.daemon.addr(NOTICE_INBOX)).unwrap();
    assert_eq!(notices.len(), 1);
    assert!(notices[0].body.contains("mailbox was removed"));
}

#[test]
fn losing_protocol_support_invalidates_routes_without_trusting_old_known_names() {
    let t = team(true);
    learn(&t);
    let old = Presence { known: vec!["inbox".into()], kept: vec!["inbox".into()], ..Default::default() };
    t.daemon.learn_mailboxes(&t.laptop.public(), &old).unwrap();
    assert!(t.daemon.saved_mailboxes(&t.daemon.inner.lock().unwrap()).is_empty());
    assert!(t.daemon.accept_anchor(t.source.public(), t.laptop.public(), message()).is_err());
}

#[test]
fn local_removal_drops_routes_and_held_mail() {
    let t = team(true);
    learn(&t);
    t.daemon.accept_anchor(t.source.public(), t.laptop.public(), message()).unwrap();
    t.daemon.remove("alex/laptop").unwrap();
    let g = t.daemon.inner.lock().unwrap();
    assert!(t.daemon.saved_mailboxes(&g).is_empty());
    assert!(g.store.anchor_pending().unwrap().is_empty());
    assert_eq!(g.store.unread(&t.daemon.addr(NOTICE_INBOX)).unwrap().len(), 1);
}

#[test]
fn merged_revocation_of_the_source_releases_its_mail_and_keeps_unrelated_routes() {
    let t = team(true);
    learn(&t);
    t.daemon.accept_anchor(t.source.public(), t.laptop.public(), message()).unwrap();
    let mut revoked = t.daemon.team_state().unwrap();
    revoked.put_member(&t.daemon.key, t.source.public(), "alex", "cloud", false);
    t.daemon.merge_state(&revoked);
    let g = t.daemon.inner.lock().unwrap();
    assert!(!g.team.as_ref().unwrap().is_active(&t.source.public()));
    assert!(g.store.anchor_pending().unwrap().is_empty());
    assert_eq!(t.daemon.saved_mailboxes(&g).len(), 1);
    assert_eq!(g.store.unread(&t.daemon.addr(NOTICE_INBOX)).unwrap().len(), 1);
}

/// A real database lets a separate connection fail just notice inserts, while roster writes still work.
fn fail_notices(t: &Team) -> (std::path::PathBuf, rusqlite::Connection) {
    let dir = std::env::temp_dir().join(format!("reason-network-anchor-failure-{}", ulid::Ulid::generate()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("network.db");
    let store = Store::open(&path, dir.join("inbox")).unwrap();
    let roster = t.daemon.team_state().unwrap();
    store.set("team", &serde_json::to_string(&roster).unwrap()).unwrap();
    t.daemon.inner.lock().unwrap().store = store;
    let db = rusqlite::Connection::open(path).unwrap();
    db.execute_batch(
        "CREATE TRIGGER fail_anchor_notice BEFORE INSERT ON messages BEGIN SELECT RAISE(FAIL, 'notice failed'); END;",
    )
    .unwrap();
    (dir, db)
}

#[test]
fn mailbox_removal_stays_cancelled_even_when_its_notice_cannot_be_saved() {
    let t = team(true);
    let (dir, db) = fail_notices(&t);
    learn(&t);
    t.daemon.accept_anchor(t.source.public(), t.laptop.public(), message()).unwrap();
    assert!(t.daemon.learn_mailboxes(&t.laptop.public(), &permanent(&[])).is_err());
    {
        let g = t.daemon.inner.lock().unwrap();
        assert!(t.daemon.saved_mailboxes(&g).is_empty());
        let pending = g.store.anchor_pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].cancelled, "a failed notice must not make removed mail deliverable again");
    }
    // Recreating the same name is a different mailbox lifetime; it cannot undo the earlier removal.
    let _ = t.daemon.learn_mailboxes(&t.laptop.public(), &permanent(&["inbox"]));
    assert!(t.daemon.inner.lock().unwrap().store.anchor_pending().unwrap()[0].cancelled);
    let cancelled: bool = db.query_row("SELECT cancelled FROM anchor_mail", [], |row| row.get(0)).unwrap();
    assert!(cancelled, "the cancellation survives an anchor crash before its drop notice is saved");
    db.execute_batch("DROP TRIGGER fail_anchor_notice").unwrap();
    t.daemon.learn_mailboxes(&t.laptop.public(), &permanent(&[])).unwrap();
    {
        let g = t.daemon.inner.lock().unwrap();
        assert!(t.daemon.saved_mailboxes(&g).is_empty());
        assert!(g.store.anchor_pending().unwrap().is_empty());
        assert_eq!(g.store.unread(&t.daemon.addr(NOTICE_INBOX)).unwrap().len(), 1);
    }
    drop(db);
    drop(t);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn failed_drop_notice_cannot_prevent_a_local_revocation_from_becoming_durable() {
    let t = team(true);
    let (dir, db) = fail_notices(&t);
    learn(&t);
    t.daemon.accept_anchor(t.source.public(), t.laptop.public(), message()).unwrap();
    t.daemon.remove("alex/laptop").unwrap();
    {
        let g = t.daemon.inner.lock().unwrap();
        assert!(!g.team.as_ref().unwrap().is_active(&t.laptop.public()));
        let persisted: TeamState = serde_json::from_str(&g.store.get("team").unwrap().unwrap()).unwrap();
        assert!(!persisted.is_active(&t.laptop.public()), "restarting cannot restore the revoked member");
        let pending = g.store.anchor_pending().unwrap();
        assert_eq!(pending.len(), 1, "the failed notice retains its payload for cleanup retry");
        assert!(pending[0].cancelled);
    }
    drop(db);
    drop(t);
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn failed_drop_notice_cannot_restore_a_team_after_leaving() {
    let t = team(true);
    let (dir, db) = fail_notices(&t);
    learn(&t);
    t.daemon.accept_anchor(t.source.public(), t.laptop.public(), message()).unwrap();
    t.daemon.leave(true).await.unwrap();
    // No further await: the single-thread runtime drops the scheduled process restart without running it.
    {
        let g = t.daemon.inner.lock().unwrap();
        assert!(g.team.is_none());
        assert!(g.store.get("team").unwrap().is_none(), "restarting cannot rejoin the old team");
        assert!(g.store.anchor_pending().unwrap()[0].cancelled);
    }
    drop(db);
    drop(t);
    std::fs::remove_dir_all(dir).unwrap();
}

fn register_terminal(d: &Arc<Daemon>, name: &str) {
    d.register(
        Some(1),
        SessionReg {
            key: "cli:terminal".into(),
            agent: "cli".into(),
            cwd: "/".into(),
            agent_session_id: None,
            pid: None,
            tier: Tier::Pull,
            name: Some(name.into()),
        },
        GitInfo::default(),
    )
    .unwrap();
}

fn register_other_mailbox(d: &Arc<Daemon>) {
    d.register(
        Some(2),
        SessionReg {
            key: "cli:other".into(),
            agent: "cli".into(),
            cwd: "/".into(),
            agent_session_id: None,
            pid: None,
            tier: Tier::Pull,
            name: Some("other".into()),
        },
        GitInfo::default(),
    )
    .unwrap();
}

#[tokio::test]
async fn notices_created_before_the_first_terminal_registration_reach_its_suffixed_name() {
    let t = team(true);
    learn(&t);
    let msg = message();
    t.daemon.accept_anchor(t.source.public(), t.laptop.public(), msg.clone()).unwrap();
    t.daemon.forward_done(&t.laptop.public(), &msg.id, "refused", "mailbox removed").unwrap();
    remember(&t.daemon, "terminal-2", true);
    {
        let g = t.daemon.inner.lock().unwrap();
        let mut r = g.store.session_by_name("terminal-2").unwrap().unwrap();
        r.key = "cli:terminal".into();
        g.store.upsert_session(&r, &t.daemon.person).unwrap();
    }
    register_terminal(&t.daemon, "terminal-2");
    register_other_mailbox(&t.daemon);
    assert_eq!(t.daemon.inner.lock().unwrap().live["cli:terminal"].name, "terminal-2");
    assert!(t.daemon.inbox("cli:other".into(), 0, false).await.unwrap().messages.is_empty());
    let inbox = t.daemon.inbox("cli:terminal".into(), 0, false).await.unwrap();
    assert_eq!(inbox.messages.len(), 1);
    assert_eq!(inbox.messages[0].reply_to.as_deref(), Some(msg.id.as_str()));
    assert!(inbox.text.contains("mailbox removed"));
    assert!(t.daemon.inbox("cli:terminal".into(), 0, false).await.unwrap().messages.is_empty());
}

#[tokio::test]
async fn notices_created_after_a_terminal_rename_are_read_once_only_by_that_terminal_key() {
    let t = team(true);
    learn(&t);
    register_terminal(&t.daemon, "terminal");
    t.daemon.set_status("cli:terminal", None, Some("operations".into())).unwrap();
    register_other_mailbox(&t.daemon);
    let msg = message();
    t.daemon.accept_anchor(t.source.public(), t.laptop.public(), msg.clone()).unwrap();
    t.daemon.forward_done(&t.laptop.public(), &msg.id, "refused", "destination removed").unwrap();
    assert_eq!(t.daemon.inner.lock().unwrap().live["cli:terminal"].name, "operations");
    assert!(t.daemon.inbox("cli:other".into(), 0, false).await.unwrap().messages.is_empty());
    let peeked = t.daemon.inbox("cli:terminal".into(), 0, true).await.unwrap();
    assert_eq!(peeked.messages.len(), 1);
    let inbox = t.daemon.inbox("cli:terminal".into(), 0, false).await.unwrap();
    assert_eq!(inbox.messages.len(), 1);
    assert_eq!(inbox.messages[0].id, peeked.messages[0].id);
    assert_eq!(inbox.messages[0].reply_to.as_deref(), Some(msg.id.as_str()));
    assert!(inbox.text.contains("destination removed"));
    assert!(t.daemon.inbox("cli:terminal".into(), 0, false).await.unwrap().messages.is_empty());
}
