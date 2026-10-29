//! Optional aggregate analytics. The messaging path only try_sends bounded events;
//! a separate thread owns its own SQLite database. No network, message text, files,
//! addresses, repository names or peer identities enter this module.
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, SyncSender},
    },
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct Dimensions {
    pub source: String,
    pub target: String,
    pub locality: String,
    pub origin: String,
    pub relationship: String,
    pub transport: String,
    pub bucket: String,
}
impl Default for Dimensions {
    fn default() -> Self {
        Self {
            source: "unknown".into(),
            target: "unknown".into(),
            locality: "unknown".into(),
            origin: "unknown".into(),
            relationship: "unknown".into(),
            transport: "unknown".into(),
            bucket: "none".into(),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Row {
    pub metric: String,
    #[serde(flatten)]
    pub dimensions: Dimensions,
    pub value: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Day {
    pub day: String,
    pub rows: Vec<Row>,
    pub dropped: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub version: u8,
    pub generation: String,
    #[serde(rename = "engineVersion")]
    pub engine_version: String,
    pub platform: String,
    pub days: Vec<Day>,
}
// IDs exist only in this bounded in-memory correlation map, never in SQLite/reports.
enum Event {
    Count(&'static str, Dimensions, u64),
    Start(String, Dimensions, Option<String>, Instant),
    Stage(String, &'static str, Instant),
}
#[derive(Default)]
pub struct Usage {
    tx: Option<SyncSender<Event>>,
    report: Arc<Mutex<Option<Report>>>,
    dropped: Arc<AtomicU64>,
}

pub fn family(name: &str) -> String {
    let name = name.to_ascii_lowercase();
    if name == "unknown" || name.is_empty() {
        return "unknown".into();
    }
    let result = if name.contains("chatgpt") {
        "chatgpt"
    } else if name.contains("muse") {
        "muse"
    } else if name.contains("hermes") {
        "hermes"
    } else if name.contains("antigravity") {
        "antigravity"
    } else if name == "cli" {
        "cli"
    } else {
        crate::agent::from_client_name(&name).unwrap_or("other")
    };
    if matches!(
        result,
        "claude-code"
            | "claude-desktop"
            | "codex"
            | "cursor"
            | "gemini"
            | "grok"
            | "muse"
            | "chatgpt"
            | "opencode"
            | "copilot"
            | "pi"
            | "reason"
            | "hermes"
            | "antigravity"
            | "devin"
            | "amp"
            | "zed"
            | "dsh"
            | "zcode"
            | "cli"
    ) {
        result.into()
    } else {
        "other".into()
    }
}
pub fn duration(ms: u64) -> &'static str {
    if ms < 100 {
        "lt100"
    } else if ms < 1000 {
        "lt1000"
    } else if ms < 10000 {
        "lt10000"
    } else if ms < 60000 {
        "lt60000"
    } else if ms < 600000 {
        "lt600000"
    } else {
        "gte600000"
    }
}
pub fn size(bytes: usize) -> &'static str {
    if bytes < 1024 {
        "lt1024"
    } else if bytes < 16384 {
        "lt16384"
    } else if bytes < 65536 {
        "lt65536"
    } else {
        "gte65536"
    }
}
pub fn number(n: usize) -> &'static str {
    match n {
        0 => "0",
        1 => "1",
        2 => "2",
        3..=5 => "3-5",
        6..=10 => "6-10",
        _ => "11+",
    }
}
impl Usage {
    pub fn open(path: PathBuf) -> Self {
        let (tx, rx) = mpsc::sync_channel(2048);
        let report = Arc::new(Mutex::new(None));
        let snapshot = report.clone();
        let dropped = Arc::new(AtomicU64::new(0));
        let lost = dropped.clone();
        // Failure disables analytics, never the engine. Own database avoids message-store locks.
        if std::thread::Builder::new().name("network-usage".into()).spawn(move || {
            let run = || -> anyhow::Result<()> {
                let mut db = Connection::open(&path)?;
                #[cfg(unix)] crate::util::chmod(&path, 0o600)?;
                db.busy_timeout(Duration::from_millis(100))?;
                db.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE IF NOT EXISTS meta (id INTEGER PRIMARY KEY CHECK(id=1), generation TEXT NOT NULL); CREATE TABLE IF NOT EXISTS counters(day TEXT NOT NULL, metric TEXT NOT NULL, dimensions TEXT NOT NULL, value INTEGER NOT NULL, PRIMARY KEY(day,metric,dimensions));")?;
                db.execute("INSERT OR IGNORE INTO meta VALUES(1,?1)", [ulid::Ulid::generate().to_string()])?;
                let generation: String = db.query_row("SELECT generation FROM meta WHERE id=1", [], |r| r.get(0))?;
                let mut pending: HashMap<String,(Dimensions,Instant,bool,bool,bool,usize)> = HashMap::new();
                let mut counts: BTreeMap<(String,String,Dimensions),u64> = BTreeMap::new();
                let mut persisted = db.prepare("SELECT day,metric,dimensions,value FROM counters WHERE day >= date('now','-7 days')")?;
                for row in persisted.query_map([], |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,i64>(3)?.max(0) as u64)))? {
                    let (day,metric,dims,value)=row?;
                    if let Ok(dims) = serde_json::from_str(&dims) {counts.insert((day,metric,dims),value);}
                }
                drop(persisted);
                let mut last_flush = Instant::now();
                let mut dirty = false;
                let mut last_day = String::new();
                loop {
                    let event = rx.recv_timeout(Duration::from_secs(1));
                    let day: String = db.query_row("SELECT date('now')", [], |r| r.get(0))?;
                    let mut add = |metric:&str, dims:Dimensions, value:u64| {
                        let key=(day.clone(),metric.to_owned(),dims);
                        let key=if counts.contains_key(&key) || counts.keys().filter(|(d,_,_)| d==&day).count()<256 {key}
                            else {(day.clone(),"dropped".into(),Dimensions::default())};
                        dirty = true;
                        let n=counts.entry(key).or_default(); *n=n.saturating_add(value).min(100_000_000);
                    };
                    let overflow = lost.swap(0, Ordering::Relaxed);
                    if overflow > 0 { add("dropped", Dimensions::default(), overflow); }
                    match event {
                        Ok(Event::Count(metric,dims,n))=>add(metric,dims,n),
                        Ok(Event::Start(id,dims,reply,at))=> {
                            let depth = reply.as_ref().and_then(|id|pending.get(id)).map_or(if reply.is_some() { 0 } else { 1 }, |p|if p.5 == 0 {0} else {p.5.saturating_add(1)});
                            if let Some(parent)=reply.as_ref().and_then(|id|pending.get(id)).filter(|p|p.2) {
                                let mut reply_dims=dims.clone(); reply_dims.bucket=duration(at.saturating_duration_since(parent.1).as_millis() as u64).into();
                                add("reply_ms",reply_dims,1);
                            }
                            if pending.len() >= 2048 {pending.retain(|_,v|v.1.elapsed()<Duration::from_secs(60));}
                            if pending.len()<2048 {pending.insert(id,(dims,at,false,false,reply.is_some(),depth));} else {add("dropped",Dimensions::default(),1);}
                        },
                        Ok(Event::Stage(id,stage,observed))=>if let Some((dims,at,accepted,delivered,is_reply,depth))=pending.get_mut(&id) {
                            let duplicate=match stage {"accepted"=>std::mem::replace(accepted,true),"delivered"=>std::mem::replace(delivered,true),_=>false};
                            if !duplicate {
                                add(stage,dims.clone(),1);
                                if stage == "accepted" {
                                    if *is_reply { add("reply",dims.clone(),1); }
                                    let mut exchange = dims.clone(); exchange.bucket = if *depth == 0 { "unknown" } else { number(*depth) }.into();
                                    add("exchange_length",exchange,1);
                                }
                                if matches!(stage,"accepted"|"delivered") {
                                    let mut d=dims.clone(); d.bucket=duration(observed.saturating_duration_since(*at).as_millis() as u64).into();
                                    add(if stage=="accepted" {"delivery_ms"} else {"queue_age_ms"},d,1);
                                }
                            }
                        } else { add(stage, Dimensions::default(), 1); },
                        Err(mpsc::RecvTimeoutError::Disconnected)=>break,
                        Err(mpsc::RecvTimeoutError::Timeout)=>{},
                    }
                    if last_flush.elapsed() >= Duration::from_secs(5) || snapshot.lock().map(|s|s.is_none()).unwrap_or(false) {
                        let oldest:String=db.query_row("SELECT date('now','-7 days')",[],|r|r.get(0))?;
                        counts.retain(|(d,_,_),_| d>=&oldest && d<=&day);
                        pending.retain(|_,v|v.1.elapsed()<Duration::from_secs(24*3600));
                        let mut days:BTreeMap<String,Day>=BTreeMap::new();
                        days.entry(day.clone()).or_insert(Day{day:day.clone(),rows:Vec::new(),dropped:0});
                        for ((day,metric,dimensions),value) in &counts {
                            let entry=days.entry(day.clone()).or_insert(Day{day:day.clone(),rows:Vec::new(),dropped:0});
                            if metric=="dropped" {entry.dropped=entry.dropped.saturating_add(*value);} else {entry.rows.push(Row{metric:metric.clone(),dimensions:dimensions.clone(),value:*value});}
                        }
                        // Publish only persisted snapshots: an upload must never get ahead of the
                        // restart baseline. Full disks affect analytics only.
                        let mut persist=||->anyhow::Result<()> {
                            let tx=db.transaction()?;
                            tx.execute("DELETE FROM counters WHERE day < date('now','-7 days')",[])?;
                            for ((day,metric,dims),value) in &counts {tx.execute("INSERT INTO counters VALUES(?1,?2,?3,?4) ON CONFLICT(day,metric,dimensions) DO UPDATE SET value=excluded.value",params![day,metric,serde_json::to_string(dims)?,*value as i64])?;}
                            tx.commit()?;Ok(())
                        };
                        if (dirty || day != last_day)
                            && persist().is_ok() { dirty = false; }
                        if !dirty
                        && let Ok(mut target)=snapshot.lock() {*target=Some(Report{version:1,generation:generation.clone(),engine_version:env!("CARGO_PKG_VERSION").into(),platform:match std::env::consts::OS {"macos"=>"macos","linux"=>"linux","windows"=>"windows",_=>"other"}.into(),days:days.into_values().collect()});}
                        last_day = day;
                        last_flush=Instant::now();
                    }
                }
                Ok(())
            };
            let _=run();
        }).is_err() {return Self::default();}
        Self { tx: Some(tx), report, dropped }
    }
    fn event(&self, event: Event) {
        if let Some(tx) = &self.tx
            && tx.try_send(event).is_err()
        {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
    pub fn count(&self, metric: &'static str, dims: Dimensions, value: u64) {
        self.event(Event::Count(metric, dims, value));
    }
    pub fn start(&self, id: &str, dims: Dimensions, reply: Option<&str>) {
        self.event(Event::Start(id.into(), dims, reply.map(str::to_owned), Instant::now()));
    }
    pub fn stage(&self, id: &str, stage: &'static str) {
        self.event(Event::Stage(id.into(), stage, Instant::now()));
    }
    pub fn snapshot(&self) -> Option<Report> {
        self.report.try_lock().ok().and_then(|s| s.clone())
    }
}

/// A malformed optional label must not refuse the message. Only this finite
/// vocabulary reaches aggregate counters; arbitrary input is discarded locally.
pub fn purpose<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    let value = serde_json::Value::deserialize(d)?;
    Ok(value.as_str().and_then(|s| match s {
        "review" | "delegate" | "status" | "question" | "handoff" | "other" => Some(s.to_owned()),
        _ => None,
    }))
}
