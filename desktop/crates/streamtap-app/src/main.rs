//! streamtap desktop app: a small Tauri shell around `streamtap-sink`.
//!
//! The sink stays a separate process; this app starts and stops it, reads its
//! capture directory and runs `streamtap-sink mux` on request. The webview
//! (plain files in `ui/`) can call the commands below and nothing else.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod captures;
mod config;
mod config_access;
mod hub;
mod jobs;
mod pairings;
mod paths;
mod phones;
mod registration;
mod secret;
mod sink;

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tauri::{Manager, RunEvent, State};

use captures::{CaptureInfo, IndexAcc};
use config::Settings;
use hub::{Hub, HubView};
use registration::Browser;
use sink::MuxState;

type CmdResult<T> = Result<T, String>;

struct SinkProc {
    child: Child,
    /// Pid and start of the process, to tell its captures from leftovers.
    run: captures::SinkRun,
    /// Ends the sink with the app, even when the app is killed hard.
    _job: Option<jobs::KillOnClose>,
}

/// How the last sink process ended, when nobody asked it to.
#[derive(Debug, Clone, Serialize)]
struct SinkExit {
    code: Option<i32>,
}

struct Inner {
    settings: Settings,
    config_path: PathBuf,
    /// Why the stored config could not be used (the defaults run meanwhile).
    config_error: Option<String>,
    /// Broad groups that can read `config.json`, checked at start (SRV-14).
    config_access: Option<config_access::ConfigAccess>,
    sink: Option<SinkProc>,
    crashed: Option<SinkExit>,
    log: VecDeque<String>,
    mux: HashMap<String, MuxState>,
    /// The running mux jobs, to abort them when the app exits.
    mux_handles: HashMap<String, Arc<sink::MuxHandle>>,
    index_cache: HashMap<String, IndexAcc>,
    /// Counts sink starts: output of an earlier sink process (its reader
    /// threads may still be draining) is not mixed into the new log.
    log_gen: u64,
    /// The app's config directory (`config.json`, `pairings.json`, `bridge.json`, `devices.json`, the host manifests).
    config_dir: PathBuf,
    /// Pairing and tab lists of the browsers (see `hub`); has its own lock.
    hub: Arc<Hub>,
    /// The bridge the native messaging host connects to.
    bridge: Option<hub::Server>,
    /// Why the bridge could not be opened.
    bridge_error: Option<String>,
}

#[derive(Clone)]
struct AppState(Arc<Mutex<Inner>>);

impl AppState {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl AppState {
    /// The app is exiting: stops the sink and aborts every running mux
    /// (their `out.part.mp4` goes with them). A hard kill skips this, the
    /// job objects then end the children.
    fn shutdown(&self) {
        let handles: Vec<_> = {
            let mut g = self.lock();
            // The hosts find no bridge any more; their browsers see the app close.
            g.bridge = None;
            g.hub.close_all();
            g.stop_sink();
            g.mux_handles.drain().map(|(_, h)| h).collect()
        };
        // Outside the lock: aborting waits for the process, and the mux
        // threads take the lock when their job ends.
        for h in handles {
            h.abort();
        }
    }
}

impl Inner {
    fn new(settings: Settings, config_path: PathBuf, config_error: Option<String>) -> Self {
        let config_dir = config_path
            .parent()
            .map_or_else(PathBuf::new, Path::to_path_buf);
        let store = pairings::Store::load(&config_dir.join("pairings.json"));
        let hub = Arc::new(Hub::new(store, settings.port, &settings.token));
        Self {
            settings,
            config_path,
            config_dir,
            hub,
            bridge: None,
            bridge_error: None,
            config_error,
            config_access: None,
            sink: None,
            crashed: None,
            log: VecDeque::new(),
            mux: HashMap::new(),
            mux_handles: HashMap::new(),
            index_cache: HashMap::new(),
            log_gen: 0,
        }
    }

    /// Notices a sink that exited by itself.
    fn refresh_sink(&mut self) {
        let Some(proc) = self.sink.as_mut() else {
            return;
        };
        if let Ok(Some(status)) = proc.child.try_wait() {
            self.sink = None;
            self.crashed = Some(SinkExit {
                code: status.code(),
            });
        }
    }

    fn sink_running(&self) -> bool {
        self.sink.is_some()
    }

    /// The running sink process.
    fn sink_run(&self) -> Option<captures::SinkRun> {
        self.sink.as_ref().map(|p| p.run)
    }

    /// Checks who can read `config.json` and notes a finding in the log (and
    /// on stderr) as well as for the UI.
    fn check_config_access(&mut self) {
        let access = config_access::check(&self.config_path);
        if let Some(a) = &access {
            let line = a.log_line(&self.config_path);
            eprintln!("{line}");
            sink::push_log(&mut self.log, line);
        }
        self.config_access = access;
    }

