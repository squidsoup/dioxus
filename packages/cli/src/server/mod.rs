use crate::{builder, serve::Serve, BuildResult, CrateConfig, Result};
use axum::{
    body::{Full, HttpBody},
    extract::{ws::Message, Extension, TypedHeader, WebSocketUpgrade},
    http::{
        header::{HeaderName, HeaderValue},
        Method, Response, StatusCode,
    },
    response::IntoResponse,
    routing::{get, get_service},
    Router,
};
use axum_server::tls_rustls::RustlsConfig;
use cargo_metadata::diagnostic::Diagnostic;
use dioxus_core::Template;
use dioxus_html::HtmlCtx;
use dioxus_rsx::hot_reload::*;
use notify::{RecommendedWatcher, Watcher};
use std::{
    net::UdpSocket,
    path::PathBuf,
    process::Command,
    sync::{Arc, Mutex},
};
use tokio::sync::broadcast::{self, Sender};
use tower::ServiceBuilder;
use tower_http::services::fs::{ServeDir, ServeFileSystemResponseBody};
use tower_http::{
    cors::{Any, CorsLayer},
    ServiceBuilderExt,
};

#[cfg(feature = "plugin")]
use plugin::PluginManager;

mod proxy;

mod hot_reload;
use hot_reload::*;

mod output;
use output::*;

pub struct BuildManager {
    config: CrateConfig,
    reload_tx: broadcast::Sender<()>,
}

impl BuildManager {
    fn rebuild(&self) -> Result<BuildResult> {
        log::info!("🪁 Rebuild project");
        let result = builder::build(&self.config, true)?;
        // change the websocket reload state to true;
        // the page will auto-reload.
        if self
            .config
            .dioxus_config
            .web
            .watcher
            .reload_html
            .unwrap_or(false)
        {
            let _ = Serve::regen_dev_page(&self.config);
        }
        let _ = self.reload_tx.send(());
        Ok(result)
    }
}

struct WsReloadState {
    update: broadcast::Sender<()>,
}

pub async fn startup(port: u16, config: CrateConfig, start_browser: bool) -> Result<()> {
    // ctrl-c shutdown checker
    let _crate_config = config.clone();
    let _ = ctrlc::set_handler(move || {
        #[cfg(feature = "plugin")]
        let _ = PluginManager::on_serve_shutdown(&_crate_config);
        std::process::exit(0);
    });

    let ip = get_ip().unwrap_or(String::from("0.0.0.0"));

    match config.hot_reload {
        true => serve_hot_reload(ip, port, config, start_browser).await?,
        false => serve_default(ip, port, config, start_browser).await?,
    }

    Ok(())
}

/// Start the server without hot reload
pub async fn serve_default(
    ip: String,
    port: u16,
    config: CrateConfig,
    start_browser: bool,
) -> Result<()> {
    let first_build_result = crate::builder::build(&config, false)?;

    log::info!("🚀 Starting development server...");

    // WS Reload Watching
    let (reload_tx, _) = broadcast::channel(100);

    // We got to own watcher so that it exists for the duration of serve
    // Otherwise full reload won't work.
    let _watcher = setup_file_watcher(&config, port, ip.clone(), reload_tx.clone()).await?;

    let ws_reload_state = Arc::new(WsReloadState {
        update: reload_tx.clone(),
    });

    // HTTPS
    // Before console info so it can stop if mkcert isn't installed or fails
    let rustls_config = get_rustls(&config).await?;

    // Print serve info
    print_console_info(
        &ip,
        port,
        &config,
        PrettierOptions {
            changed: vec![],
            warnings: first_build_result.warnings,
            elapsed_time: first_build_result.elapsed_time,
        },
    );

    // Router
    let router = setup_router(config, ws_reload_state, None).await?;

    // Start server
    start_server(port, router, start_browser, rustls_config).await?;

    Ok(())
}

