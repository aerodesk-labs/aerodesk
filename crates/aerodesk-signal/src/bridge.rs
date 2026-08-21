//! #216 M3：跨 PoP 桥接编排（`BRIDGE_CMD` 可选启用）。
//!
//! 当 viewer 加入被钉在其它 PoP 的房间（本应 Redirect）时，若配置了
//! `BRIDGE_CMD`，本 PoP 信令先确保该房间的 aerodesk-bridge 进程就绪
//! （view 主 PoP + publish 本 PoP，见 aerodesk-bridge）；就绪后放行 Join，
//! viewer 在本 PoP 经桥收流（不 Redirect）。桥接失败/超时 → 回退 v1 Redirect。
//!
//! 生命周期与边界：
//! - 按房间单飞（single-flight）：同房间并发 Join 只 spawn 一次；
//! - 桥进程退出（主 PoP 无媒体/被 kill）后自动回收；冷却期内不重复 spawn，
//!   直接 Redirect；冷却后可重试；
//! - `{room}` 占位符替换前先校验房间名（仅 `[A-Za-z0-9._-]`），防止
//!   `sh -c` 命令注入；
//! - 就绪判定：桥 stdout 出现 `publisher leg:`（双腿已连）即视为就绪。

use std::collections::HashMap;
use std::io::BufRead;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 桥就绪标记（aerodesk-bridge 双腿连接成功时打印）。
const READY_MARKER: &str = "publisher leg:";
/// spawn 代数计数器（进程级单调递增，#246：按代数区分桥重建）。
static EPOCH_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 桥接编排结果（Join 处理分支用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeOutcome {
    /// 桥接已就绪（或已有运行中桥），viewer 可在本 PoP 加入。
    Ready,
    /// 桥接失败/冷却中/配置非法 → 调用方回退 v1 Redirect。
    Redirect,
}

/// 运行中的桥子进程（stdout 由读取线程消费并置就绪/退出标志）。
struct RunningBridge {
    child: Child,
    ready: Arc<AtomicBool>,
    exited: Arc<AtomicBool>,
    /// spawn 代数（#246 review）：自然死亡→重建后空闲计时必须重置，防止旧时间戳
    /// 误杀新桥；monitor 按 (room, epoch) 区分。
    epoch: u64,
}

impl RunningBridge {
    fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Relaxed)
    }

    fn is_exited(&self) -> bool {
        self.exited.load(Ordering::Relaxed)
    }

    /// kill 并回收子进程（幂等；已退出则直接 wait 防僵尸）。
    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// 桥接管理器（进程级单例，随 Config 共享）。
pub struct BridgeManager {
    /// 原始命令模板（`{room}` 会被替换；缺失时自动追加 `--room {room}`）。
    cmd: String,
    /// 就绪等待上限（默认 15s）。
    ready_timeout: Duration,
    /// 失败冷却（默认 30s，防止桥持续失败时反复 spawn）。
    fail_cooldown: Duration,
    /// 并发桥上限（默认 8，防房间名轮换绕过冷却的进程滥用，#244 review）。
    max_running: usize,
    running: Mutex<HashMap<String, RunningBridge>>,
    failed: Mutex<HashMap<String, Instant>>,
}

impl BridgeManager {
    pub fn new(
        cmd: String,
        ready_timeout: Duration,
        fail_cooldown: Duration,
        max_running: usize,
    ) -> Self {
        Self {
            cmd,
            ready_timeout,
            fail_cooldown,
            max_running: max_running.max(1),
            running: Mutex::new(HashMap::new()),
            failed: Mutex::new(HashMap::new()),
        }
    }

    /// 该房间当前是否有运行中的桥（含尚未就绪）。用于：桥自身 publisher 腿
    /// Join 免 Redirect、并发 viewer 直接放行（桥就绪后媒体自然到达）。
    pub fn is_running(&self, room: &str) -> bool {
        self.reap_dead();
        self.running
            .lock()
            .unwrap_or_else(aerodesk_core::util::lock_recover)
            .contains_key(room)
    }

