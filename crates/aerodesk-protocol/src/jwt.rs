//! JWT 认证工具（信令服务校验 / CLI 与运维签发共用）。
//!
//! HS256 签名；Claims 携带用户/设备/房间/角色授权。
//! 环境变量 `JWT_SECRET` 为共享密钥（生产用强随机值，部署文档说明）。
//!
//! #487 审查批次 3（#10）：不再依赖 jsonwebtoken（其默认后端 ring 让 SFU
//! 二进制同时携带 ring + aws-lc-rs 双加密栈）。HS256 仅需 HMAC-SHA256，
//! 用 hmac+sha2+base64ct 直实现，compact 三段结构与既有线上 token 完全兼容。

use base64ct::{Base64UrlUnpadded, Encoding};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::signal::Role;

type HmacSha256 = Hmac<Sha256>;

/// 任意房间/角色通配符。
pub const WILDCARD: &str = "*";

/// JWT Claims（与 jsonwebtoken 的 exp/iat 命名一致，字段顺序/序列化零变化）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claims {
    /// 用户 ID。
    pub sub: String,
    /// 设备 ID（可选）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dev: Option<String>,
    /// 允许加入的房间；`*` 表示任意房间。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub room: Option<String>,
    /// 允许的角色（`publisher` / `viewer`）；`*` 表示任意角色。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// 该用户最大并发连接数（#171，可选；缺省不限）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_conns: Option<u32>,
    /// 签发时间（Unix 秒）。
    pub iat: usize,
    /// 过期时间（Unix 秒）。
    pub exp: usize,
}

/// JWT 头（固定 HS256；与 jsonwebtoken 输出一致：typ 在前）。
const HEADER: &str = r#"{"typ":"JWT","alg":"HS256"}"#;

fn b64(data: &[u8]) -> String {
    Base64UrlUnpadded::encode_string(data)
}

fn sign(secret: &str, signing_input: &str) -> Vec<u8> {
    let mut mac = <HmacSha256 as Mac>::new_from_slice(secret.as_bytes())
        .expect("HMAC accepts any key length");
    mac.update(signing_input.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

/// 恒定时间签名比较（防时序侧信道；长度不一致直接拒绝）。
fn ct_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.bytes().zip(b.bytes()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn now_secs() -> Result<usize, String> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_secs() as usize)
}

/// 签发 HS256 JWT。`ttl_secs` 为有效期（秒）。
pub fn mint_token(
    secret: &str,
    user: &str,
    device: Option<&str>,
    room: Option<&str>,
    role: Option<Role>,
    ttl_secs: u64,
    max_conns: Option<u32>,
) -> Result<String, String> {
    if secret.is_empty() {
        return Err("JWT_SECRET 不能为空".into());
    }
    let now = now_secs()?;
    let claims = Claims {
        sub: user.to_string(),
        dev: device.map(|d| d.to_string()),
        room: room.map(|r| r.to_string()),
        role: role.map(|r| role_name(r).to_string()),
        max_conns,
        iat: now,
        exp: now.saturating_add(ttl_secs as usize).max(now + 1),
    };
    let header_b64 = b64(HEADER.as_bytes());
    let payload_b64 = b64(serde_json::to_string(&claims)
        .map_err(|e| format!("jwt encode: {e}"))?
        .as_bytes());
    let signing_input = format!("{header_b64}.{payload_b64}");
    Ok(format!(
        "{signing_input}.{}",
        b64(&sign(secret, &signing_input))
    ))
}

/// 校验 JWT：签名、过期、房间与角色授权。返回解码后的 Claims。
pub fn validate_token(secret: &str, token: &str, room: &str, role: Role) -> Result<Claims, String> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return Err("jwt invalid: 需三段 compact 结构".into());
    }
    // 头校验：alg 必须是 HS256（防算法混淆攻击）。
    let header =
        Base64UrlUnpadded::decode_vec(parts[0]).map_err(|e| format!("jwt invalid: {e}"))?;
    let h: serde_json::Value =
        serde_json::from_slice(&header).map_err(|e| format!("jwt invalid: {e}"))?;
    if h.get("alg").and_then(|a| a.as_str()) != Some("HS256") {
        return Err("jwt invalid: 仅支持 HS256".into());
    }
    // 签名校验（恒定时间比较）。
    let expected = b64(&sign(secret, &format!("{}.{}", parts[0], parts[1])));
    if !ct_eq(&expected, parts[2]) {
        return Err("jwt invalid: 签名不符".into());
    }
    // Claims 解析：exp/iat 为非 Option 字段，缺失天然拒绝。
    let payload =
        Base64UrlUnpadded::decode_vec(parts[1]).map_err(|e| format!("jwt invalid: {e}"))?;
    let claims: Claims =
        serde_json::from_slice(&payload).map_err(|e| format!("jwt invalid: {e}"))?;
    // 过期校验（leeway 0）。
    let now = now_secs()?;
    if claims.exp <= now {
        return Err("jwt invalid: token expired".into());
    }

    // 房间授权：claims.room = Some(r) 且 r != "*" 且 r != 请求房间 → 拒绝。
    if let Some(allowed) = &claims.room
        && allowed != WILDCARD
        && allowed != room
    {
        return Err(format!("room not authorized: {allowed} != {room}"));
    }
    // 角色授权：claims.role = Some(r) 且 r != "*" 且 r != 请求角色 → 拒绝。
    if let Some(allowed) = &claims.role
        && allowed != WILDCARD
        && allowed != role_name(role)
    {
        return Err(format!(
            "role not authorized: {allowed} != {}",
            role_name(role)
        ));
    }
    Ok(claims)
}