/// Start dx serve with hot reload
pub async fn serve_hot_reload(
    ip: String,
    port: u16,
    config: CrateConfig,
    start_browser: bool,
) -> Result<()> {
    let first_build_result = crate::builder::build(&config, false)?;

    log::info!("🚀 Starting development server...");

    // Setup hot reload
    let (reload_tx, _) = broadcast::channel(100);
    let FileMapBuildResult { map, errors } =
        FileMap::<HtmlCtx>::create(config.crate_dir.clone()).unwrap();

    for err in errors {
        log::error!("{}", err);
    }

    let file_map = Arc::new(Mutex::new(map));
    let build_manager = Arc::new(BuildManager {
        config: config.clone(),
        reload_tx: reload_tx.clone(),
    });

    let hot_reload_tx = broadcast::channel(100).0;

    // States
    let hot_reload_state = Arc::new(HotReloadState {
        messages: hot_reload_tx.clone(),
        build_manager: build_manager.clone(),
        file_map: file_map.clone(),
        watcher_config: config.clone(),
    });

    let ws_reload_state = Arc::new(WsReloadState {
        update: reload_tx.clone(),
    });

    // Setup file watcher
    // We got to own watcher so that it exists for the duration of serve
    // Otherwise hot reload won't work.
    let _watcher = setup_file_watcher_hot_reload(
        &config,
        port,
        ip.clone(),
        hot_reload_tx,
        file_map,
        build_manager,
    )
    .await?;

    // HTTPS
    // Before console info so it can stop if mkcert isn't installed or fails
    let rustls_config = get_rustls(&config).await?;

    // Print serve info
    print_console_info(
        &ip,
        port,
        &config,
        PrettierOptions {
            changed: vec![],
            warnings: first_build_result.warnings,
            elapsed_time: first_build_result.elapsed_time,
        },
    );

    // Router
    let router = setup_router(config, ws_reload_state, Some(hot_reload_state)).await?;

    // Start server
    start_server(port, router, start_browser, rustls_config).await?;

    Ok(())
}

const DEFAULT_KEY_PATH: &str = "ssl/key.pem";
const DEFAULT_CERT_PATH: &str = "ssl/cert.pem";

/// Returns an enum of rustls config and a bool if mkcert isn't installed
async fn get_rustls(config: &CrateConfig) -> Result<Option<RustlsConfig>> {
    let web_config = &config.dioxus_config.web.https;
    if web_config.enabled != Some(true) {
        return Ok(None);
    }

    let (cert_path, key_path) = match web_config.mkcert {
        // mkcert, use it
        Some(true) => {
            // Get paths to store certs, otherwise use ssl/item.pem
            let key_path = web_config
                .key_path
                .clone()
                .unwrap_or(DEFAULT_KEY_PATH.to_string());

            let cert_path = web_config
                .cert_path
                .clone()
                .unwrap_or(DEFAULT_CERT_PATH.to_string());

            // Create ssl directory if using defaults
            if key_path == DEFAULT_KEY_PATH && cert_path == DEFAULT_CERT_PATH {
                _ = fs::create_dir("ssl");
            }

            let cmd = Command::new("mkcert")
                .args([
                    "-install",
                    "-key-file",
                    &key_path,
                    "-cert-file",
                    &cert_path,
                    "localhost",
                    "::1",
                    "127.0.0.1",
                ])
                .spawn();

            match cmd {
                Err(e) => {
                    match e.kind() {
                        io::ErrorKind::NotFound => log::error!("mkcert is not installed. See https://github.com/FiloSottile/mkcert#installation for installation instructions."),
                        e => log::error!("an error occured while generating mkcert certificates: {}", e.to_string()),
                    };
                    return Err("failed to generate mkcert certificates".into());
                }
                Ok(mut cmd) => {
                    cmd.wait()?;
                }
            }

            (cert_path, key_path)
        }
        // not mkcert
        Some(false) => {
            // get paths to cert & key
            if let (Some(key), Some(cert)) =
                (web_config.key_path.clone(), web_config.cert_path.clone())
            {
                (cert, key)
            } else {
                // missing cert or key
                return Err("https is enabled but cert or key path is missing".into());
            }
        }
        // other
        _ => return Ok(None),
    };

    Ok(Some(
        RustlsConfig::from_pem_file(cert_path, key_path).await?,
    ))
}

