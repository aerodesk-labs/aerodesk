//! TURN 临时凭证生成（coturn REST API 规范）。
//!
//! 服务端配置 coturn：`--use-auth-secret --static-auth-secret=<secret>`
//! 客户端凭证：`username = <expiry>:<userid>`，
//! `credential = base64(HMAC-SHA1(secret, username))`。
//! 参考 coturn 文档 "REST API" 一节。

pub mod codec;

use base64ct::Encoding;
use hmac::{Hmac, Mac};
use sha1::Sha1;

type HmacSha1 = Hmac<Sha1>;

/// TURN 凭证（一次性、限时）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnCredentials {
    pub username: String,
    pub credential: String,
}

/// coturn REST 风格凭证值：`base64(HMAC-SHA1(secret, username))`。
pub fn turn_credential(secret: &str, username: &str) -> String {
    let mut mac = HmacSha1::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key size");
    mac.update(username.as_bytes());
    let digest = mac.finalize().into_bytes();
    base64ct::Base64::encode_string(&digest)
}

/// 校验 coturn REST 风格凭证（服务端用）：
/// `username = "<expiry>:<userid>"`，`credential = base64(HMAC-SHA1(secret, username))`；
/// expiry 为 Unix 秒（允许 `skew_secs` 时钟偏差）。
pub fn verify_turn_credential(
    secret: &str,
    username: &str,
    credential: &str,
    now_unix: u64,
    skew_secs: u64,
) -> bool {
    let Some((expiry_s, _user)) = username.split_once(':') else {
        return false;
    };
    let Ok(expiry) = expiry_s.parse::<u64>() else {
        return false;
    };
    if expiry + skew_secs < now_unix {
        return false; // 已过期
    }
    turn_credential(secret, username) == credential
}

/// 按 coturn REST 规范生成限时凭证。
///
/// `now_unix` 为当前 Unix 时间戳（秒），便于测试注入。
pub fn generate_turn_credentials(
    secret: &str,
    user_id: &str,
    ttl_secs: u64,
    now_unix: u64,
) -> TurnCredentials {
    let expiry = now_unix + ttl_secs;
    let username = format!("{expiry}:{user_id}");

    let mut mac = HmacSha1::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key size");
    mac.update(username.as_bytes());
    let digest = mac.finalize().into_bytes();

    TurnCredentials {
        username,
        credential: base64ct::Base64::encode_string(&digest),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// coturn 文档 REST 示例向量：
    /// secret="logen", username="1446193098:myuser"（RFC 7191 风格示例）。
    #[test]
    fn matches_known_vector() {
        let c = generate_turn_credentials("logen", "myuser", 3600, 1446193098 - 3600);
        assert_eq!(c.username, "1446193098:myuser");
        // 该向量的 password 为 base64(HMAC-SHA1("logen", "1446193098:myuser"))
        let expected = "t0/eFcrPz2ELstTkzvp9HX7RwDU=";
        assert_eq!(c.credential, expected);
    }

    #[test]
    fn expires_after_ttl() {
        let secret = "s3cret";
        let a = generate_turn_credentials(secret, "alice", 3600, 1_000_000);
        let b = generate_turn_credentials(secret, "alice", 3600, 1_000_001);
        assert_ne!(a.username, b.username);
        assert_ne!(a.credential, b.credential);
    }

    #[test]
    fn verify_matches_generated() {
        let c = generate_turn_credentials("s3cret", "alice", 3600, 1_000_000);
        assert!(verify_turn_credential(
            "s3cret",
            &c.username,
            &c.credential,
            1_000_000,
            300
        ));
        // expiry = 1_000_000 + 3600 = 1_003_600；skew 300 内仍有效
        assert!(verify_turn_credential(
            "s3cret",
            &c.username,
            &c.credential,
            1_003_600 + 300,
            300
        ));
        // 超过 skew 视为过期
        assert!(!verify_turn_credential(
            "s3cret",
            &c.username,
            &c.credential,
            1_003_600 + 301,
            300
        ));
        assert!(!verify_turn_credential(
            "wrong",
            &c.username,
            &c.credential,
            1_000_000,
            300
        ));
        assert!(!verify_turn_credential(
            "s3cret",
            "not-a-username",
            &c.credential,
            1_000_000,
            300
        ));
    }

    #[test]
    fn deterministic_with_same_time() {
        let a = generate_turn_credentials("s", "u", 3600, 42);
        let b = generate_turn_credentials("s", "u", 3600, 42);
        assert_eq!(a, b);
    }
}
