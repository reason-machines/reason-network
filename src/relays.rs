//! The relays of a team that doesn't run its own: Reason's, one per region, as Reason lists them at [`URL`]. Each
//! machine makes the nearest its home relay, and is reached there by teammates on any of them (relays don't talk to
//! each other; iroh dials a peer through the peer's home relay).
//!
//! The list steers every such machine's relay traffic. That traffic is end-to-end encrypted, but a list of someone
//! else's relays could cut machines off or show who talks to whom, so it is taken only signed with Reason's proof key
//! (`typ` `reason-network-relays`, its own `iss`: neither a sign-in proof nor anything else signed with that key passes
//! for one) and fetched only over HTTPS from [`URL`], following no redirect. Any fault rejects the whole list, and so
//! does one issued before the list this machine has (no rolling back to an older one).
//!
//! The last good list is kept in the state folder (`relays.json`, the document as served) and used from then on, even
//! once it has expired, until a newer one comes. Without one, [`net::DEFAULT_RELAYS`](crate::net::DEFAULT_RELAYS).
//! The daemon fetches it at start and every 6 hours; a new list applies when the network next starts (a restart, a
//! join, a relay change), not to the running endpoint.

use anyhow::{Result, anyhow, bail};
use iroh::RelayUrl;
use serde::Deserialize;
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

pub const URL: &str = "https://reasonmachines.com/network/relays.json";
const TYPE: &str = "reason-network-relays";
const ISSUER: &str = "https://reasonmachines.com/network/relays";
const AUDIENCE: &str = "reason-network";
/// Refused beyond this, served or cached.
const MAX_BYTES: usize = 64 << 10;
const MAX_RELAYS: usize = 16;
/// A list fetched this long after it expired is still taken (clocks differ).
const LEEWAY_SECS: i64 = 300;
/// A list issued further ahead than this is refused (a broken clock, or worse): it would hold off every real one.
const MAX_SKEW_SECS: i64 = 86_400;
pub const EVERY: Duration = Duration::from_secs(6 * 3600);
/// How long a machine's first network start waits for the list. A quick answer (a list, or a refusal) is taken at
/// once; a slow fetch carries on in the background, for the next start.
const FIRST_WAIT: Duration = Duration::from_secs(3);

/// What is served: the list for people to read (ignored here), and the same list signed, which is what counts.
#[derive(Deserialize)]
struct Served {
    signed: String,
}

#[derive(Deserialize)]
struct Claims {
    iss: String,
    aud: String,
    iat: i64,
    exp: i64,
    relays: Vec<Entry>,
}

#[derive(Deserialize)]
struct Entry {
    url: String,
}

/// A checked list.
#[derive(Debug)]
struct List {
    iat: i64,
    relays: Vec<RelayUrl>,
}

/// The list in a served (or cached) document, checked as of `now` (seconds); one `fetched` just now mustn't have
/// expired either.
fn parse(doc: &[u8], now: i64, fetched: bool) -> Result<List> {
    if doc.len() > MAX_BYTES {
        bail!("the relay list is over {MAX_BYTES} bytes");
    }
    let served: Served = serde_json::from_slice(doc).map_err(|e| anyhow!("the relay list isn't one: {e}"))?;
    let claims = crate::proof::verify_signed(&served.signed, TYPE, "relay list", MAX_BYTES)?;
    let c: Claims = serde_json::from_slice(&claims).map_err(|e| anyhow!("the relay list is damaged: {e}"))?;
    if c.iss != ISSUER || c.aud != AUDIENCE {
        bail!("the relay list isn't Reason Network's");
    }
    if c.iat > now.saturating_add(MAX_SKEW_SECS) {
        bail!("the relay list is dated in the future (is this computer's clock right?)");
    }
    if fetched && c.exp.saturating_add(LEEWAY_SECS) <= now {
        bail!("the relay list expired");
    }
    if c.relays.is_empty() || c.relays.len() > MAX_RELAYS {
        bail!("the relay list has {} relays (1 to {MAX_RELAYS} expected)", c.relays.len());
    }
    let mut relays: Vec<RelayUrl> = Vec::new();
    for e in c.relays {
        let url: RelayUrl = e.url.parse().map_err(|_| anyhow!("not a relay url: {}", e.url))?;
        if url.scheme() != "https" || url.host_str().is_none() {
            bail!("not an https relay: {url}");
        }
        if relays.contains(&url) {
            bail!("{url} is in the relay list twice");
        }
        relays.push(url);
    }
    Ok(List { iat: c.iat, relays })
}

