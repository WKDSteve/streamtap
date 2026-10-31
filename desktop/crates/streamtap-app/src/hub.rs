//! The app's end of the browser link: the bridge the host connects to, the
//! pairing decisions and the tab lists.
//!
//! A host connects to a loopback socket the app opens (its port and a random
//! key, new at each start, are written to `bridge.json`), sends the key, says
//! who it is (`hello`) and then relays the extension's messages. The hub:
//!
//! - gives a connection without the right key no slot and no prompt;
//! - refuses any origin but the streamtap extension's;
//! - sends port and token only to a browser the user approved (now, or at an
//!   earlier time: `pairings.json`), and never anything but a state before that;
//! - keeps the tab list of each approved connection and passes a tap request
//!   on only for a tab that connection reported.
//!
//! Page-controlled strings (titles, hosts) are already cleaned and bounded by
//! the message schema; the UI shows them as text only.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use streamtap_host::framing::read_line;
use streamtap_host::proto::{
    BridgeMsg, HostMsg, Pairing, TabInfo, ALLOWED_ORIGIN, BRIDGE_FILE, MAX_BRIDGE_KEY,
    MAX_BRIDGE_LINE,
};

use crate::pairings::{Approval, Store};
use crate::secret::Token;

/// Hosts connected at once (one per browser profile, with room to spare).
const MAX_CONNECTIONS: usize = 16;
/// A connection that has not sent the bridge key by then is dropped.
const KEY_TIMEOUT: Duration = Duration::from_secs(2);
/// A connection that has not said hello by then is dropped.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// Longest a write to a host may block.
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

/// The sink's port and token, handed to approved browsers only.
#[derive(Clone, Debug)]
struct Credentials {
    port: u16,
    token: Token,
}

struct Conn {
    out: Box<dyn Write + Send>,
    close: Box<dyn Fn() + Send>,
    /// Set by `hello`.
    who: Option<Who>,
    approved: bool,
    tabs: Vec<TabInfo>,
}

#[derive(Clone)]
struct Who {
    origin: String,
    instance: String,
    browser: String,
    version: String,
}

impl Conn {
    fn push(&mut self, msg: &HostMsg) {
        if let Ok(mut line) = serde_json::to_vec(msg) {
            line.push(b'\n');
            if self
                .out
                .write_all(&line)
                .and_then(|()| self.out.flush())
                .is_err()
            {
                (self.close)();
            }
        }
    }
}

struct Inner {
    store: Store,
    creds: Credentials,
    conns: BTreeMap<u64, Conn>,
    next_id: u64,
}

pub(crate) struct Hub(Mutex<Inner>);

#[derive(Serialize)]
pub(crate) struct TabView {
    pub(crate) id: u32,
    pub(crate) title: String,
    pub(crate) host: String,
    pub(crate) favicon: Option<String>,
    pub(crate) drm: bool,
    pub(crate) tapping: bool,
    pub(crate) active: bool,
}

#[derive(Serialize)]
pub(crate) struct ConnView {
    pub(crate) id: u64,
    pub(crate) browser: String,
    pub(crate) version: String,
    /// First characters of the instance id: tells two profiles apart.
    pub(crate) profile: String,
    pub(crate) approved: bool,
    pub(crate) tabs: Vec<TabView>,
}

#[derive(Serialize)]
pub(crate) struct ApprovalView {
    pub(crate) instance: String,
    pub(crate) profile: String,
    pub(crate) browser: String,
    pub(crate) approved_at: u64,
    pub(crate) connected: bool,
}

#[derive(Serialize)]
pub(crate) struct HubView {
    pub(crate) connections: Vec<ConnView>,
    pub(crate) approvals: Vec<ApprovalView>,
    pub(crate) store_error: Option<String>,
}

