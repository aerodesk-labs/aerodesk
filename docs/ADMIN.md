# AeroDesk 运维 dashboard（aerodesk-dashboard）

`aerodesk-dashboard` 是一个独立的只读运维代理 + 单页 dashboard（#369），
复用 SFU/signal 现有内部 API，不新增后端耦合。

```
浏览器 --HTTP--> aerodesk-dashboard(:3080) --(X-Internal-Token)--> SFU(:3002) / session|record|metrics
                                          \--(无鉴权)-----------> signal(:3003) / healthz
```

## 构建

```sh
cargo build --release -p aerodesk-dashboard
# 产物：target/release/aerodesk-dashboard
```

## 运行

| 环境变量 | 默认 | 说明 |
|---|---|---|
| `ADMIN_BIND` | `127.0.0.1:3080` | dashboard 监听地址 |
| `SFU_ADMIN_URL` | `http://127.0.0.1:3002` | SFU 内部 HTTP |
| `SIGNAL_ADMIN_URL` | `http://127.0.0.1:3003` | signal 健康检查 HTTP |
| `INTERNAL_TOKEN` | 空 | SFU 管理接口鉴权（透传给 `/session/*`、`/record/*`、`/metrics`） |
| `ADMIN_TOKEN` | 空 | dashboard 自身鉴权；设置后 `/api/*` 需 `X-Admin-Token` 头 |

```sh
INTERNAL_TOKEN=<sfu-token> ADMIN_TOKEN=<dashboard-token> \
SFU_ADMIN_URL=http://127.0.0.1:3002 SIGNAL_ADMIN_URL=http://127.0.0.1:3003 \
./target/release/aerodesk-dashboard
```

打开 `http://<host>:3080/`，顶部输入 `ADMIN_TOKEN` 后即可看到房间/客户端/录制/负载/TURN/健康。

## systemd（示例）

```ini
[Unit]
Description=AeroDesk admin dashboard
After=network.target

[Service]
Environment=INTERNAL_TOKEN=CHANGE_ME
Environment=ADMIN_TOKEN=CHANGE_ME
Environment=SFU_ADMIN_URL=http://127.0.0.1:3002
Environment=SIGNAL_ADMIN_URL=http://127.0.0.1:3003
Environment=ADMIN_BIND=127.0.0.1:3080
ExecStart=/usr/local/bin/aerodesk-dashboard
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

## 安全

- `ADMIN_BIND` 默认只绑定 loopback；对外暴露务必加 TLS 反向代理（nginx/caddy）。
- `ADMIN_TOKEN` 保护危险写操作（踢人/录制）；`INTERNAL_TOKEN` 是 SFU 侧的管理密钥，
  二者不要复用同一个值。
- dashboard 只做前端聚合，不落盘、不缓存；敏感信息仅存在浏览器 localStorage（`ADMIN_TOKEN`）。

## 相关

- SFU/signal 内部 API 契约：`docs/DEPLOYMENT.md`、运维 skill `references/api.md`
- 问题跟踪：#369