/// Sets up and returns a router
async fn setup_router(
    config: CrateConfig,
    ws_reload: Arc<WsReloadState>,
    hot_reload: Option<Arc<HotReloadState>>,
) -> Result<Router> {
    // Setup cors
    let cors = CorsLayer::new()
        // allow `GET` and `POST` when accessing the resource
        .allow_methods([Method::GET, Method::POST])
        // allow requests from any origin
        .allow_origin(Any)
        .allow_headers(Any);

    let (coep, coop) = if config.cross_origin_policy {
        (
            HeaderValue::from_static("require-corp"),
            HeaderValue::from_static("same-origin"),
        )
    } else {
        (
            HeaderValue::from_static("unsafe-none"),
            HeaderValue::from_static("unsafe-none"),
        )
    };

    // Create file service
    let file_service_config = config.clone();
    let file_service = ServiceBuilder::new()
        .override_response_header(
            HeaderName::from_static("cross-origin-embedder-policy"),
            coep,
        )
        .override_response_header(HeaderName::from_static("cross-origin-opener-policy"), coop)
        .and_then(
            move |response: Response<ServeFileSystemResponseBody>| async move {
                let response = if file_service_config
                    .dioxus_config
                    .web
                    .watcher
                    .index_on_404
                    .unwrap_or(false)
                    && response.status() == StatusCode::NOT_FOUND
                {
                    let body = Full::from(
                        // TODO: Cache/memoize this.
                        std::fs::read_to_string(
                            file_service_config
                                .crate_dir
                                .join(file_service_config.out_dir)
                                .join("index.html"),
                        )
                        .ok()
                        .unwrap(),
                    )
                    .map_err(|err| match err {})
                    .boxed();
                    Response::builder()
                        .status(StatusCode::OK)
                        .body(body)
                        .unwrap()
                } else {
                    response.map(|body| body.boxed())
                };
                Ok(response)
            },
        )
        .service(ServeDir::new(config.crate_dir.join(&config.out_dir)));

    // Setup websocket
    let mut router = Router::new().route("/_dioxus/ws", get(ws_handler));

    // Setup proxy
    for proxy_config in config.dioxus_config.web.proxy.unwrap_or_default() {
        router = proxy::add_proxy(router, &proxy_config)?;
    }

    // Route file service
    router = router.fallback(get_service(file_service).handle_error(
        |error: std::io::Error| async move {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Unhandled internal error: {}", error),
            )
        },
    ));

    // Setup routes
    router = router
        .route("/_dioxus/hot_reload", get(hot_reload_handler))
        .layer(cors)
        .layer(Extension(ws_reload));

    if let Some(hot_reload) = hot_reload {
        router = router.layer(Extension(hot_reload))
    }

    Ok(router)
}

/// Starts dx serve with no hot reload
async fn start_server(
    port: u16,
    router: Router,
    start_browser: bool,
    rustls: Option<RustlsConfig>,
) -> Result<()> {
    // If plugins, call on_serve_start event
    #[cfg(feature = "plugin")]
    PluginManager::on_serve_start(&config)?;

    // Parse address
    let addr = format!("0.0.0.0:{}", port).parse().unwrap();

    // Open the browser
    if start_browser {
        match rustls {
            Some(_) => _ = open::that(format!("https://{}", addr)),
            None => _ = open::that(format!("http://{}", addr)),
        }
    }

    // Start the server with or without rustls
    match rustls {
        Some(rustls) => {
            axum_server::bind_rustls(addr, rustls)
                .serve(router.into_make_service())
                .await?
        }
        None => {
            axum::Server::bind(&addr)
                .serve(router.into_make_service())
                .await?
        }
    }

    Ok(())
}

/// Sets up a file watcher
async fn setup_file_watcher(
    config: &CrateConfig,
    port: u16,
    watcher_ip: String,
    reload_tx: Sender<()>,
) -> Result<RecommendedWatcher> {
    let build_manager = BuildManager {
        config: config.clone(),
        reload_tx,
    };

    let mut last_update_time = chrono::Local::now().timestamp();

    // file watcher: check file change
    let allow_watch_path = config
        .dioxus_config
        .web
        .watcher
        .watch_path
        .clone()
        .unwrap_or_else(|| vec![PathBuf::from("src")]);

    let watcher_config = config.clone();
    let mut watcher = notify::recommended_watcher(move |info: notify::Result<notify::Event>| {
        let config = watcher_config.clone();
        if let Ok(e) = info {
            if chrono::Local::now().timestamp() > last_update_time {
                match build_manager.rebuild() {
                    Ok(res) => {
                        last_update_time = chrono::Local::now().timestamp();

                        #[allow(clippy::redundant_clone)]
                        print_console_info(
                            &watcher_ip,
                            port,
                            &config,
                            PrettierOptions {
                                changed: e.paths.clone(),
                                warnings: res.warnings,
                                elapsed_time: res.elapsed_time,
                            },
                        );

                        #[cfg(feature = "plugin")]
                        let _ = PluginManager::on_serve_rebuild(
                            chrono::Local::now().timestamp(),
                            e.paths,
                        );
                    }
                    Err(e) => log::error!("{}", e),
                }
            }
        }
    })
    .unwrap();

    for sub_path in allow_watch_path {
        watcher
            .watch(
                &config.crate_dir.join(sub_path),
                notify::RecursiveMode::Recursive,
            )
            .unwrap();
    }
    Ok(watcher)
}

