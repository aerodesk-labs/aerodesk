//! TLS 证书加载。
//!
//! 优先从文件加载（`CERT_FILE` / `KEY_FILE` 环境变量，配合 certbot/ACME 自动化），
//! 未设置或读取失败时回退到仓库内嵌的开发证书（`certs/`）。

/// TLS 身份（证书 + 私钥，PEM 字节）。
pub struct TlsIdentity {
    pub cert: Vec<u8>,
    pub key: Vec<u8>,
    /// 来源："file"（CERT_FILE/KEY_FILE）或 "embedded"（开发证书）。
    pub source: &'static str,
}

impl TlsIdentity {
    /// 按优先级加载：文件 > 内嵌开发证书。
    pub fn load() -> TlsIdentity {
        let cert_env = std::env::var("CERT_FILE").ok();
        let key_env = std::env::var("KEY_FILE").ok();
        if let (Some(c), Some(k)) = (cert_env, key_env) {
            match (std::fs::read(&c), std::fs::read(&k)) {
                (Ok(cert), Ok(key)) if !cert.is_empty() && !key.is_empty() => {
                    return TlsIdentity {
                        cert,
                        key,
                        source: "file",
                    };
                }
                _ => {}
            }
        }
        TlsIdentity {
            cert: include_bytes!("../../../certs/cer.pem").to_vec(),
            key: include_bytes!("../../../certs/key.pem").to_vec(),
            source: "embedded",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn falls_back_to_embedded_dev_cert() {
        // 不设置 CERT_FILE/KEY_FILE（或指向不存在路径）→ 内嵌证书。
        let id = TlsIdentity::load();
        assert_eq!(id.source, "embedded");
        let cert = String::from_utf8_lossy(&id.cert);
        assert!(cert.starts_with("-----BEGIN"));
    }
}
