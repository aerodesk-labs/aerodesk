//! TLS 证书加载。
//!
//! 优先从文件加载（`CERT_FILE` / `KEY_FILE` 环境变量，配合 certbot/ACME 自动化）。
//!
//! 安全策略（#11）：
//! - 两个环境变量都**未设置**（或为空）→ 开发模式，回退到仓库内嵌证书（`certs/`）；
//! - **任一显式配置**后，读取失败/为空/只配一半 → **fail fast**（返回 Err，服务启动即退出），
//!   绝不静默回退到内嵌开发证书（其私钥在公开仓库中，生产误用会被 MITM）。

/// TLS 身份（证书 + 私钥，PEM 字节）。
#[derive(Debug)]
pub struct TlsIdentity {
    pub cert: Vec<u8>,
    pub key: Vec<u8>,
    /// 来源："file"（CERT_FILE/KEY_FILE）或 "embedded"（开发证书）。
    pub source: &'static str,
}

fn env_set(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

impl TlsIdentity {
    /// 加载 TLS 身份。显式配置出错时返回 Err（调用方应终止启动）。
    pub fn load() -> Result<TlsIdentity, String> {
        let cert_env = env_set("CERT_FILE");
        let key_env = env_set("KEY_FILE");
        match (cert_env, key_env) {
            // 开发模式：两者都未配置 → 内嵌开发证书。
            (None, None) => Ok(TlsIdentity {
                cert: include_bytes!("../../../../certs/cer.pem").to_vec(),
                key: include_bytes!("../../../../certs/key.pem").to_vec(),
                source: "embedded",
            }),
            // 显式配置：读取失败/内容为空 → fail fast。
            (Some(c), Some(k)) => {
                let cert = std::fs::read(&c).map_err(|e| format!("CERT_FILE={c} 读取失败: {e}"))?;
                let key = std::fs::read(&k).map_err(|e| format!("KEY_FILE={k} 读取失败: {e}"))?;
                if cert.is_empty() {
                    return Err(format!("CERT_FILE={c} 为空"));
                }
                if key.is_empty() {
                    return Err(format!("KEY_FILE={k} 为空"));
                }
                Ok(TlsIdentity {
                    cert,
                    key,
                    source: "file",
                })
            }
            // 只配了一半 → 配置错误，fail fast（避免悄悄用不匹配的证书组合）。
            (Some(c), None) => Err(format!(
                "CERT_FILE 已设置（{c}）但 KEY_FILE 未设置；两者必须同时配置"
            )),
            (None, Some(k)) => Err(format!(
                "KEY_FILE 已设置（{k}）但 CERT_FILE 未设置；两者必须同时配置"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // env 是进程全局的，串行化避免并行测试互踩。
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn clear_env() {
        unsafe {
            std::env::remove_var("CERT_FILE");
            std::env::remove_var("KEY_FILE");
        }
    }

    fn set_env(cert: &str, key: &str) {
        unsafe {
            std::env::set_var("CERT_FILE", cert);
            std::env::set_var("KEY_FILE", key);
        }
    }

    fn pem(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("aerodesk-tls-test-{}-{}", std::process::id(), name))
    }

    #[test]
    fn falls_back_to_embedded_dev_cert_when_unset() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        let id = TlsIdentity::load().expect("unset env → embedded");
        assert_eq!(id.source, "embedded");
        assert!(String::from_utf8_lossy(&id.cert).starts_with("-----BEGIN"));
    }

    #[test]
    fn empty_env_value_treated_as_unset() {
        let _g = ENV_LOCK.lock().unwrap();
        set_env("", "");
        let id = TlsIdentity::load().expect("empty env → embedded");
        assert_eq!(id.source, "embedded");
    }

    #[test]
    fn loads_from_files_when_configured() {
        let _g = ENV_LOCK.lock().unwrap();
        let cert = pem("cert.pem");
        let key = pem("key.pem");
        std::fs::write(
            &cert,
            b"-----BEGIN CERTIFICATE-----\nAAA\n-----END CERTIFICATE-----\n",
        )
        .unwrap();
        std::fs::write(
            &key,
            b"-----BEGIN PRIVATE KEY-----\nBBB\n-----END PRIVATE KEY-----\n",
        )
        .unwrap();
        set_env(cert.to_str().unwrap(), key.to_str().unwrap());
        let id = TlsIdentity::load().expect("valid files");
        assert_eq!(id.source, "file");
        assert!(String::from_utf8_lossy(&id.key).contains("BBB"));
        let _ = std::fs::remove_file(&cert);
        let _ = std::fs::remove_file(&key);
    }

    #[test]
    fn missing_file_is_fail_fast() {
        let _g = ENV_LOCK.lock().unwrap();
        set_env("/nonexistent/cert.pem", "/nonexistent/key.pem");
        let err = TlsIdentity::load().unwrap_err();
        assert!(err.contains("CERT_FILE"), "err: {err}");
    }

    #[test]
    fn empty_file_is_fail_fast() {
        let _g = ENV_LOCK.lock().unwrap();
        let cert = pem("empty-cert.pem");
        let key = pem("key2.pem");
        std::fs::write(&cert, b"").unwrap();
        std::fs::write(&key, b"-----BEGIN PRIVATE KEY-----\n").unwrap();
        set_env(cert.to_str().unwrap(), key.to_str().unwrap());
        let err = TlsIdentity::load().unwrap_err();
        assert!(err.contains("为空"), "err: {err}");
        let _ = std::fs::remove_file(&cert);
        let _ = std::fs::remove_file(&key);
    }

    #[test]
    fn partial_config_is_fail_fast() {
        let _g = ENV_LOCK.lock().unwrap();
        set_env("/some/cert.pem", "");
        let err = TlsIdentity::load().unwrap_err();
        assert!(err.contains("KEY_FILE 未设置"), "err: {err}");
        clear_env();
        set_env("", "/some/key.pem");
        let err = TlsIdentity::load().unwrap_err();
        assert!(err.contains("CERT_FILE 未设置"), "err: {err}");
    }
}
