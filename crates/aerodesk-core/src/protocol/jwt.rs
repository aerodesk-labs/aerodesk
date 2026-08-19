//! JWT 认证工具（信令服务校验 / CLI 与运维签发共用）。
//!
//! HS256 签名；Claims 携带用户/设备/房间/角色授权。
//! 环境变量 `JWT_SECRET` 为共享密钥（生产用强随机值，部署文档说明）。

use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::signal::Role;

/// 任意房间/角色通配符。
pub const WILDCARD: &str = "*";

/// JWT Claims（与 jsonwebtoken 的 exp/iat 命名一致）。
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
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_secs() as usize;
    let claims = Claims {
        sub: user.to_string(),
        dev: device.map(|d| d.to_string()),
        room: room.map(|r| r.to_string()),
        role: role.map(|r| role_name(r).to_string()),
        max_conns,
        iat: now,
        exp: now.saturating_add(ttl_secs as usize).max(now + 1),
    };
    let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256);
    jsonwebtoken::encode(&header, &claims, &encoding_key(secret))
        .map_err(|e| format!("jwt encode: {e}"))
}

/// 校验 JWT：签名、过期、房间与角色授权。返回解码后的 Claims。
pub fn validate_token(secret: &str, token: &str, room: &str, role: Role) -> Result<Claims, String> {
    let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::HS256);
    // 明确要求 iat/exp 存在且未过期，不做过期宽限。
    validation.leeway = 0;
    validation.set_required_spec_claims(&["exp", "iat"]);

    let data = jsonwebtoken::decode::<Claims>(token, &decoding_key(secret), &validation)
        .map_err(|e| format!("jwt invalid: {e}"))?;
    let claims = data.claims;

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

fn encoding_key(secret: &str) -> jsonwebtoken::EncodingKey {
    jsonwebtoken::EncodingKey::from_secret(secret.as_bytes())
}

fn decoding_key(secret: &str) -> jsonwebtoken::DecodingKey {
    jsonwebtoken::DecodingKey::from_secret(secret.as_bytes())
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
}