    fn stop_sink(&mut self) {
        if let Some(mut proc) = self.sink.take() {
            let _ = proc.child.kill();
            let _ = proc.child.wait();
        }
        self.crashed = None;
    }
}

#[derive(Serialize)]
struct SinkStatus {
    /// `stopped`, `running` or `crashed`.
    state: &'static str,
    pid: Option<u32>,
    exit_code: Option<i32>,
}

#[derive(Serialize)]
struct Snapshot {
    sink: SinkStatus,
    sink_path: Option<String>,
    ffmpeg: Option<String>,
    config_error: Option<String>,
    config_access: Option<config_access::ConfigAccess>,
    captures: Vec<CaptureInfo>,
    captures_error: Option<String>,
    mux: HashMap<String, MuxState>,
    log: Vec<String>,
    browser: BrowserSnapshot,
}

#[derive(Serialize)]
struct Registration {
    browser: &'static str,
    /// `registered`, `not-registered` or `stale`; none when the registry could not be read.
    state: Option<registration::State>,
    error: Option<String>,
}

#[derive(Serialize)]
struct BrowserSnapshot {
    host_path: Option<String>,
    bridge_error: Option<String>,
    registrations: Vec<Registration>,
    link: HubView,
}

fn browser_snapshot(g: &Inner) -> BrowserSnapshot {
    let reg = registration::WinRegistry::real();
    let host = sink::find_host();
    let ours = host.as_deref();
    let registrations = Browser::ALL
        .into_iter()
        .map(
            |b| match registration::state(&reg, &g.config_dir, b, ours) {
                Ok(state) => Registration {
                    browser: b.id(),
                    state: Some(state),
                    error: None,
                },
                Err(e) => Registration {
                    browser: b.id(),
                    state: None,
                    error: Some(e),
                },
            },
        )
        .collect();
    BrowserSnapshot {
        host_path: host.map(|p| p.display().to_string()),
        bridge_error: g.bridge_error.clone(),
        registrations,
        link: g.hub.view(),
    }
}

#[tauri::command]
async fn snapshot(state: State<'_, AppState>) -> CmdResult<Snapshot> {
    let mut g = state.lock();
    g.refresh_sink();
    let sink_run = g.sink_run();
    let running = sink_run.is_some();
    let sink = SinkStatus {
        state: if running {
            "running"
        } else if g.crashed.is_some() {
            "crashed"
        } else {
            "stopped"
        },
        pid: g.sink.as_ref().map(|p| p.run.pid),
        exit_code: g.crashed.as_ref().and_then(|c| c.code),
    };
    let root = g.settings.captures_dir.clone();
    let (captures, captures_error) = if root.is_dir() {
        match captures::scan(&root, &mut g.index_cache, sink_run, SystemTime::now()) {
            Ok(list) => (list, None),
            Err(e) => (
                Vec::new(),
                Some(format!("cannot read the captures directory: {e}")),
            ),
        }
    } else {
        (Vec::new(), None)
    };
    Ok(Snapshot {
        sink,
        sink_path: sink::find_sink().map(|p| p.display().to_string()),
        ffmpeg: sink::find_ffmpeg().map(|p| p.display().to_string()),
        config_error: g.config_error.clone(),
        config_access: g.config_access.clone(),
        captures,
        captures_error,
        mux: g.mux.clone(),
        log: g.log.iter().cloned().collect(),
        browser: browser_snapshot(&g),
    })
}

#[tauri::command]
async fn get_settings(state: State<'_, AppState>) -> CmdResult<Settings> {
    Ok(state.lock().settings.clone())
}

fn require_stopped(g: &mut Inner) -> CmdResult<()> {
    g.refresh_sink();
    if g.sink_running() {
        Err("stop the sink first".into())
    } else {
        Ok(())
    }
}

fn apply_settings(g: &mut Inner, new: Settings) -> CmdResult<()> {
    new.validate()?;
    let dir_changes = new.captures_dir != g.settings.captures_dir;
    if dir_changes && g.mux.values().any(|m| matches!(m, MuxState::Running)) {
        // The job would report into the wrong folder's list.
        return Err("a mux is still running; wait until it is done".into());
    }
    config::save(&g.config_path, &new)?;
    if dir_changes {
        g.index_cache.clear();
        g.mux.clear();
    }
    g.settings = new;
    g.config_error = None;
    // Paired browsers get the new port and token.
    g.hub.set_credentials(g.settings.port, &g.settings.token);
    Ok(())
}

#[tauri::command]
async fn save_settings(
    state: State<'_, AppState>,
    port: u16,
    captures_dir: String,
    tailnet: String,
) -> CmdResult<Settings> {
    let mut g = state.lock();
    require_stopped(&mut g)?;
    let new = Settings {
        port,
        captures_dir: PathBuf::from(captures_dir.trim()),
        tailnet: tailnet.trim().to_owned(),
        ..g.settings.clone()
    };
    apply_settings(&mut g, new)?;
    Ok(g.settings.clone())
}

/// Sets language and theme. Unlike the sink settings this works while the
/// sink runs: nothing the sink or a paired browser uses changes. The values
/// apply for this session even when they cannot be written, because the
/// stored config is unusable (that file is only replaced by an explicit
/// Save of the settings).
fn apply_ui_prefs(g: &mut Inner, language: &str, theme: &str) -> CmdResult<()> {
    let language: config::Language = language.parse()?;
    let theme: config::Theme = theme.parse()?;
    g.settings.language = language;
    g.settings.theme = theme;
    if g.config_error.is_some() {
        return Err(
            "not saved: the stored settings could not be read; save the settings first".into(),
        );
    }
    config::save(&g.config_path, &g.settings)
}

#[tauri::command]
async fn save_ui_prefs(
    state: State<'_, AppState>,
    language: String,
    theme: String,
) -> CmdResult<Settings> {
    let mut g = state.lock();
    apply_ui_prefs(&mut g, &language, &theme)?;
    Ok(g.settings.clone())
}

#[tauri::command]
async fn regenerate_token(state: State<'_, AppState>) -> CmdResult<Settings> {
    let mut g = state.lock();
    require_stopped(&mut g)?;
    let new = Settings {
        token: secret::Token::generate(),
        ..g.settings.clone()
    };
    apply_settings(&mut g, new)?;
    Ok(g.settings.clone())
}

#[tauri::command]
async fn start_sink(state: State<'_, AppState>) -> CmdResult<()> {
    let exe = sink::find_sink()
        .ok_or("streamtap-sink was not found next to the app (build it: scripts/build.ps1)")?;
    launch_sink(state.inner(), &exe)
}

/// Starts `exe serve` with the stored settings and reads its output.
fn launch_sink(shared: &AppState, exe: &Path) -> CmdResult<()> {
    launch_sink_locked(shared, &mut shared.lock(), exe)
}

/// `launch_sink` with the lock of `shared` already held as `g`.
fn launch_sink_locked(shared: &AppState, g: &mut Inner, exe: &Path) -> CmdResult<()> {
    require_stopped(g)?;
    g.settings.validate()?;
    let root = g.settings.captures_dir.clone();
    fs::create_dir_all(&root).map_err(|e| format!("cannot create {}: {e}", root.display()))?;
    let token = g.settings.token.clone();
    let devices_file = g.config_dir.join("devices.json");
    // Taken before the spawn: every capture of this sink begins at or after it.
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let mut child = sink::command(exe)
        .args(sink::serve_args(
            g.settings.port,
            &root,
            // The paired phones are kept with the app's settings, not among the captures.
            (!g.settings.tailnet.is_empty())
                .then_some((g.settings.tailnet.as_str(), devices_file.as_path())),
        ))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot start the sink: {e}"))?;
    // The token goes to stdin, never into the command line; stdin is closed
    // right after it (dropped). A sink that cannot take it is ended.
    let handed_over = child
        .stdin
        .take()
        .ok_or_else(|| std::io::Error::other("no stdin pipe"))
        .and_then(|stdin| sink::hand_over_token(stdin, &token));
    if let Err(e) = handed_over {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("cannot hand the token to the sink: {e}"));
    }
    g.log.clear();
    g.log_gen += 1;
    let generation = g.log_gen;
    g.crashed = None;
    let run = captures::SinkRun {
        pid: child.id(),
        since,
    };
    let job = match jobs::KillOnClose::new(&child) {
        Ok(job) => Some(job),
        Err(e) => {
            sink::push_log(
                &mut g.log,
                format!(
                    "warning: the sink is not tied to the app and outlives a hard kill of it: {e}"
                ),
            );
            None
        }
    };
    for pipe in [
        child.stdout.take().map(Pipe::Out),
        child.stderr.take().map(Pipe::Err),
    ]
    .into_iter()
    .flatten()
    {
        let shared = shared.clone();
        let (token, root) = (token.clone(), root.clone());
        std::thread::spawn(move || match pipe {
            Pipe::Out(p) => sink::for_each_line(p, |l| {
                on_sink_line(&shared, l, token.expose(), &root, generation);
            }),
            Pipe::Err(p) => sink::for_each_line(p, |l| {
                on_sink_line(&shared, l, token.expose(), &root, generation);
            }),
        });
    }
    g.sink = Some(SinkProc {
        child,
        run,
        _job: job,
    });
    Ok(())
}

