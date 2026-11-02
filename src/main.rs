mod detect;
mod registry;
mod web;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;

const HELP: &str = "\
proxy-distributor — 本机项目检测面板

用法:
  proxy-distributor [选项]

选项:
  --bind <ADDR:PORT>   监听地址，可重复 (默认 127.0.0.1:18791)
                       例: --bind 127.0.0.1:18791 --bind 100.64.0.1:18791
  --scan-root <PATH>   追加扫描根（可重复，默认 /opt /usr/local/bin /usr/local/lib /srv $HOME $HOME/.config /home/*）
  --state <PATH>       项目清单落盘位置（默认 $PROXY_DISTRIBUTOR_STATE，否则 /var/lib/proxy-distributor/projects.json）
  --json               不启服务，只打印一次 JSON 探测结果
  -h, --help           显示本帮助
";

struct Args {
    binds: Vec<SocketAddr>,
    json: bool,
    scan_roots: Vec<PathBuf>,
    state: Option<PathBuf>,
}

fn parse_args() -> Result<Option<Args>, String> {
    let mut binds: Vec<SocketAddr> = Vec::new();
    let mut json = false;
    let mut scan_roots = Vec::new();
    let mut state = None;

    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{HELP}");
                return Ok(None);
            }
            "--json" => json = true,
            "--bind" => {
                let v = it.next().ok_or("--bind 需要一个地址")?;
                binds.push(v.parse().map_err(|_| format!("无法解析地址: {v}"))?);
            }
            "--scan-root" => {
                let v = it.next().ok_or("--scan-root 需要一个路径")?;
                scan_roots.push(PathBuf::from(v));
            }
            "--state" => {
                let v = it.next().ok_or("--state 需要一个路径")?;
                state = Some(PathBuf::from(v));
            }
            other => {
                if let Some(v) = other.strip_prefix("--bind=") {
                    binds.push(v.parse().map_err(|_| format!("无法解析地址: {v}"))?);
                } else if let Some(v) = other.strip_prefix("--scan-root=") {
                    scan_roots.push(PathBuf::from(v));
                } else if let Some(v) = other.strip_prefix("--state=") {
                    state = Some(PathBuf::from(v));
                } else {
                    return Err(format!("未知参数: {other}（--help 看用法）"));
                }
            }
        }
    }
    if binds.is_empty() {
        binds.push("127.0.0.1:18791".parse().expect("static default addr"));
    }
    Ok(Some(Args {
        binds,
        json,
        scan_roots,
        state,
    }))
}

#[tokio::main]
async fn main() {
    let args = match parse_args() {
        Ok(Some(args)) => args,
        Ok(None) => return,
        Err(err) => {
            eprintln!("{err}");
            std::process::exit(2);
        }
    };

    let mut roots = detect::default_roots();
    roots.extend(args.scan_roots.iter().cloned());

    let store = match args.state {
        Some(path) => registry::Store::new(path),
        None => registry::Store::from_env(),
    };
    let projects = store.load();

    if args.json {
        match serde_json::to_string_pretty(&detect::detect(&projects, &roots)) {
            Ok(body) => println!("{body}"),
            Err(err) => {
                eprintln!("序列化失败: {err}");
                std::process::exit(1);
            }
        }
        return;
    }

    println!("projects state: {}", store.path().display());
    let state = Arc::new(web::AppState::new(
        projects,
        roots,
        store,
        Duration::from_millis(1500),
    ));
    let app = web::router(state);

    // One `Router` per listener: `axum::serve` takes ownership of it.
    let mut listeners = Vec::new();
    for addr in &args.binds {
        match TcpListener::bind(addr).await {
            Ok(listener) => listeners.push((*addr, listener)),
            Err(err) => {
                eprintln!("无法监听 {addr}: {err}");
                std::process::exit(1);
            }
        }
    }
    for (addr, _) in &listeners {
        println!("proxy-distributor listening on http://{addr}");
    }

    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        shutdown_signal().await;
        let _ = stop_tx.send(true);
    });

    let mut tasks = tokio::task::JoinSet::new();
    for (addr, listener) in listeners {
        let app = app.clone();
        let mut stop = stop_rx.clone();
        tasks.spawn(async move {
            let result = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = stop.changed().await;
                })
                .await;
            (addr, result)
        });
    }

    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok((addr, Err(err))) => {
                eprintln!("{addr} 服务退出: {err}");
                std::process::exit(1);
            }
            Ok((_, Ok(()))) => {}
            Err(err) => {
                eprintln!("服务任务崩了: {err}");
                std::process::exit(1);
            }
        }
    }
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    println!("\nbye");
}
