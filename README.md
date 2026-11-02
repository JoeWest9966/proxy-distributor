# proxy-distributor

本机项目检测面板：给定一个 GitHub 项目，回答三个问题 —— **本机装了吗、装在哪、开在哪个端口**。

当前跟踪的第一个项目是 [`LAGcomcom/zen-gate`](https://github.com/LAGcomcom/zen-gate)。加新项目只改 `src/registry.rs` 里的一行结构体，检测逻辑是通用的。

```
cargo run                                              # 默认 http://127.0.0.1:18791
cargo run -- --bind 127.0.0.1:18791 --bind 100.64.0.1:18791   # 多个监听，可重复
cargo run -- --json                                    # 不开服务，打印一次检测结果
cargo run -- --scan-root /data                         # 追加扫描根
```

`--bind` 可以给多次。这台机器上跑的是：

```
--bind 127.0.0.1:18791 --bind 100.106.43.81:18791
```

**别绑 `0.0.0.0`**：eth0 是公网地址，面板没有鉴权。

左列是项目清单（顶部「+ 新增」），右边是选中项目的详情卡片。面板每 5 秒拉一次 `/api/detect`（服务端 1.5s 缓存），也可以手动刷新。

## 加一个项目

点左列「+ 新增」，贴 `https://github.com/owner/name`，回车。自动推出：

| 字段 | 从什么推出来 |
| --- | --- |
| `slug` | 仓库名小写，`Zen-Gate` → `zen-gate` |
| `aliases` | `[slug]` + 你在「额外别名」里填的（逗号分隔） |
| `config_dirs` | `~/.config/<slug>` |

链接的容错：`.git` 后缀、尾斜杠、`tree/main` 之后的部分、`git@github.com:owner/name.git`、`github.com/owner/name`、裸 `owner/name` 都认；非 github.com 的域名、缺仓库名、非法字符直接 400。

添加**不校验仓库是否存在**（匿名 GitHub API 60 次/小时，这个 IP 上不止我们在用）。输错了就是一张一直 `absent` 的卡片，一眼能看出来，删掉重加。

**检测靠名字匹配，不是 git 身份。** 仓库叫 `my-proxy`、本机装成 `/opt/mp` 的话扫不到 —— 把 `mp` 填进别名。参考写在弹窗里，不指望人记得。

清单存在 `/var/lib/proxy-distributor/projects.json`（`--state <path>` 或 `$PROXY_DISTRIBUTOR_STATE` 可改；systemd 用 `StateDirectory=proxy-distributor` 建目录）。文件不存在 = 首次启动，落回种子数据（现在就是 zen-gate）。写盘是 write-then-rename，半截文件不会覆盖好的。

## 判定为「装了吗」的三档

| 状态 | 含义 |
| --- | --- |
| `running` | 有进程活着（按 comm / exe 文件名 / 命令行参数 / cwd 匹配） |
| `installed` | 没进程，但扫描根里找到安装物（二进制、AppImage、git 检出、配置目录） |
| `absent` | 三者都没有 |

进程还分两档：`direct` 是本体，`related` 只是命令行或 cwd 里引用了它（比如 zen-gate 拉起来的 `sing-box`、配套的 `panel-proxy`）。相关进程的端口折叠在「相关进程」里，不跟本体端口混在一起。

## 页面与底部 dock

页面是单页路由，底部浮动 dock 是导航（视觉照 Exitmap 的 floating-dock / workbuddy-manager，MIT：圆形按钮、悬停放大、hover tooltip、当前页底部小圆点）。现在只有一项「项目检测」，路径 `/detect`。

加一个菜单项 = 三步，其中第二步之后服务端一行都不用改：

1. `assets/index.html` 里的 `PAGES` 数组加 `{ path: "/xxx", title: "…", icon: `<svg…>` }`；
2. `<main>` 里加 `<section class="page" id="page-xxx">…</section>`；
3. 打完收工。非 `/api/*` 的路径都落到 SPA 兜底，直接刷新 `/detect` 也是 200，不用在 Rust 里加路由。

注意 `/` 会重定向到第一个菜单项（现在是 `/detect`）。

## 数据从哪来

全部只读，不需要 root，没有外部依赖：

- **进程**：`/proc/<pid>/{comm,exe,cwd,cmdline,stat}`，跳过内核线程。命令行只在参数**长得像路径**时才算数（`/`、`./`、`../`、`~` 开头，或 `--flag=/path`）—— 否则 `grep zen-gate`、`curl -d '{"url":"owner/zen-gate"}'` 这种「只是提到了它」的进程会被当成它的进程。
- **端口**：`/proc/net/tcp{,6}` 里 `0A`（LISTEN）的 socket inode，再只用已经匹配上的 pid 去 `/proc/<pid>/fd` 反查属主 —— 全量扫 fd 在这台机器上要 1 秒，限定 pid 后是毫秒。
- **命令行里声明的端口**：`-port 18787` / `--port=18787` / `-listen host:port`，跟实际 socket 合并成 `socket+cmdline`，只有声明没 socket 的会单独标出来。
- **路径**：`/opt`、`/usr/local/bin`、`/usr/local/lib`、`/srv`、`$HOME`、`$HOME/.config`、`/home/*`，深度 3，一次遍历服务所有项目。
- **服务**：`/etc|/usr/lib|/lib|/run/systemd/system` 里的 `.service`，命中名字或内容再 `systemctl is-active`。

## 部署

`/etc/systemd/system/proxy-distributor.service` → `systemctl enable --now proxy-distributor`。二进制在 `target/release/proxy-distributor`，改完代码 `cargo build --release && systemctl restart proxy-distributor`。

注意：systemd 单元里没有 `HOME`。所以主目录不是从 `$HOME` 猜的，而是依次取 `$HOME` → `/etc/passwd` 里本进程 uid 对应的 home → `/root` → `/home/*`。否则 `~/.config/<slug>` 会在服务模式下悄悄消失。

## API

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| GET | `/api/detect[?refresh=1]` | 全部项目的探测结果 |
| GET | `/api/projects` | 项目清单（不含探测） |
| POST | `/api/projects` | `{"url": "...", "aliases": "a, b"}` → 201 / 400 / 409 重名 |
| DELETE | `/api/projects/{slug}` | 204 / 404 |

写接口没有鉴权 —— 能进 tailnet 的人就能改清单。现在是只读转可写，说一句作数。

`GET /api/detect[?refresh=1]` → JSON：

```json
{
  "generated_ms": 1793596676169,
  "hostname": "9996696.xyz",
  "took_ms": 118,
  "timeline": { "procs_ms": 15, "sockets_ms": 3, "paths_ms": 78, "services_ms": 22 },
  "scan_roots": ["/opt", "..."],
  "projects": [{
    "slug": "zen-gate",
    "repo_url": "https://github.com/LAGcomcom/zen-gate",
    "status": "running",
    "ports": [{ "port": 18787, "proto": "tcp", "bind": "127.0.0.1",
                "sources": ["socket", "cmdline"], "pid": 3525648,
                "process": "zen-gate", "relation": "direct",
                "url": "http://127.0.0.1:18787/" }],
    "related_ports": [],
    "processes": [], "installations": [], "services": [], "notes": []
  }]
}
```

`GET /healthz` → `ok`。

别名里长度 ≥ 4 的才参与子串匹配，短别名（`zg`）只做整名或前缀匹配 —— 否则 `zg` 会把 `zgrep` 全抓进来。

## 已知边界

- 只在 Linux 上跑（读 `/proc`）。
- 不看 docker 容器和 k8s：容器里跑的项目，本机侧只能看到 `docker-proxy` 的端口。
- 端口是「谁在监听」，不是「对外怎么用」：反向代理、tailnet 转发这些不在这一层。

## 开发

```bash
export CARGO_TARGET_DIR=$PWD/target   # 这台机器的 CARGO_TARGET_DIR 被别的项目指走了
cargo test
cargo clippy --all-targets -- -D warnings
```

## License

未选定。