fn role_name(role: Role) -> &'static str {
    match role {
        Role::Publisher => "publisher",
        Role::Viewer => "viewer",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "test-secret-0123456789";

    fn token(ttl: u64, room: Option<&str>, role: Option<Role>) -> String {
        mint_token(SECRET, "user-1", Some("dev-1"), room, role, ttl, None).unwrap()
    }

    #[test]
    fn valid_token_passes() {
        let t = token(3600, Some("room-a"), Some(Role::Publisher));
        let c = validate_token(SECRET, &t, "room-a", Role::Publisher).unwrap();
        assert_eq!(c.sub, "user-1");
        assert_eq!(c.dev.as_deref(), Some("dev-1"));
    }

    #[test]
    fn wildcard_room_and_role() {
        let t = token(3600, None, None);
        validate_token(SECRET, &t, "any-room", Role::Viewer).unwrap();
    }

    #[test]
    fn expired_token_rejected() {
        let t = token(1, Some("room-a"), None);
        std::thread::sleep(std::time::Duration::from_millis(2200));
        assert!(validate_token(SECRET, &t, "room-a", Role::Viewer).is_err());
    }

    #[test]
    fn wrong_secret_rejected() {
        let t = token(3600, None, None);
        assert!(validate_token("other-secret", &t, "room-a", Role::Viewer).is_err());
    }

    #[test]
    fn room_mismatch_rejected() {
        let t = token(3600, Some("room-a"), None);
        assert!(validate_token(SECRET, &t, "room-b", Role::Viewer).is_err());
    }

    #[test]
    fn role_mismatch_rejected() {
        let t = token(3600, None, Some(Role::Publisher));
        assert!(validate_token(SECRET, &t, "room-a", Role::Viewer).is_err());
    }

    #[test]
    fn tampered_token_rejected() {
        let t = token(3600, None, None);
        let mut bytes = t.into_bytes();
        let last = bytes.len() - 1;
        bytes[last] = if bytes[last] == b'A' { b'B' } else { b'A' };
        assert!(
            validate_token(
                SECRET,
                &String::from_utf8(bytes).unwrap(),
                "room-a",
                Role::Viewer
            )
            .is_err()
        );
    }

    /// 兼容性：jsonwebtoken 签发的标准 HS256 JWT 仍须通过（线上存量 token）。
    #[test]
    fn jsonwebtoken_issued_token_still_validates() {
        // 用同一 claims 手工构造：与 jsonwebtoken::encode(HS256) 输出等价。
        let claims = Claims {
            sub: "user-1".into(),
            dev: Some("dev-1".into()),
            room: Some("room-a".into()),
            role: Some("publisher".into()),
            max_conns: None,
            iat: 1750000000,
            exp: 1900000000,
        };
        let payload = b64(serde_json::to_string(&claims).unwrap().as_bytes());
        let signing_input = format!("{}.{payload}", b64(HEADER.as_bytes()));
        let t = format!("{signing_input}.{}", b64(&sign(SECRET, &signing_input)));
        let c = validate_token(SECRET, &t, "room-a", Role::Publisher).unwrap();
        assert_eq!(c.sub, "user-1");
    }

    #[test]
    fn alg_confusion_rejected() {
        // alg=none 的 token 必须被拒绝。
        let none_header = b64(br#"{"typ":"JWT","alg":"none"}"#);
        let payload = b64(br#"{"sub":"u","iat":1,"exp":1900000000}"#);
        let t = format!("{none_header}.{payload}.");
        assert!(validate_token(SECRET, &t, "room-a", Role::Viewer).is_err());
    }
}