enum Pipe {
    Out(std::process::ChildStdout),
    Err(std::process::ChildStderr),
}

/// One line of sink output: kept for the UI (without the token), and a DRM
/// refusal is written down in the capture so the badge survives a restart.
/// A line of an earlier sink process (`generation` is not current) is dropped.
fn on_sink_line(state: &AppState, line: &str, token: &str, root: &Path, generation: u64) {
    if state.lock().log_gen != generation {
        return;
    }
    if let Some(id) = sink::drm_capture_of(line, root) {
        if let Ok(dir) = paths::resolve_capture(root, &id) {
            let _ = fs::write(dir.join(captures::DRM_FLAG), b"");
        }
    }
    let cleaned = sink::clean_line(line, token);
    let mut g = state.lock();
    if g.log_gen == generation {
        sink::push_log(&mut g.log, cleaned);
    }
}

#[tauri::command]
async fn stop_sink(state: State<'_, AppState>) -> CmdResult<()> {
    state.lock().stop_sink();
    Ok(())
}

#[tauri::command]
async fn mux_capture(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    let sink_exe = sink::find_sink().ok_or("streamtap-sink was not found next to the app")?;
    let dir = {
        let mut g = state.lock();
        g.refresh_sink();
        let dir = paths::resolve_capture(&g.settings.captures_dir, &id)?;
        if matches!(g.mux.get(&id), Some(MuxState::Running)) {
            return Err("a mux of this capture is already running".into());
        }
        if captures::dir_is_live(&dir, g.sink_run(), SystemTime::now()) {
            return Err("the capture is still receiving data; mux it when it is done".into());
        }
        g.mux.insert(id.clone(), MuxState::Running);
        dir
    };
    let shared = state.inner().clone();
    std::thread::spawn(move || {
        let result = sink::run_mux(&sink_exe, &dir, |handle| mux_started(&shared, &id, handle));
        let mut g = shared.lock();
        g.mux_handles.remove(&id);
        g.mux.insert(id, result);
    });
    Ok(())
}

/// A mux of capture `id` runs: its handle is kept (to abort it on exit), and
/// a mux that could not be tied to the app is noted in the log, like the sink.
fn mux_started(shared: &AppState, id: &str, handle: &Arc<sink::MuxHandle>) {
    let mut g = shared.lock();
    if let Some(e) = handle.job_error() {
        sink::push_log(
            &mut g.log,
            format!(
                "warning: the mux of {id} is not tied to the app and outlives a hard kill of it: {e}"
            ),
        );
    }
    g.mux_handles.insert(id.to_owned(), Arc::clone(handle));
}

#[tauri::command]
async fn open_result(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    let dir = paths::resolve_capture(&state.lock().settings.captures_dir, &id)?;
    let (file, _) =
        paths::result_file(&dir).ok_or("this capture has no result yet; mux it first")?;
    sink::open_path(&file)
}

#[tauri::command]
async fn open_folder(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    let dir = paths::resolve_capture(&state.lock().settings.captures_dir, &id)?;
    sink::open_path(&dir)
}

#[tauri::command]
async fn delete_capture(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    let mut g = state.lock();
    g.refresh_sink();
    let dir = paths::resolve_capture(&g.settings.captures_dir, &id)?;
    if matches!(g.mux.get(&id), Some(MuxState::Running)) {
        return Err("a mux of this capture is running".into());
    }
    if captures::dir_is_live(&dir, g.sink_run(), SystemTime::now()) {
        return Err("the capture is still receiving data; stop the sink or wait".into());
    }
    // remove_dir_all does not follow links inside the directory.
    fs::remove_dir_all(&dir).map_err(|e| format!("cannot delete the capture: {e}"))?;
    g.mux.remove(&id);
    g.index_cache.remove(&id);
    Ok(())
}

#[tauri::command]
async fn browser_register(state: State<'_, AppState>, browser: String) -> CmdResult<()> {
    let browser = Browser::parse(&browser)?;
    let host = sink::find_host()
        .ok_or("streamtap-host was not found next to the app (build it: scripts/build.ps1)")?;
    let dir = state.lock().config_dir.clone();
    registration::register(&mut registration::WinRegistry::real(), &dir, browser, &host)
}

#[tauri::command]
async fn browser_unregister(state: State<'_, AppState>, browser: String) -> CmdResult<()> {
    let browser = Browser::parse(&browser)?;
    let dir = state.lock().config_dir.clone();
    registration::unregister(&mut registration::WinRegistry::real(), &dir, browser)
}

fn hub_of(state: &AppState) -> Arc<Hub> {
    Arc::clone(&state.lock().hub)
}

#[tauri::command]
async fn browser_decide(state: State<'_, AppState>, conn: u64, approve: bool) -> CmdResult<()> {
    hub_of(state.inner()).decide(conn, approve)
}

#[tauri::command]
async fn browser_revoke(state: State<'_, AppState>, instance: String) -> CmdResult<()> {
    revoke_browser(state.inner(), &instance, sink::find_sink())
}

/// Takes the approval of `instance` back and cuts it off at once: the sink
/// gets a new token (like *New token*), which is saved and pushed to every
/// browser that is still approved and connected (the revoked one was closed
/// first and gets nothing). A running sink is restarted with the new token,
/// which ends its captures in progress; `sink_exe` is the program to start
/// it again. Everything happens under one lock.
fn revoke_browser(shared: &AppState, instance: &str, sink_exe: Option<PathBuf>) -> CmdResult<()> {
    let mut g = shared.lock();
    let hub = Arc::clone(&g.hub);
    hub.revoke(instance)?;
    let new = Settings {
        token: secret::Token::generate(),
        ..g.settings.clone()
    };
    apply_settings(&mut g, new)
        .map_err(|e| format!("revoked, but the sink token could not be changed: {e}"))?;
    g.refresh_sink();
    if !g.sink_running() {
        return Ok(());
    }
    g.stop_sink();
    let exe = sink_exe.ok_or(
        "revoked and the token changed; the sink was stopped and cannot be started again (streamtap-sink not found)",
    )?;
    launch_sink_locked(shared, &mut g, &exe).map_err(|e| {
        format!("revoked and the token changed, but the sink did not start again: {e}")
    })
}

