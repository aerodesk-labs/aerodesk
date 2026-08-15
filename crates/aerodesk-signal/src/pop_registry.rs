//! 动态 room→PoP 注册表（多 PoP v2，#154）。
//!
//! 房间归属由首个加入者所在 PoP 登记（`register`），TTL 过期后视为未登记；
//! 可选 JSON 文件持久化（`POP_REGISTRY_FILE`）——多 PoP 共享同一文件即可互见。
//! 落盘前清扫过期条目（内存与文件都不随死房间无限增长），并以 tmp+rename
//! 原子写（读方不会读到半截 JSON）。
//! 限制：文件后端为 last-writer-wins，并发登记可能互相覆盖（v2 低变更场景可接受；
//! 生产多写并发请换 Redis 后端，见 ADR-0004）。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    pop: String,
    updated_at: u64,
}

/// 进程内互斥 + 可选共享文件持久化的注册表。
pub struct PopRegistry {
    inner: Mutex<HashMap<String, Entry>>,
    path: Option<PathBuf>,
    ttl_secs: u64,
}

impl PopRegistry {
    pub fn new(path: Option<PathBuf>, ttl_secs: u64) -> Self {
        let reg = Self {
            inner: Mutex::new(HashMap::new()),
            path,
            ttl_secs: ttl_secs.max(1),
        };
        if reg.path.is_some() {
            let _ = reg.load();
        }
        reg
    }

    fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    /// 查询房间归属 PoP；过期条目视为不存在并清理。
    /// 未命中且配置了共享文件时先刷新文件（其它 PoP 的登记），再查一次。
    pub fn lookup(&self, room: &str) -> Option<String> {
        let hit = {
            let m = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let now = Self::now();
            match m.get(room) {
                Some(e) if now.saturating_sub(e.updated_at) <= self.ttl_secs => Some(e.pop.clone()),
                Some(_) => None, // 过期：下面统一清理
                None => None,
            }
        };
        if hit.is_some() {
            return hit;
        }
        self.merge_file();
        let mut m = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let now = Self::now();
        match m.get(room) {
            Some(e) if now.saturating_sub(e.updated_at) <= self.ttl_secs => Some(e.pop.clone()),
            Some(_) => {
                m.remove(room);
                None
            }
            None => None,
        }
    }

    /// 登记房间归属（幂等刷新 TTL）并持久化；先 merge 共享文件避免覆盖其它 PoP 条目。
    /// 为降低共享文件的每次 join 全量重写：仅当归属变化或 TTL 过半时才落盘。
    pub fn register(&self, room: &str, pop: &str) {
        self.merge_file();
        let now = Self::now();
        let should_save = {
            let mut m = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            match m.get(room) {
                Some(e)
                    if e.pop == pop && now.saturating_sub(e.updated_at) <= self.ttl_secs / 2 =>
                {
                    false
                }
                _ => {
                    m.insert(
                        room.to_string(),
                        Entry {
                            pop: pop.to_string(),
                            updated_at: now,
                        },
                    );
                    true
                }
            }
        };
        if should_save {
            let _ = self.save();
        }
    }

    /// 把共享文件里本进程没有的条目并入内存（本地同房间条目优先；
    /// 文件里已过期的条目跳过，避免死条目反复回流）。
    fn merge_file(&self) {
        let Some(path) = self.path.clone() else {
            return;
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return;
        };
        let Ok(file_map) = serde_json::from_str::<HashMap<String, Entry>>(&text) else {
            return;
        };
        let now = Self::now();
        let mut m = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        // 文件里的新条目应覆盖本地的过期/旧条目（or_insert 会保留本地旧值，导致
        // 本地过期条目挡住文件里的新归属 → lookup 返回 None → 调用方误重新登记覆盖）。
        for (room, entry) in file_map {
            if now.saturating_sub(entry.updated_at) > self.ttl_secs {
                continue; // 已过期：不并入（save 时也不会再写回）。
            }
            match m.get(&room) {
                Some(local) if local.updated_at >= entry.updated_at => {}
                _ => {
                    m.insert(room, entry);
                }
            }
        }
    }

    /// 当前条目数（测试/观测）。
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    fn load(&self) -> Result<(), String> {
        let path = self.path.as_ref().ok_or("no registry file path")?;
        let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        let mut data: HashMap<String, Entry> =
            serde_json::from_str(&text).map_err(|e| e.to_string())?;
        // 启动即丢弃过期条目：重启不该继承死房间的归属。
        let now = Self::now();
        data.retain(|_, e| now.saturating_sub(e.updated_at) <= self.ttl_secs);
        *self.inner.lock().unwrap_or_else(|e| e.into_inner()) = data;
        Ok(())
    }