fn short(instance: &str) -> String {
    instance.chars().take(8).collect()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl Hub {
    pub(crate) fn new(store: Store, port: u16, token: &Token) -> Self {
        Self(Mutex::new(Inner {
            store,
            creds: Credentials {
                port,
                token: token.clone(),
            },
            conns: BTreeMap::new(),
            next_id: 1,
        }))
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A new, still anonymous connection. `None` when there are too many.
    pub(crate) fn attach(
        &self,
        out: Box<dyn Write + Send>,
        close: Box<dyn Fn() + Send>,
    ) -> Option<u64> {
        let mut g = self.lock();
        if g.conns.len() >= MAX_CONNECTIONS {
            return None;
        }
        let id = g.next_id;
        g.next_id += 1;
        g.conns.insert(
            id,
            Conn {
                out,
                close,
                who: None,
                approved: false,
                tabs: Vec::new(),
            },
        );
        Some(id)
    }

    pub(crate) fn is_attached(&self, id: u64) -> bool {
        self.lock().conns.contains_key(&id)
    }

    /// Connection slots in use (named or not).
    #[cfg(test)]
    fn slots(&self) -> usize {
        self.lock().conns.len()
    }

    /// The host named itself. Any origin but the extension's is refused; a
    /// browser approved before is approved again at once, any other waits for
    /// the user.
    pub(crate) fn hello(
        &self,
        id: u64,
        origin: &str,
        instance: &str,
        browser: &str,
        version: &str,
    ) {
        let mut g = self.lock();
        if origin != ALLOWED_ORIGIN {
            if let Some(mut c) = g.conns.remove(&id) {
                c.push(&HostMsg::error("origin-not-allowed"));
                (c.close)();
            }
            return;
        }
        // A newer connection of the same browser profile replaces the older one.
        let stale: Vec<u64> = g
            .conns
            .iter()
            .filter(|(k, c)| {
                **k != id
                    && c.who
                        .as_ref()
                        .is_some_and(|w| w.origin == origin && w.instance == instance)
            })
            .map(|(k, _)| *k)
            .collect();
        for k in stale {
            if let Some(c) = g.conns.remove(&k) {
                (c.close)();
            }
        }
        let known = g.store.contains(origin, instance);
        let creds = g.creds.clone();
        let Some(conn) = g.conns.get_mut(&id) else {
            return;
        };
        conn.who = Some(Who {
            origin: origin.to_string(),
            instance: instance.to_string(),
            browser: browser.to_string(),
            version: version.to_string(),
        });
        if known {
            conn.approved = true;
            conn.push(&HostMsg::state(Pairing::Approved));
            conn.push(&HostMsg::credentials(creds.port, creds.token.expose()));
        } else {
            conn.push(&HostMsg::state(Pairing::Pending));
        }
    }

    /// The tab list of an approved connection; from an unapproved one it is ignored.
    pub(crate) fn tabs(&self, id: u64, tabs: Vec<TabInfo>) {
        if let Some(c) = self.lock().conns.get_mut(&id) {
            if c.approved {
                c.tabs = tabs;
            }
        }
    }

    /// The user's answer to a pending pairing.
    pub(crate) fn decide(&self, id: u64, approve: bool) -> Result<(), String> {
        let mut g = self.lock();
        let creds = g.creds.clone();
        let who = match g.conns.get(&id) {
            Some(c) if c.approved => return Err("already approved".into()),
            Some(Conn { who: Some(w), .. }) => w.clone(),
            _ => return Err("this request is gone".into()),
        };
        if !approve {
            if let Some(mut c) = g.conns.remove(&id) {
                c.push(&HostMsg::state(Pairing::Denied));
                (c.close)();
            }
            return Ok(());
        }
        g.store.add(Approval {
            origin: who.origin,
            instance: who.instance,
            browser: who.browser,
            approved_at: now_secs(),
        })?;
        if let Some(c) = g.conns.get_mut(&id) {
            c.approved = true;
            c.push(&HostMsg::state(Pairing::Approved));
            c.push(&HostMsg::credentials(creds.port, creds.token.expose()));
        }
        Ok(())
    }

    /// Takes the approval of a browser profile back and ends its connections.
    pub(crate) fn revoke(&self, instance: &str) -> Result<(), String> {
        let mut g = self.lock();
        g.store.remove(instance)?;
        let ids: Vec<u64> = g
            .conns
            .iter()
            .filter(|(_, c)| c.who.as_ref().is_some_and(|w| w.instance == instance))
            .map(|(k, _)| *k)
            .collect();
        for id in ids {
            if let Some(mut c) = g.conns.remove(&id) {
                c.push(&HostMsg::state(Pairing::Revoked));
                (c.close)();
            }
        }
        Ok(())
    }

    /// Asks the browser of connection `id` to switch capture of `tab`. Only a
    /// tab that connection reported is accepted.
    pub(crate) fn request_tap(&self, id: u64, tab: u32, on: bool) -> Result<(), String> {
        let mut g = self.lock();
        let c = g
            .conns
            .get_mut(&id)
            .ok_or("this browser is not connected")?;
        if !c.approved {
            return Err("this browser is not paired".into());
        }
        if !c.tabs.iter().any(|t| t.id == tab) {
            return Err("this tab is not in the list (any more)".into());
        }
        c.push(&HostMsg::tap(tab, on));
        Ok(())
    }

    /// The sink's settings changed: approved browsers get the new ones.
    pub(crate) fn set_credentials(&self, port: u16, token: &Token) {
        let mut g = self.lock();
        g.creds = Credentials {
            port,
            token: token.clone(),
        };
        for c in g.conns.values_mut().filter(|c| c.approved) {
            c.push(&HostMsg::credentials(port, token.expose()));
        }
    }

    pub(crate) fn disconnect(&self, id: u64) {
        if let Some(c) = self.lock().conns.remove(&id) {
            (c.close)();
        }
    }

    /// Ends every connection (the app exits).
    pub(crate) fn close_all(&self) {
        let conns = std::mem::take(&mut self.lock().conns);
        for c in conns.into_values() {
            (c.close)();
        }
    }

    pub(crate) fn view(&self) -> HubView {
        let g = self.lock();
        let connections = g
            .conns
            .iter()
            .filter_map(|(id, c)| {
                let who = c.who.as_ref()?;
                Some(ConnView {
                    id: *id,
                    browser: who.browser.clone(),
                    version: who.version.clone(),
                    profile: short(&who.instance),
                    approved: c.approved,
                    tabs: c
                        .tabs
                        .iter()
                        .map(|t| TabView {
                            id: t.id,
                            title: t.title.clone(),
                            host: t.host.clone(),
                            favicon: t.favicon.clone(),
                            drm: t.drm,
                            tapping: t.tapping,
                            active: t.active,
                        })
                        .collect(),
                })
            })
            .collect();
        let approvals = g
            .store
            .list()
            .iter()
            .map(|a| ApprovalView {
                instance: a.instance.clone(),
                profile: short(&a.instance),
                browser: a.browser.clone(),
                approved_at: a.approved_at,
                connected: g
                    .conns
                    .values()
                    .any(|c| c.who.as_ref().is_some_and(|w| w.instance == a.instance)),
            })
            .collect();
        HubView {
            connections,
            approvals,
            store_error: g.store.error.clone(),
        }
    }
}

/// The listening side: accepts hosts and feeds the hub. Dropping it (or
/// `stop`) removes `bridge.json` so a host does not find a dead port.
pub(crate) struct Server {
    file: PathBuf,
}

impl Server {
    /// Opens the bridge on a loopback port of the system's choosing and
    /// writes the port and a new random key into `dir/bridge.json`. Only a
    /// connection that sends that key first gets a slot in the hub.
    pub(crate) fn start(hub: &Arc<Hub>, dir: &Path) -> Result<Self, String> {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .map_err(|e| format!("cannot open the browser bridge: {e}"))?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        let key = Arc::new(Token::generate());
        let file = dir.join(BRIDGE_FILE);
        let tmp = file.with_extension("json.tmp");
        let text = serde_json::json!({ "port": port, "key": key.expose() }).to_string();
        std::fs::write(&tmp, text)
            .and_then(|()| std::fs::rename(&tmp, &file))
            .map_err(|e| format!("cannot write {}: {e}", file.display()))?;
        let hub = Arc::clone(hub);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (hub, key) = (Arc::clone(&hub), Arc::clone(&key));
                std::thread::spawn(move || serve_connection(&hub, stream, &key));
            }
        });
        Ok(Self { file })
    }

    pub(crate) fn stop(&self) {
        let _ = std::fs::remove_file(&self.file);
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Reads the key line: at most `MAX_BRIDGE_KEY` bytes and until `deadline`
/// in total, not per read, so a client that trickles in a byte now and then
/// is cut off as well. None for a timeout, a closed or failed connection, a
/// line that is too long or one without its end.
fn read_key(lines: &mut BufReader<TcpStream>, deadline: Instant) -> Option<Vec<u8>> {
    let mut key = Vec::new();
    loop {
        let left = deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())?;
        lines.get_ref().set_read_timeout(Some(left)).ok()?;
        let buf = lines.fill_buf().ok()?;
        if buf.is_empty() {
            return None;
        }
        let (take, found) = buf
            .iter()
            .position(|&b| b == b'\n')
            .map_or((buf.len(), false), |i| (i, true));
        if key.len() + take > MAX_BRIDGE_KEY {
            return None;
        }
        key.extend_from_slice(&buf[..take]);
        lines.consume(take + usize::from(found));
        if found {
            if key.last() == Some(&b'\r') {
                key.pop();
            }
            return Some(key);
        }
    }
}