// Todo: reduce duplication and merge with setup_file_watcher()
/// Sets up a file watcher with hot reload
async fn setup_file_watcher_hot_reload(
    config: &CrateConfig,
    port: u16,
    watcher_ip: String,
    hot_reload_tx: Sender<Template<'static>>,
    file_map: Arc<Mutex<FileMap<HtmlCtx>>>,
    build_manager: Arc<BuildManager>,
) -> Result<RecommendedWatcher> {
    // file watcher: check file change
    let allow_watch_path = config
        .dioxus_config
        .web
        .watcher
        .watch_path
        .clone()
        .unwrap_or_else(|| vec![PathBuf::from("src")]);

    let watcher_config = config.clone();
    let mut last_update_time = chrono::Local::now().timestamp();

    let mut watcher = RecommendedWatcher::new(
        move |evt: notify::Result<notify::Event>| {
            let config = watcher_config.clone();
            // Give time for the change to take effect before reading the file
            std::thread::sleep(std::time::Duration::from_millis(100));
            if chrono::Local::now().timestamp() > last_update_time {
                if let Ok(evt) = evt {
                    let mut messages: Vec<Template<'static>> = Vec::new();
                    for path in evt.paths.clone() {
                        // if this is not a rust file, rebuild the whole project
                        if path.extension().and_then(|p| p.to_str()) != Some("rs") {
                            match build_manager.rebuild() {
                                Ok(res) => {
                                    print_console_info(
                                        &watcher_ip,
                                        port,
                                        &config,
                                        PrettierOptions {
                                            changed: evt.paths,
                                            warnings: res.warnings,
                                            elapsed_time: res.elapsed_time,
                                        },
                                    );
                                }
                                Err(err) => {
                                    log::error!("{}", err);
                                }
                            }
                            return;
                        }
                        // find changes to the rsx in the file
                        let mut map = file_map.lock().unwrap();

                        match map.update_rsx(&path, &config.crate_dir) {
                            Ok(UpdateResult::UpdatedRsx(msgs)) => {
                                messages.extend(msgs);
                            }
                            Ok(UpdateResult::NeedsRebuild) => {
                                match build_manager.rebuild() {
                                    Ok(res) => {
                                        print_console_info(
                                            &watcher_ip,
                                            port,
                                            &config,
                                            PrettierOptions {
                                                changed: evt.paths,
                                                warnings: res.warnings,
                                                elapsed_time: res.elapsed_time,
                                            },
                                        );
                                    }
                                    Err(err) => {
                                        log::error!("{}", err);
                                    }
                                }
                                return;
                            }
                            Err(err) => {
                                log::error!("{}", err);
                            }
                        }
                    }
                    for msg in messages {
                        let _ = hot_reload_tx.send(msg);
                    }
                }
                last_update_time = chrono::Local::now().timestamp();
            }
        },
        notify::Config::default(),
    )
    .unwrap();

    for sub_path in allow_watch_path {
        if let Err(err) = watcher.watch(
            &config.crate_dir.join(&sub_path),
            notify::RecursiveMode::Recursive,
        ) {
            log::error!("error watching {sub_path:?}: \n{}", err);
        }
    }

    Ok(watcher)
}

/// Get the network ip
fn get_ip() -> Option<String> {
    let socket = match UdpSocket::bind("0.0.0.0:0") {
        Ok(s) => s,
        Err(_) => return None,
    };

    match socket.connect("8.8.8.8:80") {
        Ok(()) => (),
        Err(_) => return None,
    };

    match socket.local_addr() {
        Ok(addr) => Some(addr.ip().to_string()),
        Err(_) => None,
    }
}

/// Handle websockets
async fn ws_handler(
    ws: WebSocketUpgrade,
    _: Option<TypedHeader<headers::UserAgent>>,
    Extension(state): Extension<Arc<WsReloadState>>,
) -> impl IntoResponse {
    ws.on_upgrade(|mut socket| async move {
        let mut rx = state.update.subscribe();
        let reload_watcher = tokio::spawn(async move {
            loop {
                rx.recv().await.unwrap();
                // ignore the error
                if socket
                    .send(Message::Text(String::from("reload")))
                    .await
                    .is_err()
                {
                    break;
                }

                // flush the errors after recompling
                rx = rx.resubscribe();
            }
        });

        reload_watcher.await.unwrap();
    })
}
