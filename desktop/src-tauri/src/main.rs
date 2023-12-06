// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;

use std::sync::Arc;

use anyhow::Context;
use config::RqbitDesktopConfig;
use http::StatusCode;
use librqbit::{
    api::{
        ApiAddTorrentResponse, EmptyJsonResponse, TorrentDetailsResponse, TorrentListResponse,
        TorrentStats,
    },
    dht::PersistentDhtConfig,
    librqbit_spawn, AddTorrent, AddTorrentOptions, Api, ApiError, PeerConnectionOptions, Session,
    SessionOptions,
};
use parking_lot::RwLock;
use tracing::error_span;

const ERR_NOT_CONFIGURED: ApiError =
    ApiError::new_from_text(StatusCode::FAILED_DEPENDENCY, "not configured");

struct StateShared {
    config: config::RqbitDesktopConfig,
    api: Api,
    session: Arc<Session>,
}

impl StateShared {}

struct State {
    shared: Arc<RwLock<Option<StateShared>>>,
    rust_log_reload_tx: tokio::sync::mpsc::UnboundedSender<String>,
}

impl State {
    fn api(&self) -> Result<Api, ApiError> {
        let g = self.shared.read();
        match &*g {
            Some(s) => Ok(s.api.clone()),
            None => Err(ERR_NOT_CONFIGURED),
        }
    }

    fn current_config(&self) -> Option<RqbitDesktopConfig> {
        self.shared.read().as_ref().map(|s| s.config.clone())
    }

    async fn configure(&self, config: RqbitDesktopConfig) -> Result<(), ApiError> {
        let existing = self.shared.write().take();

        if let Some(existing) = existing {
            existing.session.stop().await;
        }

        let config_clone = config.clone();

        let session = Session::new_with_opts(
            config.default_download_location,
            SessionOptions {
                disable_dht: config.dht.disable,
                disable_dht_persistence: config.dht.disable_persistence,
                dht_config: Some(PersistentDhtConfig {
                    config_filename: Some(config.dht.persistence_filename),
                    ..Default::default()
                }),
                persistence: !config.persistence.disable,
                persistence_filename: Some(config.persistence.filename),
                peer_opts: Some(PeerConnectionOptions {
                    connect_timeout: Some(config.peer_opts.connect_timeout),
                    read_write_timeout: Some(config.peer_opts.read_write_timeout),
                    ..Default::default()
                }),
                listen_port_range: if !config.tcp_listen.disable {
                    Some(config.tcp_listen.min_port..config.tcp_listen.max_port)
                } else {
                    None
                },
                enable_upnp_port_forwarding: !config.upnp.disable,
                ..Default::default()
            },
        )
        .await
        .context("couldn't set up librqbit session")?;

        let api = Api::new(session.clone(), None);

        if !config.http_api.disable {
            let http_api_task = librqbit::http_api::HttpApi::new(
                session.clone(),
                Some(self.rust_log_reload_tx.clone()),
            )
            .make_http_api_and_run(config.http_api.listen_addr, config.http_api.read_only);

            session.spawn("http api", error_span!("http_api"), http_api_task);
        }

        *self.shared.write() = Some(StateShared {
            config: config_clone,
            api,
            session,
        });

        Ok(())
    }
}

#[tauri::command]
fn config_default() -> config::RqbitDesktopConfig {
    config::RqbitDesktopConfig::default()
}

#[tauri::command]
fn config_current(state: tauri::State<'_, State>) -> Option<config::RqbitDesktopConfig> {
    state.current_config()
}

#[tauri::command]
async fn config_change(
    state: tauri::State<'_, State>,
    config: RqbitDesktopConfig,
) -> Result<EmptyJsonResponse, ApiError> {
    state.configure(config).await.map(|_| EmptyJsonResponse {})
}

#[tauri::command]
fn torrents_list(state: tauri::State<State>) -> Result<TorrentListResponse, ApiError> {
    Ok(state.api()?.api_torrent_list())
}