fn now() -> i64 {
    crate::util::now_ms() / 1000
}

fn cache(home: &Path) -> PathBuf {
    home.join("relays.json")
}

/// The cached list, if there is a good one (expired or not).
fn cached(home: &Path) -> Option<List> {
    parse(&std::fs::read(cache(home)).ok()?, now(), false).ok()
}

/// The relays to use: the cached list, else the built-in default.
pub fn current() -> Vec<RelayUrl> {
    current_in(&crate::paths::home())
}

fn current_in(home: &Path) -> Vec<RelayUrl> {
    cached(home).map_or_else(crate::net::default_relays, |l| l.relays)
}

/// The same, fetching the list first if none is cached yet (a machine's first network start), for at most
/// `FIRST_WAIT`.
pub async fn current_or_fetch() -> Vec<RelayUrl> {
    current_or_fetch_in(crate::paths::home(), url()).await
}

async fn current_or_fetch_in(home: PathBuf, url: String) -> Vec<RelayUrl> {
    if cached(&home).is_none() {
        let h = home.clone();
        let fetching = tokio::spawn(async move {
            if let Err(e) = refresh_in(&h, &url).await {
                log(&format!("{e:#}; using the default relay"));
            }
        });
        if tokio::time::timeout(FIRST_WAIT, fetching).await.is_err() {
            log("the relay list is slow to come: the default relay until the next start");
        }
    }
    current_in(&home)
}

/// Fetches the list and, if good, keeps it; says why not otherwise.
pub async fn refresh() {
    if let Err(e) = refresh_in(&crate::paths::home(), &url()).await {
        log(&format!("{e:#}; keeping the relays this machine has"));
    }
}

fn log(msg: &str) {
    eprintln!("[{}] relays: {msg}", crate::util::now_ms());
    crate::debuglog::ours("relays", msg);
}

async fn refresh_in(home: &Path, url: &str) -> Result<()> {
    let doc = fetch(url).await?;
    let list = parse(&doc, now(), true)?;
    if let Some(had) = cached(home)
        && list.iat < had.iat
    {
        bail!("the relay list is older than the one this machine has");
    }
    if std::fs::read(cache(home)).ok().as_deref() == Some(doc.as_slice()) {
        return Ok(());
    }
    let tmp = home.join(format!("relays.json.{}.tmp", ulid::Ulid::generate()));
    std::fs::write(&tmp, &doc)?;
    std::fs::rename(&tmp, cache(home))?;
    Ok(())
}

/// [`URL`]; in debug builds, `REASON_NETWORK_RELAYS_URL` instead, for tests (release builds only ever fetch `URL`).
fn url() -> String {
    #[cfg(debug_assertions)]
    if let Ok(u) = crate::paths::env_var("RELAYS_URL") {
        return u;
    }
    URL.to_string()
}

/// How curl fetches `url`: ignoring any `.curlrc` (`-q`, which only counts first), over HTTPS only (or plain HTTP
/// for a debug build's test URL), following no redirect, the status after the body.
fn curl_args(url: &str) -> Vec<String> {
    let proto = if cfg!(debug_assertions) && url != URL && !url.starts_with("https:") { "=http" } else { "=https" };
    ["-q", "-fsS", "--proto", proto, "--max-redirs", "0", "--max-time", "30", "-w", "%{response_code}", url]
        .map(String::from)
        .to_vec()
}