#[tauri::command]
async fn browser_tap(state: State<'_, AppState>, conn: u64, tab: u32, on: bool) -> CmdResult<()> {
    let hub = {
        let mut g = state.lock();
        g.refresh_sink();
        if on && !g.sink_running() {
            return Err("start the sink first".into());
        }
        Arc::clone(&g.hub)
    };
    hub.request_tap(conn, tab, on)
}

/// What a question about the phones needs: the sink must run, and with a tailnet name. The lock is not held while
/// the sink is asked.
fn phone_target(shared: &AppState) -> CmdResult<(u16, secret::Token, String)> {
    let mut g = shared.lock();
    g.refresh_sink();
    if g.settings.tailnet.is_empty() {
        return Err("set the name of this PC in the tailnet first".into());
    }
    if !g.sink_running() {
        return Err("start the sink first".into());
    }
    Ok((
        g.settings.port,
        g.settings.token.clone(),
        g.settings.tailnet.clone(),
    ))
}

/// A one-time pairing code of the running sink, as a link, its QR code and the code to type.
fn phone_pairing(shared: &AppState, exe: &Path) -> CmdResult<phones::Pairing> {
    let (port, token, tailnet) = phone_target(shared)?;
    phones::pairing_of(
        &phones::ask(exe, port, &token, &phones::Op::Pair)?,
        &tailnet,
    )
}

/// The phones paired with the running sink.
fn phone_list(shared: &AppState, exe: &Path) -> CmdResult<Vec<phones::Phone>> {
    let (port, token, _) = phone_target(shared)?;
    phones::phones_of(&phones::ask(exe, port, &token, &phones::Op::List)?)
}

/// Revokes one phone: the sink forgets it and closes its connections; the others and the token stay.
fn phone_revoked(shared: &AppState, exe: &Path, id: &str) -> CmdResult<()> {
    let (port, token, _) = phone_target(shared)?;
    phones::ask(exe, port, &token, &phones::Op::Revoke(id.to_owned())).map(|_| ())
}

/// Runs a question to the sink off the async runtime: it waits for a child process.
async fn phone_job<T: Send + 'static>(
    state: State<'_, AppState>,
    job: impl FnOnce(&AppState, &Path) -> CmdResult<T> + Send + 'static,
) -> CmdResult<T> {
    let exe = sink::find_sink().ok_or("streamtap-sink was not found next to the app")?;
    let shared = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || job(&shared, &exe))
        .await
        .map_err(|e| format!("the question to the sink was interrupted: {e}"))?
}

#[tauri::command]
async fn phone_pair(state: State<'_, AppState>) -> CmdResult<phones::Pairing> {
    phone_job(state, phone_pairing).await
}

#[tauri::command]
async fn phone_devices(state: State<'_, AppState>) -> CmdResult<Vec<phones::Phone>> {
    phone_job(state, phone_list).await
}

#[tauri::command]
async fn phone_revoke(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    phone_job(state, move |shared, exe| phone_revoked(shared, exe, &id)).await
}