    /// 当前运行中（含等待就绪）的房间列表 + spawn 代数（生命周期 monitor 用，#246）。
    pub fn running_rooms(&self) -> Vec<(String, u64)> {
        self.reap_dead();
        self.running
            .lock()
            .unwrap_or_else(aerodesk_core::util::lock_recover)
            .iter()
            .map(|(r, rb)| (r.clone(), rb.epoch))
            .collect()
    }

    /// 停止指定房间的桥（kill + 回收 + 移除）。不存在/已退出返回 false。
    /// 不写失败冷却——运维/空闲回收触发，不应阻塞后续正常重建。
    pub fn stop(&self, room: &str) -> bool {
        let mut running = self
            .running
            .lock()
            .unwrap_or_else(aerodesk_core::util::lock_recover);
        if let Some(mut rb) = running.remove(room) {
            rb.kill();
            info!("bridge: stopped for room {room}");
            true
        } else {
            false
        }
    }

    /// 确保房间桥就绪：spawn（如缺）→ 轮询就绪/退出至超时。
    /// `Ready` → viewer 本 PoP 加入；`Redirect` → 调用方回退 v1。
    pub fn ensure_ready(&self, room: &str) -> BridgeOutcome {
        if !sanitize_room(room) {
            warn!("bridge: room {room:?} contains unsafe chars, fallback redirect");
            return BridgeOutcome::Redirect;
        }
        self.reap_dead();

        // 失败冷却期内不重试。
        {
            let mut failed = self
                .failed
                .lock()
                .unwrap_or_else(aerodesk_core::util::lock_recover);
            if let Some(t) = failed.get(room) {
                if t.elapsed() < self.fail_cooldown {
                    warn!(
                        "bridge: room {room} in fail cooldown ({:?} left), fallback redirect",
                        self.fail_cooldown.saturating_sub(t.elapsed())
                    );
                    return BridgeOutcome::Redirect;
                }
                failed.remove(room);
            }
        }

        // 缺桥 → spawn：在同一 running 锁内完成「上限检查 + 启动 + 登记」，
        // 消除 max_running 竞态（并发 Join 不同房间此前可同时通过上限检查）；
        // 同房间并发 Join 仍单飞（contains_key 检查 + insert 在同一锁）。
        {
            let mut running = self
                .running
                .lock()
                .unwrap_or_else(aerodesk_core::util::lock_recover);
            if !running.contains_key(room) {
                // 全局并发上限：仅对「新 spawn」生效，已运行房间不受限。
                if running.len() >= self.max_running {
                    warn!(
                        "bridge: running bridges {} >= max {}; fallback redirect",
                        running.len(),
                        self.max_running
                    );
                    return BridgeOutcome::Redirect;
                }
                match self.start(room) {
                    Ok(rb) => {
                        running.insert(room.to_string(), rb);
                    }
                    Err(e) => {
                        drop(running);
                        warn!("bridge: spawn room {room} failed: {e}; fallback redirect");
                        self.mark_failed(room);
                        return BridgeOutcome::Redirect;
                    }
                }
            }
        }

        // 轮询就绪，最多 ready_timeout。
        let deadline = Instant::now() + self.ready_timeout;
        loop {
            {
                let mut running = self
                    .running
                    .lock()
                    .unwrap_or_else(aerodesk_core::util::lock_recover);
                match running.get_mut(room) {
                    Some(rb) if rb.is_ready() => {
                        info!("bridge: room {room} ready");
                        return BridgeOutcome::Ready;
                    }
                    Some(rb) if rb.is_exited() || Instant::now() >= deadline => {
                        warn!(
                            "bridge: room {room} not ready (exited={}, timeout={:?}); fallback redirect",
                            rb.is_exited(),
                            self.ready_timeout
                        );
                        if let Some(mut rb) = running.remove(room) {
                            rb.kill();
                        }
                        drop(running);
                        self.mark_failed(room);
                        return BridgeOutcome::Redirect;
                    }
                    Some(_) => {}
                    None => {
                        // 条目被并发移除（如 monitor 空闲回收/#246）→ 回退 Redirect，
                        // 不写失败冷却（stop/回收不应阻塞后续重建）。
                        drop(running);
                        warn!(
                            "bridge: room {room} bridge removed during readiness wait; fallback redirect"
                        );
                        return BridgeOutcome::Redirect;
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// 回收已退出（stdout EOF）的桥。
    fn reap_dead(&self) {
        let mut running = self
            .running
            .lock()
            .unwrap_or_else(aerodesk_core::util::lock_recover);
        let dead: Vec<String> = running
            .iter()
            .filter(|(_, rb)| rb.is_exited())
            .map(|(r, _)| r.clone())
            .collect();
        for r in dead {
            if let Some(mut rb) = running.remove(&r) {
                rb.kill();
            }
        }
    }

    /// 启动桥进程 + 就绪读取线程（不持 running 锁；由调用方在锁内登记）。
    fn start(&self, room: &str) -> Result<RunningBridge, String> {
        let cmd = render_command(&self.cmd, room);
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| format!("{e}"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "bridge stdout unavailable".to_string())?;
        let ready = Arc::new(AtomicBool::new(false));
        let exited = Arc::new(AtomicBool::new(false));
        let ready_reader = ready.clone();
        let exited_reader = exited.clone();
        // 读取线程：扫描就绪标记；EOF 视为桥退出。
        std::thread::Builder::new()
            .name(format!("bridge-read-{room}"))
            .spawn(move || {
                let reader = std::io::BufReader::new(stdout);
                for line in reader.lines() {
                    let Ok(line) = line else { break };
                    if line.contains(READY_MARKER) {
                        ready_reader.store(true, Ordering::Relaxed);
                    }
                }
                exited_reader.store(true, Ordering::Relaxed);
            })
            .map_err(|e| format!("spawn reader: {e}"))?;
        info!("bridge: spawned for room {room}: {cmd}");
        Ok(RunningBridge {
            child,
            ready,
            exited,
            epoch: EPOCH_COUNTER.fetch_add(1, Ordering::Relaxed),
        })
    }

    fn mark_failed(&self, room: &str) {
        const MAX_FAILED: usize = 1024;
        let mut failed = self
            .failed
            .lock()
            .unwrap_or_else(aerodesk_core::util::lock_recover);
        failed.insert(room.to_string(), Instant::now());
        // 有界：超过上限先清掉已过冷却期的，再按时间淘汰最旧，防失败房间永久累积。
        if failed.len() > MAX_FAILED {
            failed.retain(|_, t| t.elapsed() < self.fail_cooldown);
            while failed.len() > MAX_FAILED {
                if let Some(oldest) = failed
                    .iter()
                    .min_by_key(|(_, t)| **t)
                    .map(|(r, _)| r.clone())
                {
                    failed.remove(&oldest);
                } else {
                    break;
                }
            }
        }
    }
}

impl Drop for BridgeManager {
    fn drop(&mut self) {
        let mut running = self
            .running
            .lock()
            .unwrap_or_else(aerodesk_core::util::lock_recover);
        for (_, mut rb) in running.drain() {
            rb.kill();
        }
    }
}

/// 房间名安全校验：仅 `[A-Za-z0-9._-]`（防 `sh -c` 命令注入）。
pub fn sanitize_room(room: &str) -> bool {
    !room.is_empty()
        && !room.starts_with('-') // 防模板缺 --room 时被当作命令行选项（#244 review）
        && room
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// 空闲回收纯函数（#246 review）：给定当前运行房间（含代数）、真实客户端数、
/// 空闲计时器与上次代数，返回应停桥的房间列表。
///
/// 副作用（纯逻辑可测）：
/// - 修剪不在运行中的 `idle_since`/`last_epoch` 条目；
/// - 房间代数变化（自然死亡后重建）→ 重置空闲计时，防止旧时间戳误杀新桥；
/// - 有真实客户端 → 清除空闲计时；
/// - 空闲超时 → 加入待停列表并清除计时。
pub fn idle_rooms_to_stop(
    running: &[(String, u64)],
    real_peers: &HashMap<String, usize>,
    idle_since: &mut HashMap<String, Instant>,
    last_epoch: &mut HashMap<String, u64>,
    idle: Duration,
    now: Instant,
) -> Vec<String> {
    let running_set: std::collections::HashSet<&str> =
        running.iter().map(|(r, _)| r.as_str()).collect();
    idle_since.retain(|r, _| running_set.contains(r.as_str()));
    last_epoch.retain(|r, _| running_set.contains(r.as_str()));
    let mut to_stop = Vec::new();
    for (room, epoch) in running {
        if last_epoch.get(room) != Some(epoch) {
            // 新代桥（重启/重建）：本轮重置计时，不立即判定空闲。
            idle_since.remove(room);
            last_epoch.insert(room.clone(), *epoch);
            continue;
        }
        let real = real_peers.get(room).copied().unwrap_or(0);
        if real > 0 {
            idle_since.remove(room);
            continue;
        }
        let since = idle_since.entry(room.clone()).or_insert(now);
        if now.duration_since(*since) >= idle {
            to_stop.push(room.clone());
            idle_since.remove(room);
        }
    }
    to_stop
}

/// 桥死亡检测纯函数（#249）：上轮存活集合与本轮存活集合的差集，排除 monitor
/// 本轮自己停掉的房间，即「自然死亡」的房间——应触发已连接 viewer 的恢复（kick）。
pub fn died_rooms(
    alive_prev: &std::collections::HashSet<String>,
    alive_now: &std::collections::HashSet<String>,
    stopped_by_monitor: &std::collections::HashSet<String>,
) -> Vec<String> {
    let mut died: Vec<String> = alive_prev.difference(alive_now).cloned().collect();
    died.retain(|r| !stopped_by_monitor.contains(r));
    died.sort();
    died
}

/// 渲染实际命令：替换 `{room}`；无占位符时追加 ` --room {room}`。
pub fn render_command(cmd: &str, room: &str) -> String {
    if cmd.contains("{room}") {
        cmd.replace("{room}", room)
    } else {
        format!("{cmd} --room {room}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mgr(cmd: &str, ready_ms: u64, cooldown_s: u64) -> BridgeManager {
        BridgeManager::new(
            cmd.to_string(),
            Duration::from_millis(ready_ms),
            Duration::from_secs(cooldown_s),
            8,
        )
    }

    #[test]
    fn sanitize_room_rejects_injection() {
        assert!(sanitize_room("demo"));
        assert!(sanitize_room("room-1_a.b"));
        assert!(!sanitize_room(""));
        assert!(!sanitize_room("x; touch /tmp/pwned"));
        assert!(!sanitize_room("$(id)"));
        assert!(!sanitize_room("a b"));
        assert!(!sanitize_room("a|b"));
        // 前导 '-'：模板缺 --room 时会被当作命令行选项（#244 review）。
        assert!(!sanitize_room("-h"));
        assert!(!sanitize_room("--help"));
    }

    #[test]
    fn render_command_substitutes_or_appends_room() {
        assert_eq!(
            render_command("bridge --room {room} --codec h264", "demo"),
            "bridge --room demo --codec h264"
        );
        assert_eq!(
            render_command("bridge --remote-signal ws://x", "demo"),
            "bridge --remote-signal ws://x --room demo"
        );
    }

    #[cfg(unix)]
    #[test]
    fn ready_when_marker_printed() {
        let m = mgr("sh -c 'echo \"publisher leg: up\"; sleep 30'", 5000, 60);
        assert_eq!(m.ensure_ready("demo"), BridgeOutcome::Ready);
        assert!(m.is_running("demo"), "就绪后桥应仍在运行");
    }

    #[cfg(unix)]
    #[test]
    fn failure_redirects_and_cooldown_blocks_respawn() {
        let m = mgr("sh -c 'exit 1'", 800, 60);
        let t0 = Instant::now();
        assert_eq!(m.ensure_ready("demo"), BridgeOutcome::Redirect);
        assert!(t0.elapsed() < Duration::from_secs(5), "失败应快速返回");
        assert!(!m.is_running("demo"));
        // 冷却期内再次调用必须直接 Redirect（不重试 spawn）。
        let t1 = Instant::now();
        assert_eq!(m.ensure_ready("demo"), BridgeOutcome::Redirect);
        assert!(t1.elapsed() < Duration::from_secs(2), "冷却期不应等待");
    }

    #[test]
    fn injection_room_falls_back_without_spawn() {
        let m = mgr("touch /tmp/bridge-injected", 500, 60);
        assert_eq!(
            m.ensure_ready("x; touch /tmp/pwned"),
            BridgeOutcome::Redirect
        );
        assert!(!m.is_running("x; touch /tmp/pwned"));
    }

    #[cfg(unix)]
    #[test]
    fn timeout_without_marker_redirects() {
        let m = mgr("sh -c 'sleep 30'", 300, 60);
        assert_eq!(m.ensure_ready("demo"), BridgeOutcome::Redirect);
        assert!(!m.is_running("demo"), "超时失败后桥应被清理");
    }

    #[cfg(unix)]
    #[test]
    fn concurrent_ensure_ready_spawns_once() {
        // 两个线程同时 ensure_ready：只 spawn 一个进程（用 temp_dir+pid 计数文件）。
        let count_file = std::env::temp_dir().join(format!("bridge-spawn-{}", std::process::id()));
        let _ = std::fs::remove_file(&count_file);
        let cmd = format!(
            "sh -c 'echo \"publisher leg: up\"; echo x >> {}; sleep 30'",
            count_file.display()
        );
        let m = Arc::new(BridgeManager::new(
            cmd,
            Duration::from_secs(5),
            Duration::from_secs(60),
            8,
        ));
        let m1 = m.clone();
        let m2 = m.clone();
        let t1 = std::thread::spawn(move || m1.ensure_ready("demo"));
        let t2 = std::thread::spawn(move || m2.ensure_ready("demo"));
        let r1 = t1.join().unwrap();
        let r2 = t2.join().unwrap();
        assert_eq!(r1, BridgeOutcome::Ready);
        assert_eq!(r2, BridgeOutcome::Ready);
        let count = std::fs::read_to_string(&count_file)
            .map(|s| s.lines().count())
            .unwrap_or(0);
        assert_eq!(count, 1, "同房间并发只应 spawn 一次");
        let _ = std::fs::remove_file(&count_file);
    }

    #[cfg(unix)]
    #[test]
    fn running_rooms_and_stop() {
        let m = BridgeManager::new(
            "sh -c 'echo \"publisher leg: up\"; sleep 30'".to_string(),
            Duration::from_secs(5),
            Duration::from_secs(60),
            8,
        );
        assert_eq!(m.ensure_ready("room-a"), BridgeOutcome::Ready);
        assert!(m.running_rooms().iter().any(|(r, _)| r == "room-a"));
        assert!(m.stop("room-a"), "运行中的桥 stop 应成功");
        assert!(!m.stop("room-a"), "重复 stop 返回 false");
        assert!(!m.running_rooms().iter().any(|(r, _)| r == "room-a"));
        // stop 后再次 ensure_ready 应能重建（stop 不写失败冷却）。
        assert_eq!(m.ensure_ready("room-a"), BridgeOutcome::Ready);
    }

    #[test]
    fn idle_rooms_pure_policy() {
        use std::collections::HashMap;
        let idle = Duration::from_secs(300);
        let t0 = Instant::now();
        let mut idle_since = HashMap::new();
        let mut last_epoch = HashMap::new();

        // 首次出现（新代数）：只登记代数不立即停（fresh-spawn 保护），计时从下一 tick 起。
        let running = vec![("r".to_string(), 1u64)];
        assert!(
            idle_rooms_to_stop(
                &running,
                &HashMap::new(),
                &mut idle_since,
                &mut last_epoch,
                idle,
                t0
            )
            .is_empty()
        );
        assert_eq!(last_epoch.get("r"), Some(&1));

        // 第二个 tick：开始空闲计时；未超时不停。
        assert!(
            idle_rooms_to_stop(
                &running,
                &HashMap::new(),
                &mut idle_since,
                &mut last_epoch,
                idle,
                t0 + Duration::from_secs(100)
            )
            .is_empty()
        );

        // 计时起点后 301s：停，且清计时。
        let to_stop = idle_rooms_to_stop(
            &running,
            &HashMap::new(),
            &mut idle_since,
            &mut last_epoch,
            idle,
            t0 + Duration::from_secs(100 + 301),
        );
        assert_eq!(to_stop, vec!["r".to_string()]);
        assert!(idle_since.is_empty());

        // 有真实客户端：不清算空闲。
        let mut real = HashMap::new();
        real.insert("r".to_string(), 1usize);
        assert!(
            idle_rooms_to_stop(&running, &real, &mut idle_since, &mut last_epoch, idle, t0)
                .is_empty()
        );
        assert!(idle_since.is_empty());

        // 不在运行中的房间：计时器被修剪（自然死亡后重建不会带旧时间戳）。
        idle_since.insert("gone".to_string(), t0 - Duration::from_secs(1000));
        last_epoch.insert("gone".to_string(), 1);
        assert!(
            idle_rooms_to_stop(
                &[],
                &HashMap::new(),
                &mut idle_since,
                &mut last_epoch,
                idle,
                t0
            )
            .is_empty()
        );
        assert!(idle_since.is_empty() && last_epoch.is_empty());

        // 代数变化（重建）：重置旧计时，新桥不会被旧时间戳误杀。
        idle_since.insert("r".to_string(), t0 - Duration::from_secs(1000));
        last_epoch.insert("r".to_string(), 1);
        let rebuilt = vec![("r".to_string(), 2u64)];
        assert!(
            idle_rooms_to_stop(
                &rebuilt,
                &HashMap::new(),
                &mut idle_since,
                &mut last_epoch,
                idle,
                t0
            )
            .is_empty(),
            "新代桥必须重置空闲计时"
        );
        assert!(idle_since.is_empty());
        assert_eq!(last_epoch.get("r"), Some(&2));
    }

    #[test]
    fn died_rooms_pure_policy() {
        use std::collections::HashSet;
        let hs = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<HashSet<_>>();
        let stopped = HashSet::new();

        // 正常死亡：上轮有、本轮无 → 检出。
        let prev = hs(&["a", "b"]);
        let now = hs(&["a"]);
        assert_eq!(died_rooms(&prev, &now, &stopped), vec!["b".to_string()]);

        // monitor 自己停的：不视为死亡（避免误踢）。
        let mut stopped2 = HashSet::new();
        stopped2.insert("b".to_string());
        assert!(died_rooms(&prev, &now, &stopped2).is_empty());

        // 无变化 / 重建（两轮都有）：不检出。
        assert!(died_rooms(&prev, &prev, &stopped).is_empty());
        assert!(died_rooms(&now, &now, &stopped).is_empty());

        // 多房间死亡：排序稳定。
        let prev3 = hs(&["c", "a", "b"]);
        let now3 = hs(&["a"]);
        assert_eq!(
            died_rooms(&prev3, &now3, &stopped),
            vec!["b".to_string(), "c".to_string()]
        );
    }

    #[cfg(unix)]
    #[test]
    fn max_running_cap_redirects_new_rooms() {
        // 上限 1：第一个房间就绪后，第二个房间必须 Redirect（不 spawn）。
        let m = BridgeManager::new(
            "sh -c 'echo \"publisher leg: up\"; sleep 30'".to_string(),
            Duration::from_secs(5),
            Duration::from_secs(60),
            1,
        );
        assert_eq!(m.ensure_ready("room-a"), BridgeOutcome::Ready);
        assert_eq!(m.ensure_ready("room-b"), BridgeOutcome::Redirect);
        assert!(!m.is_running("room-b"), "超限房间不应有桥");
    }
}