/// The body at `url`, if a 200 of at most `MAX_BYTES`: curl is stopped as soon as more comes, however it is sent.
async fn fetch(url: &str) -> Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let mut curl = tokio::process::Command::new("curl")
        .args(curl_args(url))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let stdout = curl.stdout.take().expect("piped");
    // The body and the 3-digit status, and a byte more to tell if there's too much.
    let mut body = Vec::new();
    let mut capped = stdout.take((MAX_BYTES + 4) as u64);
    tokio::time::timeout(Duration::from_secs(40), capped.read_to_end(&mut body)).await??;
    if body.len() > MAX_BYTES + 3 {
        bail!("fetching {url}: stopped past {MAX_BYTES} bytes");
    }
    let out = tokio::time::timeout(Duration::from_secs(10), curl.wait_with_output()).await??;
    if !out.status.success() {
        bail!("fetching {url}: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    // Anything but 200 (a redirect, which isn't followed) is no list.
    let code = body.split_off(body.len().saturating_sub(3));
    if code != b"200" {
        bail!("fetching {url}: HTTP {}", String::from_utf8_lossy(&code));
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proof::tests::{KID, forger, sign_with, signer};
    use serde_json::{Value, json};

    fn region(r: &str) -> String {
        format!("https://relay-{r}.compute.reasonmachines.com")
    }

    fn claims(urls: &[String]) -> Value {
        let relays: Vec<Value> = urls.iter().map(|u| json!({"url": u, "region": "x"})).collect();
        json!({"iss": ISSUER, "aud": AUDIENCE, "relays": relays, "iat": now(), "exp": now() + 30 * 86_400})
    }

    fn doc_with(header: Value, claims: Value) -> Vec<u8> {
        let signed = sign_with(signer(), header, claims.clone());
        serde_json::to_vec(&json!({"relays": claims["relays"], "signed": signed, "note": "extra fields are fine"}))
            .unwrap()
    }

    fn doc(claims: Value) -> Vec<u8> {
        doc_with(json!({"alg": "RS256", "kid": KID, "typ": TYPE}), claims)
    }

    fn four() -> Vec<String> {
        ["sfo1", "iad1", "fra1", "sin1"].map(region).to_vec()
    }

    fn home() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("reason-network-relays-{}", ulid::Ulid::generate()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_signed_list_gives_its_relays_in_order() {
        let got = parse(&doc(claims(&four())), now(), true).unwrap().relays;
        assert_eq!(
            got.iter().map(|u| u.to_string()).collect::<Vec<_>>(),
            four().iter().map(|u| format!("{u}/")).collect::<Vec<_>>()
        );
    }

    #[test]
    fn any_fault_rejects_the_whole_list() {
        let bad = |d: Vec<u8>| parse(&d, now(), true).unwrap_err().to_string();
        let with = |f: &dyn Fn(&mut Value)| {
            let mut c = claims(&four());
            f(&mut c);
            doc(c)
        };
        let mut http = four();
        http[2] = "http://relay-fra1.compute.reasonmachines.com".into();
        assert!(bad(doc(claims(&http))).contains("not an https relay"));
        assert!(bad(doc(claims(&["not a url".into()]))).contains("not a relay url"));
        assert!(bad(doc(claims(&[]))).contains("has 0 relays"));
        let many: Vec<String> = (0..17).map(|i| region(&format!("r{i}"))).collect();
        assert!(bad(doc(claims(&many))).contains("has 17 relays"));
        assert!(parse(&doc(claims(&many[..16])), now(), true).is_ok(), "16 is fine");
        let dup = [region("sfo1"), region("iad1"), format!("{}/", region("sfo1"))];
        assert!(bad(doc(claims(&dup))).contains("twice"));
        assert!(bad(b"{not json".to_vec()).contains("isn't one"));
        assert!(bad(br#"{"relays":[{"url":"https://relay.example.com"}]}"#.to_vec()).contains("isn't one"), "unsigned");
        let mut big = doc(claims(&four()));
        big.splice(1..1, std::iter::repeat_n(b' ', MAX_BYTES));
        assert!(bad(big).contains("over"));
        assert!(bad(with(&|c| c["exp"] = json!(now() - LEEWAY_SECS - 1))).contains("expired"));
        assert!(bad(with(&|c| c["iat"] = json!(now() + MAX_SKEW_SECS + 60))).contains("in the future"));
        assert!(bad(with(&|c| c.as_object_mut().unwrap().remove("iat").map(drop).unwrap())).contains("damaged"));
        assert!(
            bad(with(&|c| c["iss"] = json!("https://reasonmachines.com/network"))).contains("isn't Reason Network's")
        );
        assert!(bad(with(&|c| c["aud"] = json!("other"))).contains("isn't Reason Network's"));
        // Signed by someone else, or the signed part swapped for another.
        let forged = sign_with(forger(), json!({"alg": "RS256", "kid": KID, "typ": TYPE}), claims(&four()));
        assert!(bad(serde_json::to_vec(&json!({"signed": forged})).unwrap()).contains("doesn't verify"));
    }

    #[test]
    fn a_sign_in_proof_is_no_relay_list_nor_the_other_way_round() {
        let me = iroh::SecretKey::generate().public();
        let proof = crate::proof::tests::proof("u_1", "alex", &me);
        let as_list = serde_json::to_vec(&json!({"signed": proof})).unwrap();
        assert!(parse(&as_list, now(), true).unwrap_err().to_string().contains("not a Reason Network relay list"));
        // A relay list's signed part isn't a proof, even with a proof's claims in it; and those claims (the proof's
        // issuer) make no relay list.
        let mut c = crate::proof::tests::claims("u_1", "alex", &me);
        c["relays"] = json!([{"url": region("sfo1")}]);
        let list = sign_with(signer(), json!({"alg": "RS256", "kid": KID, "typ": TYPE}), c);
        let err = crate::proof::verify(&list, &me).unwrap_err().to_string();
        assert!(err.contains("not a Reason Network sign-in proof"), "{err}");
        let err = parse(&serde_json::to_vec(&json!({"signed": list})).unwrap(), now(), true).unwrap_err();
        assert!(err.to_string().contains("isn't Reason Network's"), "{err}");
        // Nor does a list signed as a proof pass as one.
        let typ_proof = doc_with(json!({"alg": "RS256", "kid": KID, "typ": "reason-network-proof"}), claims(&four()));
        assert!(parse(&typ_proof, now(), true).unwrap_err().to_string().contains("not a Reason Network relay list"));
    }

    #[test]
    fn the_cached_list_is_used_even_expired_and_the_default_without_one() {
        let h = home();
        assert_eq!(current_in(&h), crate::net::default_relays(), "nothing cached: the built-in default");
        let mut c = claims(&four());
        c["exp"] = json!(now() - 86_400);
        std::fs::write(cache(&h), doc(c)).unwrap();
        assert_eq!(current_in(&h).len(), 4, "expired, but the last good one");
        std::fs::write(cache(&h), b"garbage").unwrap();
        assert_eq!(current_in(&h), crate::net::default_relays(), "a damaged cache counts for nothing");
        let _ = std::fs::remove_dir_all(&h);
    }

    /// Serves `responses` in turn on a local port, one connection each, as HTTP/1.1 (a status of `chunked` sends a
    /// 200 in chunks, with no length); its URL.
    async fn serve(responses: Vec<(&'static str, Vec<u8>)>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/network/relays.json", l.local_addr().unwrap());
        tokio::spawn(async move {
            for (status, body) in responses {
                let (mut s, _) = l.accept().await.unwrap();
                let _ = s.read(&mut [0; 4096]).await;
                if status == "chunked" {
                    let _ = s.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n").await;
                    for chunk in body.chunks(8192) {
                        let framed = [format!("{:x}\r\n", chunk.len()).as_bytes(), chunk, b"\r\n"].concat();
                        if s.write_all(&framed).await.is_err() {
                            break;
                        }
                    }
                    let _ = s.write_all(b"0\r\n\r\n").await;
                    continue;
                }
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nLocation: https://example.com/\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = s.write_all(&[head.as_bytes(), &body].concat()).await;
            }
        });
        url
    }

    #[tokio::test]
    async fn fetches_keeps_and_falls_back() {
        let h = home();
        let good = doc(claims(&four()));
        let mut only_fra = claims(&[region("fra1")]);
        only_fra["exp"] = json!(now() - 3600);
        let url = serve(vec![
            ("200 OK", good.clone()),
            ("500 Oops", Vec::new()),
            ("301 Moved", good.clone()),
            ("200 OK", doc(claims(&[region("sfo1"), "http://relay.example.com".into()]))),
            ("200 OK", doc(only_fra)),
            ("chunked", vec![b' '; 4 << 20]),
            ("chunked", good.clone()),
        ])
        .await;
        refresh_in(&h, &url).await.unwrap();
        assert_eq!(std::fs::read(cache(&h)).unwrap(), good, "kept as served");
        assert_eq!(current_in(&h).len(), 4);
        assert!(refresh_in(&h, &url).await.is_err(), "a server error");
        assert!(refresh_in(&h, &url).await.unwrap_err().to_string().contains("HTTP 301"), "no redirect followed");
        assert!(refresh_in(&h, &url).await.unwrap_err().to_string().contains("https"), "a bad list");
        assert!(refresh_in(&h, &url).await.unwrap_err().to_string().contains("expired"), "an expired one, fresh");
        let err = refresh_in(&h, &url).await.unwrap_err().to_string();
        assert!(err.contains("stopped past 65536 bytes"), "too much, with no length to tell beforehand: {err}");
        refresh_in(&h, &url).await.unwrap();
        assert!(refresh_in(&h, &url).await.is_err(), "nobody there");
        assert_eq!(std::fs::read(cache(&h)).unwrap(), good, "the last good list stays");
        assert_eq!(std::fs::read_dir(&h).unwrap().count(), 1, "nothing but the list left behind");
        let _ = std::fs::remove_dir_all(&h);
    }

    #[tokio::test]
    async fn an_older_list_never_replaces_a_newer_one() {
        let h = home();
        let (mut newer, mut older) = (claims(&four()), claims(&[region("sfo1")]));
        newer["iat"] = json!(now());
        older["iat"] = json!(now() - 20 * 86_400);
        let url = serve(vec![("200 OK", doc(newer.clone())), ("200 OK", doc(older)), ("200 OK", doc(newer))]).await;
        refresh_in(&h, &url).await.unwrap();
        let err = refresh_in(&h, &url).await.unwrap_err().to_string();
        assert!(err.contains("older than the one this machine has"), "{err}");
        assert_eq!(current_in(&h).len(), 4);
        refresh_in(&h, &url).await.unwrap();
        let _ = std::fs::remove_dir_all(&h);
    }

    #[test]
    fn curl_reads_no_curlrc_and_only_https() {
        let args = curl_args(URL);
        assert_eq!(args[0], "-q", "only first does it count");
        assert!(args.windows(2).any(|w| w == ["--proto", "=https"]));
        assert!(args.windows(2).any(|w| w == ["--max-redirs", "0"]));
        assert!(!args.iter().any(|a| a == "-L" || a == "--location"));
    }

    #[tokio::test]
    async fn the_first_start_does_not_wait_on_a_failed_fetch() {
        let h = home();
        // A server that says no: the default, at once.
        let url = serve(vec![("503 Unavailable", Vec::new())]).await;
        let start = std::time::Instant::now();
        assert_eq!(current_or_fetch_in(h.clone(), url).await, crate::net::default_relays());
        assert!(start.elapsed() < Duration::from_secs(1), "{:?}", start.elapsed());
        // Nobody there: the default, within the wait (Windows retries a refused connection for about 2 s).
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let start = std::time::Instant::now();
        let got = current_or_fetch_in(h.clone(), format!("http://127.0.0.1:{port}/relays.json")).await;
        assert!(start.elapsed() < FIRST_WAIT + Duration::from_secs(1), "{:?}", start.elapsed());
        assert_eq!(got, crate::net::default_relays());
        // A server that answers with nothing for long: the default, after a short wait.
        let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/relays.json", silent.local_addr().unwrap());
        let start = std::time::Instant::now();
        assert_eq!(current_or_fetch_in(h.clone(), url).await, crate::net::default_relays());
        assert!(start.elapsed() < FIRST_WAIT + Duration::from_secs(1), "{:?}", start.elapsed());
        let _ = std::fs::remove_dir_all(&h);
    }
}