fn main() {
    let app = tauri::Builder::default()
        .setup(|app| {
            let config_dir = app.path().app_config_dir()?;
            let data_dir = app.path().app_data_dir()?;
            let config_path = config_dir.join("config.json");
            let (settings, config_error) = match config::load_or_create(&config_path, &data_dir) {
                Ok(s) => (s, None),
                // The file stays as it is until the user saves new settings.
                Err(e) => (Settings::defaults(&data_dir), Some(e)),
            };
            let mut inner = Inner::new(settings, config_path, config_error);
            inner.check_config_access();
            match hub::Server::start(&inner.hub, &inner.config_dir) {
                Ok(server) => inner.bridge = Some(server),
                Err(e) => inner.bridge_error = Some(e),
            }
            app.manage(AppState(Arc::new(Mutex::new(inner))));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            snapshot,
            get_settings,
            save_settings,
            save_ui_prefs,
            regenerate_token,
            start_sink,
            stop_sink,
            mux_capture,
            open_result,
            open_folder,
            delete_capture,
            browser_register,
            browser_unregister,
            browser_decide,
            browser_revoke,
            browser_tap,
            phone_pair,
            phone_devices,
            phone_revoke,
        ])
        .build(tauri::generate_context!())
        .expect("cannot build the app");
    app.run(|handle, event| {
        if let RunEvent::Exit = event {
            if let Some(state) = handle.try_state::<AppState>() {
                state.shutdown();
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(name: &str) -> (AppState, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("streamtap-app-main-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let settings = Settings::defaults(&dir);
        let inner = Inner::new(settings, dir.join("config.json"), None);
        (AppState(Arc::new(Mutex::new(inner))), dir)
    }

    #[test]
    fn output_of_an_earlier_sink_does_not_enter_the_new_log() {
        let (st, dir) = state("loggen");
        let root = dir.join("captures");
        st.lock().log_gen = 2;
        on_sink_line(&st, "late line of sink 1", "tok", &root, 1);
        on_sink_line(&st, "line of sink 2", "tok", &root, 2);
        let log: Vec<String> = st.lock().log.iter().cloned().collect();
        assert_eq!(log, ["line of sink 2"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_forged_drm_line_flags_nothing() {
        let (st, dir) = state("drmforge");
        let root = dir.join("captures");
        let victim = root.join("1730000000_victim");
        fs::create_dir_all(&victim).unwrap();
        st.lock().log_gen = 1;
        let forged = format!(
            "127.0.0.1:1: session {} (/1730000000_victim: sb1 carries encrypted media (DRM))",
            root.join("1730000001_attack").display()
        );
        on_sink_line(&st, &forged, "tok", &root, 1);
        assert!(!victim.join(captures::DRM_FLAG).exists());
        let real = format!(
            "{}: sb1 carries encrypted media (DRM); not capturing this track",
            victim.display()
        );
        on_sink_line(&st, &real, "tok", &root, 1);
        assert!(victim.join(captures::DRM_FLAG).exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    /// The start-up check of `config.json` (SRV-14): a log line and a snapshot
    /// field once Everyone may read it. (Before the grant the temp folder's
    /// own rights decide; a CI runner's may already include Users.)
    #[cfg(windows)]
    #[test]
    fn a_config_readable_by_everyone_is_reported_at_start() {
        let (st, dir) = state("config-access");
        let path = dir.join("config.json");
        config::save(&path, &st.lock().settings).unwrap();
        let mut g = st.lock();
        g.check_config_access();
        let everyone = |a: &Option<config_access::ConfigAccess>| matches!(a, Some(config_access::ConfigAccess::Exposed { groups }) if groups.contains(&"everyone"));
        assert!(!everyone(&g.config_access), "{:?}", g.config_access);
        assert_eq!(g.log.len(), usize::from(g.config_access.is_some()));
        g.log.clear();
        let granted = std::process::Command::new("icacls")
            .arg(&path)
            .args(["/grant", "*S-1-1-0:(R)"])
            .stdout(Stdio::null())
            .status()
            .unwrap();
        assert!(granted.success());
        g.check_config_access();
        assert!(everyone(&g.config_access), "{:?}", g.config_access);
        assert_eq!(g.log.len(), 1);
        assert!(g.log[0].starts_with("warning: ") && g.log[0].contains("everyone"));
        assert!(!g.log[0].contains(g.settings.token.expose()));
        drop(g);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_captures_folder_cannot_change_while_a_mux_runs() {
        let (st, dir) = state("muxdir");
        let mut g = st.lock();
        g.mux.insert("1_x".into(), MuxState::Running);
        let other = Settings {
            captures_dir: dir.join("other"),
            ..g.settings.clone()
        };
        assert!(apply_settings(&mut g, other.clone()).is_err());
        g.mux.insert("1_x".into(), MuxState::Ok);
        assert!(apply_settings(&mut g, other).is_ok());
        drop(g);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn ui_prefs_are_validated_strictly_and_saved() {
        use config::{Language, Theme};
        let (st, dir) = state("uiprefs");
        let mut g = st.lock();
        let before = g.settings.clone();
        for (lang, theme) in [("fr", "dark"), ("de", "sepia"), ("DE", "dark"), ("", "")] {
            assert!(
                apply_ui_prefs(&mut g, lang, theme).is_err(),
                "{lang}/{theme}"
            );
            assert!(
                g.settings.same_as(&before),
                "a refused value changes nothing"
            );
        }
        assert!(!dir.join("config.json").exists());
        apply_ui_prefs(&mut g, "de", "dark").unwrap();
        assert_eq!(
            (g.settings.language, g.settings.theme),
            (Language::De, Theme::Dark)
        );
        assert!(
            g.settings.token.expose() == before.token.expose(),
            "only the preferences change"
        );
        let stored = config::load_or_create(&dir.join("config.json"), &dir).unwrap();
        assert!(stored.same_as(&g.settings));
        drop(g);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn ui_prefs_do_not_overwrite_an_unusable_config() {
        use config::{Language, Theme};
        let (st, dir) = state("uiprefs-broken");
        let path = dir.join("config.json");
        fs::write(&path, b"{ broken").unwrap();
        let mut g = st.lock();
        g.config_error = Some("broken".into());
        assert!(apply_ui_prefs(&mut g, "en", "light").is_err());
        assert_eq!(
            (g.settings.language, g.settings.theme),
            (Language::En, Theme::Light)
        );
        assert_eq!(fs::read(&path).unwrap(), b"{ broken");
        drop(g);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// What a hub connection was sent.
    #[derive(Clone, Default)]
    struct Wire(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Wire {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl Wire {
        /// The tokens this connection was given, in order.
        fn tokens(&self) -> Vec<String> {
            let data = self.0.lock().unwrap().clone();
            String::from_utf8(data)
                .unwrap()
                .lines()
                .filter_map(|l| {
                    let v: serde_json::Value = serde_json::from_str(l).unwrap();
                    v["token"].as_str().map(str::to_owned)
                })
                .collect()
        }
    }

    /// A connected browser profile the user approved.
    fn paired(hub: &Hub, instance: &str) -> Wire {
        let w = Wire::default();
        let id = hub.attach(Box::new(w.clone()), Box::new(|| {})).unwrap();
        hub.hello(
            id,
            streamtap_host::proto::ALLOWED_ORIGIN,
            instance,
            "chrome",
            "0.3.0",
        );
        hub.decide(id, true).unwrap();
        w
    }

    #[test]
    fn revoking_a_browser_rotates_the_token_for_everyone_else() {
        let (st, dir) = state("revoke-rotate");
        let old = st.lock().settings.token.expose().to_owned();
        let hub = Arc::clone(&st.lock().hub);
        let revoked = paired(&hub, "profile-a");
        let kept = paired(&hub, "profile-b");
        assert_eq!(revoked.tokens(), std::slice::from_ref(&old));

        revoke_browser(&st, "profile-a", None).unwrap();
        let new = st.lock().settings.token.expose().to_owned();
        assert_ne!(new, old, "a new token");
        assert_eq!(new.len(), 64);
        assert_eq!(
            revoked.tokens(),
            std::slice::from_ref(&old),
            "the revoked one gets nothing"
        );
        assert_eq!(kept.tokens(), [old, new.clone()], "the others get it");
        let stored = config::load_or_create(&dir.join("config.json"), &dir).unwrap();
        assert_eq!(stored.token.expose(), new, "the new token is saved");
        assert!(!st.lock().sink_running(), "a stopped sink stays stopped");
        assert!(hub
            .view()
            .approvals
            .iter()
            .all(|a| a.instance != "profile-a"));
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A mux that could not be put into a job is noted in the app's log.
    #[test]
    fn an_untied_mux_is_noted_in_the_log() {
        let (st, dir) = state("mux-untied");
        let shared = st.clone();
        sink::run_child(
            sink::tests::quick_command(),
            None,
            |_| Err(std::io::Error::other("job denied")),
            |h| mux_started(&shared, "1_x", h),
        );
        let log: Vec<String> = st.lock().log.iter().cloned().collect();
        assert_eq!(log.len(), 1, "{log:?}");
        assert!(log[0].starts_with("warning: the mux of 1_x is not tied"));
        assert!(log[0].ends_with("job denied"));
        assert!(st.lock().mux_handles.contains_key("1_x"));
        fs::remove_dir_all(&dir).unwrap();
    }

    // ---- end to end against the real sink ----

    /// The sink exe next to the test exe's profile directory, built first.
    fn built_sink() -> PathBuf {
        built_bin("streamtap-sink")
    }

    /// `package`'s exe in the test exe's profile directory, built first (the
    /// package and the exe have the same name).
    fn built_bin(package: &str) -> PathBuf {
        let exe = std::env::current_exe().unwrap();
        let profile_dir = exe.parent().and_then(Path::parent).unwrap().to_path_buf();
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let status = std::process::Command::new(cargo)
            .args(["build", "-p", package, "--locked"])
            .args(build_profile_args(&profile_dir))
            .status()
            .expect("cargo build");
        assert!(status.success(), "cargo build -p {package} failed");
        let bin = profile_dir.join(if cfg!(windows) {
            format!("{package}.exe")
        } else {
            package.to_string()
        });
        assert!(bin.is_file(), "{} missing", bin.display());
        bin
    }

    /// The profile the test exe was built with, from its directory
    /// (`target/debug`, `target/release`, `target/<custom>`): the sink must be
    /// built the same way or the test would start a stale or missing exe.
    fn build_profile_args(profile_dir: &Path) -> Vec<String> {
        match profile_dir.file_name().and_then(|n| n.to_str()) {
            Some("debug") | None => Vec::new(),
            Some(name) => vec!["--profile".into(), name.into()],
        }
    }

    #[test]
    fn the_sink_is_built_with_the_profile_of_the_test() {
        assert_eq!(build_profile_args(Path::new("target/debug")).len(), 0);
        assert_eq!(
            build_profile_args(Path::new("target/release")),
            ["--profile", "release"]
        );
        assert_eq!(
            build_profile_args(Path::new("target/ci-fast")),
            ["--profile", "ci-fast"]
        );
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    fn wait_for<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
        let end = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while std::time::Instant::now() < end {
            if let Some(v) = f() {
                return v;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        panic!("timed out waiting for {what}");
    }

    /// One wire message of the extension: u32 LE header length, JSON header, payload.
    fn frame(header: &serde_json::Value, payload: &[u8]) -> Vec<u8> {
        let h = serde_json::to_vec(header).unwrap();
        let mut out = u32::try_from(h.len()).unwrap().to_le_bytes().to_vec();
        out.extend(h);
        out.extend_from_slice(payload);
        out
    }

    fn mp4_box(kind: [u8; 4], body: &[u8]) -> Vec<u8> {
        let mut b = u32::try_from(8 + body.len())
            .unwrap()
            .to_be_bytes()
            .to_vec();
        b.extend_from_slice(&kind);
        b.extend_from_slice(body);
        b
    }

    /// An init segment (`ftyp` + `moov`) that declares an encrypted track.
    fn encrypted_init() -> Vec<u8> {
        let tenc = mp4_box(*b"tenc", &[0; 24]);
        let encv = mp4_box(*b"encv", &tenc);
        let pssh = mp4_box(*b"pssh", &[0; 24]);
        let mut init = mp4_box(*b"ftyp", b"isom\0\0\0\0isom");
        init.extend(mp4_box(*b"moov", &[encv, pssh].concat()));
        init
    }

    /// The same init without the encryption boxes.
    fn clear_init() -> Vec<u8> {
        let mut init = mp4_box(*b"ftyp", b"isom\0\0\0\0isom");
        init.extend(mp4_box(*b"moov", &mp4_box(*b"mvhd", &[0; 100])));
        init
    }

    fn scan_one(st: &AppState, root: &Path) -> Option<captures::CaptureInfo> {
        let mut g = st.lock();
        let run = g.sink_run();
        let mut list = captures::scan(root, &mut g.index_cache, run, SystemTime::now()).unwrap();
        assert!(list.len() <= 1, "at most one capture");
        (!list.is_empty()).then(|| list.remove(0))
    }

    /// What a capture looks like after the page closed its connection.
    struct Finished {
        done: captures::CaptureInfo,
        st: AppState,
        root: PathBuf,
        dir: PathBuf,
    }

    /// Starts the real sink, sends `init` as the first segment of a track over
    /// a WebSocket like the extension does, and closes the connection. The
    /// capture is live while it is open and done at once after the close.
    fn capture_with_init(name: &str, init: &[u8]) -> Finished {
        use tungstenite::client::IntoClientRequest;
        let sink = built_sink();
        let (st, dir) = state(name);
        let port = free_port();
        let root = dir.join("captures");
        {
            let mut g = st.lock();
            g.settings.port = port;
            g.settings.captures_dir = root.clone();
        }
        launch_sink(&st, &sink).expect("start the sink");
        let token = st.lock().settings.token.clone();

        let mut request = format!("ws://127.0.0.1:{port}/")
            .into_client_request()
            .unwrap();
        request.headers_mut().insert(
            "Origin",
            "chrome-extension://oggpddakdbffbjhkjmcakidgmhpgakia"
                .parse()
                .unwrap(),
        );
        let mut ws = wait_for("the sink to accept", || {
            tungstenite::connect(request.clone()).ok().map(|(ws, _)| ws)
        });
        let hello = serde_json::json!({
            "type": "hello", "token": token, "session": name,
            "url": "https://video.example/watch", "title": "e2e",
        });
        ws.send(tungstenite::Message::binary(frame(&hello, b"")))
            .unwrap();
        let header = serde_json::json!({
            "type": "segment", "sb": 1, "seq": 1, "isInit": true, "kind": "mp4",
            "mime": "video/mp4; codecs=\"avc1.64001e\"", "timescale": 1000,
        });
        ws.send(tungstenite::Message::binary(frame(&header, init)))
            .unwrap();

        // The connection is open: the capture is live, exactly.
        let live = wait_for("a live capture", || scan_one(&st, &root).filter(|c| c.live));
        assert_eq!(live.title, "e2e");

        // The page closes the connection: done at once, although the files
        // were just written (the old 10 s heuristic would still say live).
        ws.close(None).unwrap();
        while ws.read().is_ok() {}
        drop(ws);
        let closed_at = std::time::Instant::now();
        let done = wait_for("the capture to be done", || {
            scan_one(&st, &root).filter(|c| !c.live)
        });
        assert!(closed_at.elapsed() < captures::LIVE_WINDOW);
        Finished {
            done,
            st,
            root,
            dir,
        }
    }

    #[test]
    fn end_to_end_an_encrypted_init_through_the_real_sink_flags_the_capture() {
        let f = capture_with_init("e2e-drm", &encrypted_init());
        // The sink logged the refusal, the app read it and flagged the capture.
        let flagged = wait_for("the DRM flag", || {
            scan_one(&f.st, &f.root).filter(|c| c.drm)
        });
        assert!(flagged.drm && !flagged.live);
        assert_eq!(f.done.id, flagged.id);
        // An init counts as bytes, not as a segment.
        assert_eq!(flagged.bytes, 0, "nothing of the encrypted track is stored");
        f.st.shutdown();
        fs::remove_dir_all(&f.dir).unwrap();
    }

    /// The positive control of the test above: the same path with a clear
    /// init is stored and not flagged, so the DRM test cannot pass merely
    /// because the connection, the framing or the init storage is broken.
    #[test]
    fn end_to_end_a_clear_init_through_the_real_sink_is_stored_and_not_flagged() {
        let f = capture_with_init("e2e-clear", &clear_init());
        assert!(f.done.bytes > 0, "the clear init is stored");
        // The DRM line, had there been one, was printed before the close.
        std::thread::sleep(std::time::Duration::from_millis(500));
        let again = scan_one(&f.st, &f.root).unwrap();
        assert!(!again.drm, "a clear track is no DRM");
        f.st.shutdown();
        fs::remove_dir_all(&f.dir).unwrap();
    }

    /// A revoke while the real sink runs: the sink is started again with the
    /// new token (another process, the old one is gone).
    #[test]
    fn end_to_end_revoking_restarts_a_running_sink_with_the_new_token() {
        let sink = built_sink();
        let (st, dir) = state("e2e-revoke");
        {
            let mut g = st.lock();
            g.settings.port = free_port();
            g.settings.captures_dir = dir.join("captures");
        }
        launch_sink(&st, &sink).expect("start the sink");
        let old_pid = st.lock().sink_run().unwrap().pid;
        let hub = Arc::clone(&st.lock().hub);
        let kept = paired(&hub, "profile-b");
        let revoked = paired(&hub, "profile-a");

        revoke_browser(&st, "profile-a", Some(sink.clone())).unwrap();
        let (new_pid, token) = {
            let g = st.lock();
            (
                g.sink_run().map(|r| r.pid),
                g.settings.token.expose().to_owned(),
            )
        };
        assert!(new_pid.is_some_and(|p| p != old_pid), "the sink restarted");
        assert_eq!(kept.tokens().last(), Some(&token));
        assert!(!revoked.tokens().contains(&token));
        std::thread::sleep(std::time::Duration::from_millis(500));
        let mut g = st.lock();
        g.refresh_sink();
        assert!(g.sink_running(), "the new sink keeps running");
        drop(g);
        // Without the sink program the revoke still happens, the sink is
        // stopped and the caller is told.
        let _ = paired(&hub, "profile-c");
        let err = revoke_browser(&st, "profile-c", None).unwrap_err();
        assert!(err.contains("cannot be started again"), "{err}");
        assert!(!st.lock().sink_running());
        st.shutdown();
        fs::remove_dir_all(&dir).unwrap();
    }

    /// The phones of the running sink through the app's own functions: a pairing code with its link and QR code, a
    /// phone that pairs with it (a WebSocket as Tailscale Serve forwards one), the list, and the revocation of that
    /// one phone. The list of devices lies with the app's settings, not among the captures.
    #[test]
    fn end_to_end_a_phone_is_paired_listed_and_revoked_through_the_real_sink() {
        use tungstenite::client::IntoClientRequest;
        const NAME: &str = "pc.example.ts.net";
        let sink = built_sink();
        let (st, dir) = state("e2e-phones");
        assert!(phone_list(&st, &sink).unwrap_err().contains("tailnet"));
        {
            let mut g = st.lock();
            g.settings.port = free_port();
            g.settings.captures_dir = dir.join("captures");
            g.settings.tailnet = NAME.into();
        }
        assert_eq!(phone_list(&st, &sink).unwrap_err(), "start the sink first");
        launch_sink(&st, &sink).expect("start the sink");
        let pairing = wait_for("a pairing code of the sink", || {
            phone_pairing(&st, &sink).ok()
        });
        assert_eq!(pairing.tailnet, NAME);
        assert_eq!(
            pairing.link,
            format!("https://{NAME}/#streamtap-pair=1&code={}", pairing.code)
        );
        assert!(pairing.qr.len() >= 21 && pairing.qr.iter().all(|r| r.len() == pairing.qr.len()));
        assert_eq!(phone_list(&st, &sink).unwrap(), Vec::new());

        // A phone: the tailnet name as Host, the proxy's header, an add-on's origin; its key is the curve's base point.
        let port = st.lock().settings.port;
        let mut request = format!("ws://127.0.0.1:{port}/")
            .into_client_request()
            .unwrap();
        for (name, value) in [
            ("host", NAME),
            (
                "origin",
                "moz-extension://0f3c1a52-7b1e-4d0c-9a3e-2f6f1f0e9c11",
            ),
            ("x-forwarded-for", "100.64.0.7"),
        ] {
            request.headers_mut().insert(name, value.parse().unwrap());
        }
        let stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        let (mut ws, _) = tungstenite::client(request, stream).expect("the phone's handshake");
        let pair = serde_json::json!({
            "type": "pair", "v": 1, "code": pairing.code, "name": "e2e phone",
            "key": "046b17d1f2e12c4247f8bce6e563a440f277037d812deb33a0f4a13945d898c2964fe342e2fe1a7f9b8ee7eb4a7c0f9e162bce33576b315ececbb6406837bf51f5",
        });
        ws.send(tungstenite::Message::binary(frame(&pair, b"")))
            .unwrap();
        let said: serde_json::Value = loop {
            if let tungstenite::Message::Text(t) = ws.read().expect("the sink's answer") {
                break serde_json::from_str(t.as_str()).unwrap();
            }
        };
        assert_eq!(said["type"], "paired", "{said}");
        let id = said["device"].as_str().unwrap().to_owned();

        let list = phone_list(&st, &sink).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(
            (list[0].id.as_str(), list[0].name.as_str()),
            (id.as_str(), "e2e phone")
        );
        assert!(dir.join("devices.json").is_file());
        assert!(!dir.join("captures").join("devices.json").exists());
        // The token never appears in what the sink printed.
        let token = st.lock().settings.token.expose().to_owned();
        assert!(st.lock().log.iter().all(|l| !l.contains(&token)));

        phone_revoked(&st, &sink, &id).expect("revoke");
        assert_eq!(phone_list(&st, &sink).unwrap(), Vec::new());
        let again = phone_revoked(&st, &sink, &id).unwrap_err();
        assert!(again.contains("no paired device"), "{again}");
        assert_eq!(
            phone_revoked(&st, &sink, "--port").unwrap_err(),
            "not a device id"
        );
        st.shutdown();
        assert_eq!(phone_list(&st, &sink).unwrap_err(), "start the sink first");
        fs::remove_dir_all(&dir).unwrap();
    }

    // ---- end to end: the real host against the real bridge ----

    /// Frames read from a child's stdout, delivered through a channel so a
    /// test cannot hang on a silent host.
    fn frames_of(
        stdout: std::process::ChildStdout,
    ) -> std::sync::mpsc::Receiver<serde_json::Value> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut out = stdout;
            while let Ok(Some(body)) = streamtap_host::framing::read_frame(&mut out, 1 << 20) {
                let value = serde_json::from_slice(&body).unwrap();
                if tx.send(value).is_err() {
                    break;
                }
            }
        });
        rx
    }

    fn native_frame(json: &serde_json::Value) -> Vec<u8> {
        let body = serde_json::to_vec(json).unwrap();
        let mut out = u32::try_from(body.len()).unwrap().to_le_bytes().to_vec();
        out.extend(body);
        out
    }

    fn next_frame(rx: &std::sync::mpsc::Receiver<serde_json::Value>) -> serde_json::Value {
        rx.recv_timeout(std::time::Duration::from_secs(15))
            .expect("a frame from the host")
    }

    /// A real host that read another key (a stale or forged `bridge.json`)
    /// gets no slot and no prompt, and does not take this app for its own:
    /// the app's proof does not hold for the key the host read, so the host
    /// tells the extension that the app is not running. The file is put back
    /// afterwards.
    fn a_wrong_key_is_turned_away(
        dir: &Path,
        hub: &Hub,
        launch: &dyn Fn(&str) -> std::process::Child,
    ) {
        use std::io::Write as _;
        let bridge_file = dir.join(streamtap_host::proto::BRIDGE_FILE);
        let real = fs::read_to_string(&bridge_file).unwrap();
        let bridge = streamtap_host::relay::read_bridge(dir).unwrap();
        fs::write(
            &bridge_file,
            serde_json::json!({"port": bridge.port, "key": "ab".repeat(32)}).to_string(),
        )
        .unwrap();
        let mut forged = launch("chrome-extension://oggpddakdbffbjhkjmcakidgmhpgakia/");
        let rx = frames_of(forged.stdout.take().unwrap());
        let mut stdin = forged.stdin.take().unwrap();
        let hello = serde_json::json!({
            "t": "hello", "v": 1, "instance": "e2e-forged", "browser": "chrome", "version": "0.3.0",
        });
        stdin.write_all(&native_frame(&hello)).unwrap();
        stdin.flush().unwrap();
        // The handshake fails on the host's side: it leaves, and says that
        // its app is not running.
        wait_for("the turned-away host to exit", || {
            forged.try_wait().unwrap()
        });
        let said: Vec<serde_json::Value> = rx.try_iter().collect();
        assert!(
            said.len() == 1
                && said
                    .iter()
                    .all(|m| m["t"] == "error" && m["code"] == "app-not-running"),
            "{said:?}"
        );
        assert!(
            hub.view().connections.is_empty(),
            "no prompt for a wrong key"
        );
        drop(stdin);
        fs::write(&bridge_file, real).unwrap();
    }

    #[test]
    fn end_to_end_the_real_host_pairs_through_the_bridge() {
        use std::io::Write as _;
        let host = built_bin("streamtap-host");
        let (st, dir) = state("e2e-host");
        let hub = Arc::clone(&st.lock().hub);
        let server = hub::Server::start(&hub, &dir).unwrap();
        st.lock().bridge = Some(server);
        let token = st.lock().settings.token.clone();
        let launch = |origin: &str| {
            std::process::Command::new(&host)
                .arg(origin)
                .env("STREAMTAP_APP_DIR", &dir)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap()
        };

        // Another origin: refused by the host itself, no bridge involved.
        let mut bad = launch("chrome-extension://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/");
        let rx = frames_of(bad.stdout.take().unwrap());
        assert_eq!(next_frame(&rx)["code"], "origin-not-allowed");
        assert_eq!(bad.wait().unwrap().code(), Some(2));
        assert!(hub.view().connections.is_empty());

        a_wrong_key_is_turned_away(&dir, &hub, &launch);

        let mut child = launch("chrome-extension://oggpddakdbffbjhkjmcakidgmhpgakia/");
        let rx = frames_of(child.stdout.take().unwrap());
        let mut stdin = child.stdin.take().unwrap();
        let hello = serde_json::json!({
            "t": "hello", "v": 1, "instance": "e2e-profile-1", "browser": "brave", "version": "0.3.0",
        });
        stdin.write_all(&native_frame(&hello)).unwrap();
        stdin.flush().unwrap();
        assert_eq!(
            next_frame(&rx),
            serde_json::json!({"t": "state", "v": 1, "pairing": "pending"})
        );
        // Until the user approves, nothing else comes, least of all the token
        // (and the host is still there: a closed channel would prove nothing).
        assert_eq!(
            rx.recv_timeout(std::time::Duration::from_millis(400)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        );
        let conn = wait_for("the pending connection", || {
            hub.view().connections.first().map(|c| (c.id, c.approved))
        });
        assert!(!conn.1);

        hub.decide(conn.0, true).unwrap();
        assert_eq!(next_frame(&rx)["pairing"], "approved");
        let creds = next_frame(&rx);
        assert_eq!(creds["t"], "credentials");
        assert_eq!(creds["token"], token.expose());
        assert_eq!(creds["port"], st.lock().settings.port);

        let tabs = serde_json::json!({"t": "tabs", "v": 1, "tabs": [
            {"id": 11, "title": "A video", "host": "video.example", "favicon": null,
             "drm": false, "tapping": false, "active": true}]});
        stdin.write_all(&native_frame(&tabs)).unwrap();
        stdin.flush().unwrap();
        wait_for("the tab list", || {
            hub.view()
                .connections
                .first()
                .filter(|c| c.tabs.len() == 1)
                .map(|_| ())
        });
        hub.request_tap(conn.0, 11, true).unwrap();
        assert_eq!(
            next_frame(&rx),
            serde_json::json!({"t": "tap", "v": 1, "tab": 11, "on": true})
        );

        // Revoking ends the link: the extension is told, the host leaves.
        hub.revoke("e2e-profile-1").unwrap();
        assert_eq!(next_frame(&rx)["pairing"], "revoked");
        assert!(wait_for("the host to exit", || child.try_wait().unwrap())
            .code()
            .is_some());
        drop(stdin);
        st.shutdown();
        fs::remove_dir_all(&dir).unwrap();
    }
}