    /// 落盘前清扫内存中的过期条目（文件随之有界），再原子写（tmp + rename）：
    /// 读方（其它 PoP）不会读到半截 JSON，共享文件并发写不产生损坏文件。
    fn save(&self) -> Result<(), String> {
        let path = self.path.as_ref().ok_or("no registry file path")?;
        let now = Self::now();
        let text = {
            let mut m = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            m.retain(|_, e| now.saturating_sub(e.updated_at) <= self.ttl_secs);
            serde_json::to_string_pretty(&*m).map_err(|e| e.to_string())?
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        // 临时文件带 pid：多 PoP 共享文件并发 save 时互不覆盖对方的 tmp。
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "registry.json".into());
        let tmp = path.with_file_name(format!("{file_name}.tmp-{}", std::process::id()));
        std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, path).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_lookup_expire() {
        let reg = PopRegistry::new(None, 3600);
        assert_eq!(reg.lookup("r"), None);
        reg.register("r", "pop-a");
        assert_eq!(reg.lookup("r"), Some("pop-a".into()));
        // TTL=1：sleep 2s 后过期
        let reg2 = PopRegistry::new(None, 1);
        reg2.register("r2", "pop-b");
        assert_eq!(reg2.lookup("r2"), Some("pop-b".into()));
        std::thread::sleep(std::time::Duration::from_secs(2));
        assert_eq!(reg2.lookup("r2"), None, "expired entry must vanish");
        assert_eq!(reg2.len(), 0, "expired entry must be cleaned");
    }

    #[test]
    fn load_save_roundtrip() {
        let dir = std::env::temp_dir().join(format!("popreg-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("reg.json");
        {
            let reg = PopRegistry::new(Some(path.clone()), 3600);
            reg.register("room-x", "pop-a");
            reg.register("room-y", "pop-b");
        }
        {
            let reg = PopRegistry::new(Some(path.clone()), 3600);
            assert_eq!(reg.lookup("room-x"), Some("pop-a".into()));
            assert_eq!(reg.lookup("room-y"), Some("pop-b".into()));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_local_entry_does_not_block_newer_file_entry() {
        let dir = std::env::temp_dir().join(format!("popreg-test3-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("reg.json");
        let reg = PopRegistry::new(Some(path.clone()), 1); // TTL=1s
        reg.register("room-x", "pop-a"); // 本地 + 文件登记
        std::thread::sleep(std::time::Duration::from_secs(2)); // 本地条目过期
        // 外部写入更新的有效归属（updated_at 更大）
        let external = "{\"room-x\":{\"pop\":\"pop-b\",\"updated_at\":9999999999}}";
        std::fs::write(&path, external).unwrap();
        // 本地过期条目不应挡住文件里的新有效条目。
        assert_eq!(reg.lookup("room-x"), Some("pop-b".into()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refresh_picks_up_external_writes() {
        let dir = std::env::temp_dir().join(format!("popreg-test2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("reg.json");
        let reg = PopRegistry::new(Some(path.clone()), 3600);
        // 外部进程写了一条登记（模拟其它 PoP）
        let external = "{\"room-x\":{\"pop\":\"pop-b\",\"updated_at\":9999999999}}";
        std::fs::write(&path, external).unwrap();
        assert_eq!(
            reg.lookup("room-x"),
            Some("pop-b".into()),
            "miss should refresh from file"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 落盘清扫（M3）：save 前丢弃过期条目，文件与内存都不随死房间无限增长；
    /// 原子写（tmp+rename）不残留临时文件。
    #[test]
    fn save_sweeps_expired_and_leaves_no_tmp() {
        let dir = std::env::temp_dir().join(format!("popreg-sweep-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("reg.json");
        {
            let reg = PopRegistry::new(Some(path.clone()), 1); // TTL=1s
            reg.register("stale-room", "pop-a");
            std::thread::sleep(std::time::Duration::from_secs(2));
            reg.register("fresh-room", "pop-b"); // 归属变化触发 save
            assert_eq!(reg.len(), 1, "内存中的过期条目应被清扫");
        }
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("fresh-room"));
        assert!(!text.contains("stale-room"), "文件中的过期条目应被清扫：{text}");
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "原子写不应残留 tmp 文件");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