#[tauri::command]
async fn torrent_create_from_url(
    state: tauri::State<'_, State>,
    url: String,
    opts: Option<AddTorrentOptions>,
) -> Result<ApiAddTorrentResponse, ApiError> {
    state
        .api()?
        .api_add_torrent(AddTorrent::Url(url.into()), opts)
        .await
}

#[tauri::command]
async fn torrent_create_from_base64_file(
    state: tauri::State<'_, State>,
    contents: String,
    opts: Option<AddTorrentOptions>,
) -> Result<ApiAddTorrentResponse, ApiError> {
    use base64::{engine::general_purpose, Engine as _};
    let bytes = general_purpose::STANDARD
        .decode(&contents)
        .context("invalid base64")
        .map_err(|e| ApiError::new_from_anyhow(StatusCode::BAD_REQUEST, e))?;
    state
        .api()?
        .api_add_torrent(AddTorrent::TorrentFileBytes(bytes.into()), opts)
        .await
}

#[tauri::command]
async fn torrent_details(
    state: tauri::State<'_, State>,
    id: usize,
) -> Result<TorrentDetailsResponse, ApiError> {
    state.api()?.api_torrent_details(id)
}

#[tauri::command]
async fn torrent_stats(
    state: tauri::State<'_, State>,
    id: usize,
) -> Result<TorrentStats, ApiError> {
    state.api()?.api_stats_v1(id)
}

#[tauri::command]
async fn torrent_action_delete(
    state: tauri::State<'_, State>,
    id: usize,
) -> Result<EmptyJsonResponse, ApiError> {
    state.api()?.api_torrent_action_delete(id)
}

#[tauri::command]
async fn torrent_action_pause(
    state: tauri::State<'_, State>,
    id: usize,
) -> Result<EmptyJsonResponse, ApiError> {
    state.api()?.api_torrent_action_pause(id)
}

#[tauri::command]
async fn torrent_action_forget(
    state: tauri::State<'_, State>,
    id: usize,
) -> Result<EmptyJsonResponse, ApiError> {
    state.api()?.api_torrent_action_forget(id)
}

#[tauri::command]
async fn torrent_action_start(
    state: tauri::State<'_, State>,
    id: usize,
) -> Result<EmptyJsonResponse, ApiError> {
    state.api()?.api_torrent_action_start(id)
}

#[tauri::command]
fn get_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

fn init_logging() -> tokio::sync::mpsc::UnboundedSender<String> {
    use tracing_subscriber::{fmt, prelude::*, EnvFilter};
    let (stderr_filter, reload_stderr_filter) =
        tracing_subscriber::reload::Layer::new(EnvFilter::builder().parse("info").unwrap());

    let layered = tracing_subscriber::registry().with(fmt::layer().with_filter(stderr_filter));
    layered.init();

    let (reload_tx, mut reload_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    librqbit_spawn(
        "fmt_filter_reloader",
        error_span!("fmt_filter_reloader"),
        async move {
            while let Some(rust_log) = reload_rx.recv().await {
                let stderr_env_filter = match EnvFilter::builder().parse(&rust_log) {
                    Ok(f) => f,
                    Err(e) => {
                        eprintln!("can't parse env filter {:?}: {:#?}", rust_log, e);
                        continue;
                    }
                };
                eprintln!("setting RUST_LOG to {:?}", rust_log);
                let _ = reload_stderr_filter.reload(stderr_env_filter);
            }
            Ok(())
        },
    );
    reload_tx
}

async fn start() {
    tauri::async_runtime::set(tokio::runtime::Handle::current());
    let rust_log_reload_tx = init_logging();

    tauri::Builder::default()
        .manage(State {
            shared: Arc::new(RwLock::new(None)),
            rust_log_reload_tx,
        })
        .invoke_handler(tauri::generate_handler![
            torrents_list,
            torrent_details,
            torrent_stats,
            torrent_create_from_url,
            torrent_action_delete,
            torrent_action_pause,
            torrent_action_forget,
            torrent_action_start,
            torrent_create_from_base64_file,
            get_version,
            config_default,
            config_current,
            config_change,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

fn main() {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("couldn't set up tokio runtime")
        .block_on(start())
}
