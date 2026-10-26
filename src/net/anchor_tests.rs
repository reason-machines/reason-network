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
    let other = Endpoint::builder(presets::Minimal)
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
    let net = Net {
        me: endpoint.id(),
        udp: UdpPort { port: endpoint.bound_sockets()[0].port(), fixed: None, fell_back: false },
        endpoint: endpoint.clone(),
        relays: Vec::new(),
        proxy: None,
        at: None,
        router: Mutex::new(None),
        invites: Default::default(),
        daemon: Weak::new(),
        links: Default::default(),
        remote: Default::default(),
        acks: Default::default(),
        dialing: Default::default(),
        wake: Notify::new(),
        changed: Notify::new(),
        next_link: AtomicU64::new(1),
        anchor_sent: Default::default(),
    };
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
    conn.close(0u32.into(), b"test complete");
    accepted.close(0u32.into(), b"test complete");
    endpoint.close().await;
    other.close().await;
}