fn serve_connection(hub: &Hub, stream: TcpStream, key: &Token) {
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    // The key comes first, within a short time in total and a small bound;
    // without it the connection gets no slot and no prompt.
    let deadline = Instant::now() + KEY_TIMEOUT;
    let mut lines = BufReader::new(stream);
    let keyed = read_key(&mut lines, deadline).is_some_and(|k| key.matches(&k));
    let stream = lines.get_ref();
    if !keyed {
        let _ = stream.shutdown(Shutdown::Both);
        return;
    }
    let (Ok(writer), Ok(closer)) = (stream.try_clone(), stream.try_clone()) else {
        return;
    };
    let Some(id) = hub.attach(
        Box::new(writer),
        Box::new(move || {
            let _ = closer.shutdown(Shutdown::Both);
        }),
    ) else {
        let _ = stream.shutdown(Shutdown::Both);
        return;
    };
    let _ = stream.set_read_timeout(Some(HELLO_TIMEOUT));
    // The first line must be the hello.
    let first = read_line(&mut lines, MAX_BRIDGE_LINE)
        .ok()
        .flatten()
        .and_then(|l| BridgeMsg::parse(&l).ok());
    let Some(BridgeMsg::Hello {
        origin,
        instance,
        browser,
        version,
        ..
    }) = first
    else {
        hub.disconnect(id);
        return;
    };
    hub.hello(id, &origin, &instance, &browser, &version);
    let _ = lines.get_ref().set_read_timeout(None);
    while hub.is_attached(id) {
        let Some(msg) = read_line(&mut lines, MAX_BRIDGE_LINE)
            .ok()
            .flatten()
            .and_then(|l| BridgeMsg::parse(&l).ok())
        else {
            break;
        };
        match msg {
            BridgeMsg::Tabs { tabs, .. } => hub.tabs(id, tabs),
            BridgeMsg::Ping { .. } => {}
            BridgeMsg::Hello { .. } => break,
        }
    }
    hub.disconnect(id);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    const SECRET: &str = "0123456789abcdef0123456789abcdef";

    /// What a connection was sent, and whether it was closed.
    #[derive(Clone, Default)]
    struct Wire {
        data: Arc<Mutex<Vec<u8>>>,
        closed: Arc<AtomicBool>,
    }
    impl Write for Wire {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.data.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl Wire {
        fn sent(&self) -> Vec<HostMsg> {
            let data = self.data.lock().unwrap().clone();
            String::from_utf8(data)
                .unwrap()
                .lines()
                .map(|l| HostMsg::parse(l.as_bytes()).unwrap())
                .collect()
        }
        fn has_credentials(&self) -> bool {
            self.sent()
                .iter()
                .any(|m| matches!(m, HostMsg::Credentials { .. }))
        }
        fn closed(&self) -> bool {
            self.closed.load(Ordering::SeqCst)
        }
    }

    fn hub(name: &str) -> (Hub, PathBuf) {
        let dir = std::env::temp_dir().join(format!("streamtap-hub-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::load(&dir.join("pairings.json"));
        (Hub::new(store, 17650, &SECRET.into()), dir)
    }

    fn connect(h: &Hub) -> (u64, Wire) {
        let w = Wire::default();
        let flag = Arc::clone(&w.closed);
        let id = h
            .attach(
                Box::new(w.clone()),
                Box::new(move || flag.store(true, Ordering::SeqCst)),
            )
            .unwrap();
        (id, w)
    }

    fn hello(h: &Hub, id: u64, instance: &str) {
        h.hello(id, ALLOWED_ORIGIN, instance, "chrome", "0.3.0");
    }

    fn tab(id: u32) -> TabInfo {
        TabInfo {
            id,
            title: "t".into(),
            host: "video.example".into(),
            favicon: None,
            drm: false,
            tapping: false,
            active: true,
        }
    }

    #[test]
    fn an_unknown_browser_gets_no_credentials_until_the_user_approves() {
        let (h, dir) = hello_dir("pending");
        let (id, w) = connect(&h);
        hello(&h, id, "profile-aaaa");
        assert_eq!(w.sent(), [HostMsg::state(Pairing::Pending)]);
        assert!(!w.has_credentials());
        // Its tab list is not kept either.
        h.tabs(id, vec![tab(1)]);
        assert!(h.view().connections[0].tabs.is_empty());
        assert!(h.request_tap(id, 1, true).is_err());

        h.decide(id, true).unwrap();
        assert!(w.has_credentials());
        assert_eq!(w.sent()[1], HostMsg::state(Pairing::Approved));
        h.tabs(id, vec![tab(1)]);
        assert_eq!(h.view().connections[0].tabs.len(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn hello_dir(name: &str) -> (Hub, PathBuf) {
        hub(name)
    }

    #[test]
    fn a_foreign_origin_is_rejected_and_closed() {
        let (h, dir) = hub("origin");
        let (id, w) = connect(&h);
        h.hello(
            id,
            "chrome-extension://bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb/",
            "profile-aaaa",
            "chrome",
            "1",
        );
        assert_eq!(w.sent(), [HostMsg::error("origin-not-allowed")]);
        assert!(w.closed());
        assert!(!h.is_attached(id));
        assert!(h.decide(id, true).is_err(), "nothing to approve");
        assert!(!w.has_credentials());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_denied_browser_is_closed_and_never_stored() {
        let (h, dir) = hub("deny");
        let (id, w) = connect(&h);
        hello(&h, id, "profile-aaaa");
        h.decide(id, false).unwrap();
        assert_eq!(
            w.sent(),
            [
                HostMsg::state(Pairing::Pending),
                HostMsg::state(Pairing::Denied)
            ]
        );
        assert!(w.closed());
        assert!(h.view().approvals.is_empty());
        // Asking again is a new question.
        let (id2, w2) = connect(&h);
        hello(&h, id2, "profile-aaaa");
        assert_eq!(w2.sent(), [HostMsg::state(Pairing::Pending)]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_approved_profile_is_known_again_after_a_restart() {
        let (h, dir) = hub("again");
        let (id, _) = connect(&h);
        hello(&h, id, "profile-aaaa");
        h.decide(id, true).unwrap();
        // A new hub over the same file: the app was restarted.
        let h2 = Hub::new(
            Store::load(&dir.join("pairings.json")),
            17651,
            &SECRET.into(),
        );
        let (id, w) = connect(&h2);
        hello(&h2, id, "profile-aaaa");
        assert_eq!(w.sent()[0], HostMsg::state(Pairing::Approved));
        assert!(w.has_credentials());
        // Another profile of the same browser asks again.
        let (id, w) = connect(&h2);
        hello(&h2, id, "profile-bbbb");
        assert_eq!(w.sent(), [HostMsg::state(Pairing::Pending)]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn revoking_ends_the_connection_and_the_next_hello_asks_again() {
        let (h, dir) = hub("revoke");
        let (id, w) = connect(&h);
        hello(&h, id, "profile-aaaa");
        h.decide(id, true).unwrap();
        h.revoke("profile-aaaa").unwrap();
        assert_eq!(*w.sent().last().unwrap(), HostMsg::state(Pairing::Revoked));
        assert!(w.closed());
        assert!(h.view().approvals.is_empty());
        let (id, w2) = connect(&h);
        hello(&h, id, "profile-aaaa");
        assert_eq!(w2.sent(), [HostMsg::state(Pairing::Pending)]);
        assert!(!w2.has_credentials());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// What the app does on a revoke: the approval goes, then the new token
    /// is handed out. The revoked browser never sees it.
    #[test]
    fn a_token_set_after_a_revoke_reaches_the_others_only() {
        let (h, dir) = hub("revoke-rotate");
        let (a, wa) = connect(&h);
        hello(&h, a, "profile-aaaa");
        h.decide(a, true).unwrap();
        let (b, wb) = connect(&h);
        hello(&h, b, "profile-bbbb");
        h.decide(b, true).unwrap();
        h.revoke("profile-aaaa").unwrap();
        let before = wa.sent().len();
        h.set_credentials(17650, &"fedcba9876543210fedcba9876543210".into());
        assert_eq!(wa.sent().len(), before, "the revoked browser gets nothing");
        assert_eq!(*wa.sent().last().unwrap(), HostMsg::state(Pairing::Revoked));
        assert_eq!(
            wb.sent().pop().unwrap(),
            HostMsg::credentials(17650, "fedcba9876543210fedcba9876543210")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_tap_request_needs_an_approved_browser_and_a_reported_tab() {
        let (h, dir) = hub("tap");
        let (id, w) = connect(&h);
        hello(&h, id, "profile-aaaa");
        h.decide(id, true).unwrap();
        h.tabs(id, vec![tab(7)]);
        assert!(h.request_tap(id, 8, true).is_err(), "a tab nobody reported");
        h.request_tap(id, 7, true).unwrap();
        assert_eq!(*w.sent().last().unwrap(), HostMsg::tap(7, true));
        assert!(h.request_tap(999, 7, true).is_err(), "unknown connection");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn changed_settings_reach_approved_browsers_only() {
        let (h, dir) = hub("creds");
        let (a, wa) = connect(&h);
        hello(&h, a, "profile-aaaa");
        h.decide(a, true).unwrap();
        let (b, wb) = connect(&h);
        hello(&h, b, "profile-bbbb");
        h.set_credentials(17999, &"fedcba9876543210fedcba9876543210".into());
        let last = wa.sent().pop().unwrap();
        assert!(matches!(last, HostMsg::Credentials { port: 17999, .. }));
        assert!(!wb.has_credentials(), "a pending browser gets nothing");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_new_connection_of_the_same_profile_replaces_the_old_one() {
        let (h, dir) = hub("replace");
        let (a, wa) = connect(&h);
        hello(&h, a, "profile-aaaa");
        let (b, _) = connect(&h);
        hello(&h, b, "profile-aaaa");
        assert!(wa.closed());
        assert!(!h.is_attached(a) && h.is_attached(b));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn connections_are_bounded() {
        let (h, dir) = hub("bound");
        for _ in 0..MAX_CONNECTIONS {
            connect(&h);
        }
        assert!(h
            .attach(Box::new(Wire::default()), Box::new(|| {}))
            .is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn credentials_do_not_print_their_token() {
        let c = Credentials {
            port: 1,
            token: SECRET.into(),
        };
        assert!(!format!("{c:?}").contains(SECRET));
    }

    fn hello_line(origin: &str) -> Vec<u8> {
        let hello = BridgeMsg::Hello {
            v: 1,
            origin: origin.into(),
            instance: "profile-aaaa".into(),
            browser: "edge".into(),
            version: "0.3.0".into(),
        };
        let mut line = serde_json::to_vec(&hello).unwrap();
        line.push(b'\n');
        line
    }

    /// Sends `first` (the key line, or anything else) and a hello; returns
    /// the first answer, or none when the bridge closed without one.
    fn talk_raw(port: u16, first: &[u8]) -> Option<HostMsg> {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.write_all(first).unwrap();
        // The bridge may already have closed after a bad first line.
        let _ = s.write_all(&hello_line(ALLOWED_ORIGIN));
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut r = BufReader::new(s);
        read_line(&mut r, 4096)
            .ok()
            .flatten()
            .map(|l| HostMsg::parse(&l).unwrap())
    }

    /// Without the key from `bridge.json` a connection gets no slot and no
    /// pairing prompt: silent, wrong, missing or oversized keys.
    #[test]
    fn the_bridge_wants_its_key_before_anything_else() {
        let (h, dir) = hub("key");
        let hub = Arc::new(h);
        let server = Server::start(&hub, &dir).unwrap();
        let bridge = streamtap_host::relay::read_bridge(&dir).unwrap();
        let key = bridge.key().to_string();
        assert_eq!(key.len(), 64);

        // More silent connections than there are slots: none takes one.
        let silent: Vec<TcpStream> = (0..=MAX_CONNECTIONS)
            .map(|_| TcpStream::connect(("127.0.0.1", bridge.port)).unwrap())
            .collect();
        let mut wrong = key.clone().into_bytes();
        wrong[0] = if wrong[0] == b'0' { b'1' } else { b'0' };
        wrong.push(b'\n');
        for first in [
            wrong,
            hello_line(ALLOWED_ORIGIN),
            b"\n".to_vec(),
            format!("{key}{key}{key}\n").into_bytes(),
            key.as_bytes()[..63].to_vec(),
        ] {
            assert_eq!(
                talk_raw(bridge.port, &first),
                None,
                "{}",
                String::from_utf8_lossy(&first)
            );
        }
        assert_eq!(hub.slots(), 0, "no slot without the key");
        assert!(hub.view().connections.is_empty(), "no prompt");

        // The right key works, although the silent ones are still open.
        let ok = talk_raw(bridge.port, format!("{key}\n").as_bytes());
        assert_eq!(ok, Some(HostMsg::state(Pairing::Pending)));
        drop(silent);
        server.stop();
        hub.close_all();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The key timeout counts in total: a client that sends a byte of the
    /// right key now and then (each read is quick) is cut off all the same.
    #[test]
    fn a_trickled_key_is_cut_off_after_the_key_timeout() {
        use std::io::Read as _;
        let (h, dir) = hub("trickle");
        let hub = Arc::new(h);
        let server = Server::start(&hub, &dir).unwrap();
        let bridge = streamtap_host::relay::read_bridge(&dir).unwrap();
        let key = bridge.key().as_bytes().to_vec();
        let mut s = TcpStream::connect(("127.0.0.1", bridge.port)).unwrap();
        s.set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let start = Instant::now();
        let mut sent = 0;
        let closed = loop {
            if start.elapsed() > KEY_TIMEOUT * 3 {
                break false;
            }
            if s.write_all(&key[sent..=sent]).is_err() {
                break true;
            }
            sent += 1;
            let mut b = [0u8; 1];
            match s.read(&mut b) {
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                _ => break true,
            }
            std::thread::sleep(Duration::from_millis(400));
        };
        assert!(closed, "still open after {:?}", start.elapsed());
        assert!(
            start.elapsed() < KEY_TIMEOUT * 2,
            "closed only after {:?}",
            start.elapsed()
        );
        assert!(sent < key.len(), "the key was never complete");
        assert_eq!(hub.slots(), 0);
        server.stop();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn every_start_writes_a_new_key() {
        let (h, dir) = hub("newkey");
        let hub = Arc::new(h);
        let first = Server::start(&hub, &dir).unwrap();
        let a = streamtap_host::relay::read_bridge(&dir).unwrap();
        drop(first);
        let second = Server::start(&hub, &dir).unwrap();
        let b = streamtap_host::relay::read_bridge(&dir).unwrap();
        assert_ne!(a.key(), b.key());
        // The old key does not open the new bridge.
        assert_eq!(talk_raw(b.port, format!("{}\n", a.key()).as_bytes()), None);
        drop(second);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The real bridge over a socket: a host-like client says hello, is told
    /// it is pending, and its unknown origin is turned away.
    #[test]
    fn the_server_writes_its_port_and_speaks_the_line_protocol() {
        let (h, dir) = hub("server");
        let hub = Arc::new(h);
        let server = Server::start(&hub, &dir).unwrap();
        let bridge = streamtap_host::relay::read_bridge(&dir).unwrap();
        let port = bridge.port;
        let key_line = format!("{}\n", bridge.key());

        let talk = |origin: &str| {
            let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
            s.write_all(key_line.as_bytes()).unwrap();
            let hello = BridgeMsg::Hello {
                v: 1,
                origin: origin.into(),
                instance: "profile-aaaa".into(),
                browser: "edge".into(),
                version: "0.3.0".into(),
            };
            let mut line = serde_json::to_vec(&hello).unwrap();
            line.push(b'\n');
            s.write_all(&line).unwrap();
            s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut r = BufReader::new(s);
            let reply = read_line(&mut r, 4096).unwrap().unwrap();
            HostMsg::parse(&reply).unwrap()
        };
        assert_eq!(talk(ALLOWED_ORIGIN), HostMsg::state(Pairing::Pending));
        assert_eq!(
            talk("chrome-extension://cccccccccccccccccccccccccccccccc/"),
            HostMsg::error("origin-not-allowed")
        );
        // A line after the key that is not a hello is dropped without an answer.
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.write_all(key_line.as_bytes()).unwrap();
        s.write_all(b"{\"t\":\"ping\",\"v\":1}\n").unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut r = BufReader::new(s);
        assert!(read_line(&mut r, 4096).unwrap().is_none());

        server.stop();
        assert!(streamtap_host::relay::read_bridge(&dir).is_err());
        hub.close_all();
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
