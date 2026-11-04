use super::*;

#[tokio::test]
async fn anchor_frames_require_the_current_connections_matching_capability() {
    let endpoint = Endpoint::builder(presets::Minimal)
        .clear_ip_transports()
        .bind_addr("127.0.0.1:0")
        .unwrap()
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await
        .unwrap();
    let other_key = SecretKey::generate();
    let other = Endpoint::builder(presets::Minimal)
        .secret_key(other_key.clone())
        .clear_ip_transports()
        .bind_addr("127.0.0.1:0")
        .unwrap()
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await
        .unwrap();
    let peer = other.id();
    let address = EndpointAddr::new(peer).with_ip_addr(other.bound_sockets()[0]);
    let (conn, accepted) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(endpoint.connect(address, ALPN), async { other.accept().await.unwrap().await })
    })
    .await
    .unwrap();
    let conn = conn.unwrap();
    let accepted = accepted.unwrap();
    let daemon = crate::daemon::test_anchor_peer(&other_key);
    let net = Net::isolated(&daemon, endpoint.clone());
    let msg = Message {
        id: ulid::Ulid::generate().to_string(),
        from: "alex/terminal".into(),
        from_agent: Some("cli".into()),
        from_repo: None,
        to: "alex/inbox".into(),
        via: None,
        reply_to: None,
        body: "handoff".into(),
        wake: false,
        notify: false,
        sent_at: util::now_ms(),
        approval: None,
        files: Vec::new(),
    };
    for (feature, wrong_feature, frame) in [
        ("anchor", "anchor-mail", Wire::Anchor { target: peer, msg: msg.clone() }),
        ("anchor-mail", "anchor", Wire::Forward { source: net.me, msg: msg.clone() }),
    ] {
        let (tx, mut rx) = mpsc::unbounded_channel();
        net.links.lock().unwrap().insert(
            peer,
            Link { id: 1, tx, conn: conn.clone(), dialer: net.me, since: Instant::now(), presence_seen: false },
        );
        net.remote.lock().unwrap().insert(
            peer,
            Remote {
                presence: Presence { features: vec![feature.into()], ..Default::default() },
                last_seen: util::now_ms(),
                online: true,
            },
        );
        net.anchor_send(peer, &msg.id, frame.clone());
        assert!(matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)), "cached {feature} is insufficient");
        assert!(net.anchor_sent.lock().unwrap().is_empty());

        net.links.lock().unwrap().get_mut(&peer).unwrap().presence_seen = true;
        for features in [Vec::new(), vec![wrong_feature.into()]] {
            net.remote.lock().unwrap().get_mut(&peer).unwrap().presence.features = features;
            net.anchor_send(peer, &msg.id, frame.clone());
            assert!(matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)), "{feature} is required");
            assert!(net.anchor_sent.lock().unwrap().is_empty());
        }

        net.remote.lock().unwrap().get_mut(&peer).unwrap().presence.features = vec![feature.into()];
        net.anchor_send(peer, &msg.id, frame.clone());
        let sent = rx.try_recv().expect("the current connection advertised the matching capability");
        assert_eq!(serde_json::to_value(sent).unwrap(), serde_json::to_value(&frame).unwrap());
        if feature == "anchor" {
            let tx = net.links.lock().unwrap()[&peer].tx.clone();
            net.on_wire(
                peer,
                &tx,
                Wire::Anchored { target: peer, id: msg.id.clone(), state: "retry".into(), note: "full".into() },
            )
            .await
            .unwrap();
            for _ in 0..30 {
                net.anchor_send(peer, &msg.id, frame.clone());
            }
            assert!(
                matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
                "fast refusal must retain the retry guard"
            );
            net.anchor_sent.lock().unwrap().get_mut(&(peer, msg.id.clone())).unwrap().0 =
                Instant::now() - Duration::from_secs(31);
            net.anchor_send(peer, &msg.id, frame.clone());
            assert!(rx.try_recv().is_ok());
            assert_eq!(net.anchor_sent.lock().unwrap()[&(peer, msg.id.clone())].2, Duration::from_secs(60));
        }

        // A retry can become due before the replacement connection has announced its engine's capabilities.
        net.anchor_sent.lock().unwrap().get_mut(&(peer, msg.id.clone())).unwrap().0 =
            Instant::now() - Duration::from_secs(31);
        let (replacement, mut replacement_rx) = mpsc::unbounded_channel();
        net.links.lock().unwrap().insert(
            peer,
            Link {
                id: 2,
                tx: replacement,
                conn: conn.clone(),
                dialer: net.me,
                since: Instant::now(),
                presence_seen: false,
            },
        );
        net.anchor_send(peer, &msg.id, frame);
        assert!(
            matches!(replacement_rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
            "a replacement connection cannot inherit the old connection's {feature} capability"
        );
        net.anchor_sent.lock().unwrap().clear();
    }
    let presence = Wire::Presence {
        presence: Presence {
            features: vec!["anchor".into()],
            mailboxes: vec!["inbox".into()],
            anchor_routes: vec![MailboxRoute { machine: peer, names: vec!["inbox".into()] }],
            ..Default::default()
        },
    };
    let team = daemon.team_state().unwrap();
    let stranger = team.active().find(|m| m.person == "bob").unwrap().endpoint;
    let Wire::Presence { presence: private } = net.for_peer(&presence, &stranger) else { panic!() };
    assert!(!private.has("anchor") && private.mailboxes.is_empty() && private.anchor_routes.is_empty());
    let Wire::Presence { presence: own } = net.for_peer(&presence, &peer) else { panic!() };
    assert!(own.has("anchor") && !own.anchor_routes.is_empty());
    conn.close(0u32.into(), b"test complete");
    accepted.close(0u32.into(), b"test complete");
    endpoint.close().await;
    other.close().await;
}

#[test]
fn mailbox_advertisements_are_bounded_before_deduplication() {
    let names: Vec<_> = (0..10000).map(|n| format!("inbox-{n}")).collect();
    let p = tidy_presence(Presence {
        mailboxes: names.clone(),
        anchor_routes: vec![MailboxRoute { machine: SecretKey::generate().public(), names }],
        ..Default::default()
    });
    assert_eq!(p.mailboxes.len(), 100);
    assert_eq!(p.anchor_routes[0].names.len(), 100);
}
