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
  --bind <ADDR:PORT>   监听地址 (默认 127.0.0.1:18791)
  --scan-root <PATH>   追加扫描根（可重复，默认 /opt /usr/local/bin /usr/local/lib /srv $HOME $HOME/.config /home/*）
  --json               不启服务，只打印一次 JSON 探测结果
  -h, --help           显示本帮助
";

struct Args {
    bind: SocketAddr,
    json: bool,
    scan_roots: Vec<PathBuf>,
}

fn parse_args() -> Result<Option<Args>, String> {
    let mut bind: SocketAddr = "127.0.0.1:18791".parse().expect("static default addr");
    let mut json = false;
    let mut scan_roots = Vec::new();

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
                bind = v.parse().map_err(|_| format!("无法解析地址: {v}"))?;
            }
            "--scan-root" => {
                let v = it.next().ok_or("--scan-root 需要一个路径")?;
                scan_roots.push(PathBuf::from(v));
            }
            other => {
                if let Some(v) = other.strip_prefix("--bind=") {
                    bind = v.parse().map_err(|_| format!("无法解析地址: {v}"))?;
                } else if let Some(v) = other.strip_prefix("--scan-root=") {
                    scan_roots.push(PathBuf::from(v));
                } else {
                    return Err(format!("未知参数: {other}（--help 看用法）"));
                }
            }
        }
    }
    Ok(Some(Args {
        bind,
        json,
        scan_roots,
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
    let projects = registry::all();

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

    let state = Arc::new(web::AppState::new(
        projects,
        roots,
        Duration::from_millis(1500),
    ));
    let app = web::router(state);

    let listener = match TcpListener::bind(args.bind).await {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!("无法监听 {}: {err}", args.bind);
            std::process::exit(1);
        }
    };
    println!("proxy-distributor listening on http://{}", args.bind);

    if let Err(err) = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
    {
        eprintln!("服务退出: {err}");
        std::process::exit(1);
    }
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    println!("\nbye");
}
