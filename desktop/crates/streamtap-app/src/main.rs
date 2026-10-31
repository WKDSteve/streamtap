//! streamtap desktop app: a small Tauri shell around `streamtap-sink`.
//!
//! The sink stays a separate process; this app starts and stops it, reads its
//! capture directory and runs `streamtap-sink mux` on request. The webview
//! (plain files in `ui/`) can call the commands below and nothing else.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod captures;
mod config;
mod paths;
mod sink;

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::SystemTime;

use serde::Serialize;
use tauri::{Manager, RunEvent, State};

use captures::{CaptureInfo, IndexAcc};
use config::Settings;
use sink::MuxState;

type CmdResult<T> = Result<T, String>;

struct SinkProc {
    child: Child,
    pid: u32,
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
    sink: Option<SinkProc>,
    crashed: Option<SinkExit>,
    log: VecDeque<String>,
    mux: HashMap<String, MuxState>,
    index_cache: HashMap<String, IndexAcc>,
    /// Counts sink starts: output of an earlier sink process (its reader
    /// threads may still be draining) is not mixed into the new log.
    log_gen: u64,
}

#[derive(Clone)]
struct AppState(Arc<Mutex<Inner>>);

impl AppState {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Inner {
    fn new(settings: Settings, config_path: PathBuf, config_error: Option<String>) -> Self {
        Self {
            settings,
            config_path,
            config_error,
            sink: None,
            crashed: None,
            log: VecDeque::new(),
            mux: HashMap::new(),
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
    captures: Vec<CaptureInfo>,
    captures_error: Option<String>,
    mux: HashMap<String, MuxState>,
    log: Vec<String>,
}

#[tauri::command]
async fn snapshot(state: State<'_, AppState>) -> CmdResult<Snapshot> {
    let mut g = state.lock();
    g.refresh_sink();
    let running = g.sink_running();
    let sink = SinkStatus {
        state: if running {
            "running"
        } else if g.crashed.is_some() {
            "crashed"
        } else {
            "stopped"
        },
        pid: g.sink.as_ref().map(|p| p.pid),
        exit_code: g.crashed.as_ref().and_then(|c| c.code),
    };
    let root = g.settings.captures_dir.clone();
    let (captures, captures_error) = if root.is_dir() {
        match captures::scan(&root, &mut g.index_cache, running, SystemTime::now()) {
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
        captures,
        captures_error,
        mux: g.mux.clone(),
        log: g.log.iter().cloned().collect(),
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
    Ok(())
}

#[tauri::command]
async fn save_settings(
    state: State<'_, AppState>,
    port: u16,
    captures_dir: String,
) -> CmdResult<Settings> {
    let mut g = state.lock();
    require_stopped(&mut g)?;
    let new = Settings {
        port,
        token: g.settings.token.clone(),
        captures_dir: PathBuf::from(captures_dir.trim()),
    };
    apply_settings(&mut g, new)?;
    Ok(g.settings.clone())
}

#[tauri::command]
async fn regenerate_token(state: State<'_, AppState>) -> CmdResult<Settings> {
    let mut g = state.lock();
    require_stopped(&mut g)?;
    let new = Settings {
        token: config::random_token(),
        ..g.settings.clone()
    };
    apply_settings(&mut g, new)?;
    Ok(g.settings.clone())
}

#[tauri::command]
async fn start_sink(state: State<'_, AppState>) -> CmdResult<()> {
    let mut g = state.lock();
    require_stopped(&mut g)?;
    g.settings.validate()?;
    let exe = sink::find_sink()
        .ok_or("streamtap-sink was not found next to the app (build it: scripts/build.ps1)")?;
    let root = g.settings.captures_dir.clone();
    fs::create_dir_all(&root).map_err(|e| format!("cannot create {}: {e}", root.display()))?;
    let token = g.settings.token.clone();
    let mut child = sink::command(&exe)
        .args(sink::serve_args(g.settings.port, &token, &root))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot start the sink: {e}"))?;
    g.log.clear();
    g.log_gen += 1;
    let generation = g.log_gen;
    g.crashed = None;
    let pid = child.id();
    for pipe in [
        child.stdout.take().map(Pipe::Out),
        child.stderr.take().map(Pipe::Err),
    ]
    .into_iter()
    .flatten()
    {
        let shared = state.inner().clone();
        let (token, root) = (token.clone(), root.clone());
        std::thread::spawn(move || match pipe {
            Pipe::Out(p) => sink::for_each_line(p, |l| {
                on_sink_line(&shared, l, &token, &root, generation);
            }),
            Pipe::Err(p) => sink::for_each_line(p, |l| {
                on_sink_line(&shared, l, &token, &root, generation);
            }),
        });
    }
    g.sink = Some(SinkProc { child, pid });
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
        if captures::is_live(&dir, g.sink_running(), SystemTime::now()) {
            return Err("the capture is still receiving data; mux it when it is done".into());
        }
        g.mux.insert(id.clone(), MuxState::Running);
        dir
    };
    let shared = state.inner().clone();
    std::thread::spawn(move || {
        let result = sink::run_mux(&sink_exe, &dir);
        shared.lock().mux.insert(id, result);
    });
    Ok(())
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
    if captures::is_live(&dir, g.sink_running(), SystemTime::now()) {
        return Err("the capture is still receiving data; stop the sink or wait".into());
    }
    // remove_dir_all does not follow links inside the directory.
    fs::remove_dir_all(&dir).map_err(|e| format!("cannot delete the capture: {e}"))?;
    g.mux.remove(&id);
    g.index_cache.remove(&id);
    Ok(())
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
            app.manage(AppState(Arc::new(Mutex::new(Inner::new(
                settings,
                config_path,
                config_error,
            )))));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            snapshot,
            get_settings,
            save_settings,
            regenerate_token,
            start_sink,
            stop_sink,
            mux_capture,
            open_result,
            open_folder,
            delete_capture,
        ])
        .build(tauri::generate_context!())
        .expect("cannot build the app");
    app.run(|handle, event| {
        if let RunEvent::Exit = event {
            if let Some(state) = handle.try_state::<AppState>() {
                state.lock().stop_sink();
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
}
