//! STUN/TURN 线协议编解码（RFC 5389/5766）——客户端（aerodesk-core）与服务端
//! （aerodesk-sfu）共用，避免双份实现漂移。
//!
//! 互操作要点（已与真实 coturn 4.17.2 验证，见 ADR-0005）：
//! - XOR-RELAYED-ADDRESS = 0x0016（0x0022 是 RESERVATION-TOKEN）
//! - XOR 地址 family 在 byte1（byte0 保留 0x00）
//! - MESSAGE-INTEGRITY 的 HMAC 输入不含 MI 属性头 + 值（`msg.len()-24`）

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use md5::Md5;
use sha1::{Digest, Sha1};

pub const STUN_MAGIC: u32 = 0x2112_a442;

// TURN/STUN 方法。
pub const MSG_BINDING: u16 = 0x0001;
pub const MSG_ALLOCATE: u16 = 0x0003;
pub const MSG_REFRESH: u16 = 0x0004;
pub const MSG_SEND: u16 = 0x0006;
pub const MSG_CREATE_PERMISSION: u16 = 0x0008;
pub const MSG_CHANNEL_BIND: u16 = 0x0009;
pub const MSG_DATA_INDICATION: u16 = 0x0017;
/// 成功响应方法 = 请求方法 | 0x0100。
pub const MSG_SUCCESS_BASE: u16 = 0x0100;
/// 错误响应方法 = 请求方法 | 0x0110。
pub const MSG_ERROR_BASE: u16 = 0x0110;

// STUN 属性。
pub const ATTR_MAPPED_ADDRESS: u16 = 0x0001;
pub const ATTR_USERNAME: u16 = 0x0006;
pub const ATTR_MESSAGE_INTEGRITY: u16 = 0x0008;
pub const ATTR_ERROR_CODE: u16 = 0x0009;
pub const ATTR_CHANNEL_NUMBER: u16 = 0x000c;
pub const ATTR_LIFETIME: u16 = 0x000d;
pub const ATTR_XOR_PEER_ADDRESS: u16 = 0x0012;
pub const ATTR_DATA: u16 = 0x0013;
pub const ATTR_REALM: u16 = 0x0014;
pub const ATTR_NONCE: u16 = 0x0015;
/// XOR-RELAYED-ADDRESS（RFC 5766 §14.5）。
pub const ATTR_XOR_RELAYED_ADDRESS: u16 = 0x0016;
pub const ATTR_REQUESTED_TRANSPORT: u16 = 0x0019;
pub const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;
pub const ATTR_SOFTWARE: u16 = 0x8022;

/// ChannelData 起始 channel 号。
pub const CHANNEL_BASE: u16 = 0x4000;
pub const CHANNEL_MAX: u16 = 0x7fff;

/// 401/200 公共属性：realm、nonce、error_code。
pub type CommonAttrs = (Option<String>, Option<String>, Option<u16>);

pub fn encode_header(msg_type: u16, txid: [u8; 12], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(20 + body.len());
    out.extend_from_slice(&msg_type.to_be_bytes());
    out.extend_from_slice(&(body.len() as u16).to_be_bytes());
    out.extend_from_slice(&STUN_MAGIC.to_be_bytes());
    out.extend_from_slice(&txid);
    out.extend_from_slice(body);
    out
}

pub fn encode_attr(ty: u16, val: &[u8]) -> Vec<u8> {
    let pad = (4 - (val.len() % 4)) % 4;
    let mut out = Vec::with_capacity(4 + val.len() + pad);
    out.extend_from_slice(&ty.to_be_bytes());
    out.extend_from_slice(&(val.len() as u16).to_be_bytes());
    out.extend_from_slice(val);
    out.extend(std::iter::repeat_n(0, pad));
    out
}

/// 构造 STUN 请求/响应。`auth` 为 (username, password, realm, nonce)：追加
/// USERNAME/REALM/NONCE + MESSAGE-INTEGRITY（HMAC-SHA1，key=MD5(user:realm:pass)）。
pub fn build_stun(
    msg_type: u16,
    attrs: &[(u16, Vec<u8>)],
    txid: [u8; 12],
    auth: Option<(&str, &str, &str, &str)>,
) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    for (ty, val) in attrs {
        body.extend_from_slice(&encode_attr(*ty, val));
    }
    if let Some((username, password, realm, nonce)) = auth {
        body.extend_from_slice(&encode_attr(ATTR_USERNAME, username.as_bytes()));
        body.extend_from_slice(&encode_attr(ATTR_REALM, realm.as_bytes()));
        body.extend_from_slice(&encode_attr(ATTR_NONCE, nonce.as_bytes()));
        let key = md5_key(username, realm, password);
        body.extend_from_slice(&ATTR_MESSAGE_INTEGRITY.to_be_bytes());
        body.extend_from_slice(&20u16.to_be_bytes());
        let mi_start = body.len();
        body.extend(std::iter::repeat_n(0, 20));
        let msg = encode_header(msg_type, txid, &body);
        // RFC 5389 §15.4：HMAC 输入为"含调整后 length 的报文、但不含 MESSAGE-INTEGRITY
        // 属性本身（属性头 + 值）"：即 msg 去掉末尾 24 字节。
        let mac = hmac_sha1(&key, &msg[..msg.len() - 24]);
        body[mi_start..mi_start + 20].copy_from_slice(&mac);
    }
    Ok(encode_header(msg_type, txid, &body))
}

/// 校验 MESSAGE-INTEGRITY（服务端用）。`password` 为长时凭证口令（REST 模式下为
/// base64(HMAC-SHA1(secret, username)) 字符串）。
///
/// 按 MI 属性实际偏移计算 HMAC，兼容 MI 之后带 FINGERPRINT 等尾部属性的请求
/// （RFC 5389 §15.4：MI 必须在 FINGERPRINT 之前；发送方计算 MI 时 length 字段
/// 尚未包含尾部属性，校验时须把 length 改回"截至 MI 值"再算——与 pion/stun 一致）。
pub fn verify_message_integrity(pkt: &[u8], username: &str, realm: &str, password: &str) -> bool {
    let Some((mi_off, _, _)) = parse_attrs_off(pkt)
        .into_iter()
        .find(|(_, ty, _)| *ty == ATTR_MESSAGE_INTEGRITY)
    else {
        return false;
    };
    if mi_off + 4 + 20 > pkt.len() {
        return false;
    }
    // 发送方计算 MI 时 length 字段 = body 截至 MI 值（= mi_off + 4，不含 MI 之后的
    // FINGERPRINT 等）；无尾部属性时 mi_off + 4 正好等于原 length。
    let adjusted_len = (mi_off + 4) as u16;
    let mut pkt2 = pkt.to_vec();
    pkt2[2..4].copy_from_slice(&adjusted_len.to_be_bytes());
    let key = md5_key(username, realm, password);
    let mac = hmac_sha1(&key, &pkt2[..mi_off]);
    pkt[mi_off + 4..mi_off + 24] == mac
}

pub fn md5_key(username: &str, realm: &str, password: &str) -> Vec<u8> {
    Md5::digest(format!("{username}:{realm}:{password}").as_bytes()).to_vec()
}

pub fn hmac_sha1(key: &[u8], msg: &[u8]) -> [u8; 20] {
    const BLOCK: usize = 64;
    let mut key = key.to_vec();
    if key.len() > BLOCK {
        key = Sha1::digest(&key).to_vec();
    }
    key.resize(BLOCK, 0);
    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] ^= key[i];
        opad[i] ^= key[i];
    }
    let mut inner_input = Vec::with_capacity(BLOCK + msg.len());
    inner_input.extend_from_slice(&ipad);
    inner_input.extend_from_slice(msg);
    let inner = Sha1::digest(&inner_input);
    let mut outer_input = Vec::with_capacity(BLOCK + 20);
    outer_input.extend_from_slice(&opad);
    outer_input.extend_from_slice(&inner);
    let out = Sha1::digest(&outer_input);
    let mut mac = [0u8; 20];
    mac.copy_from_slice(&out);
    mac
}

pub fn parse_attrs(pkt: &[u8]) -> Vec<(u16, Vec<u8>)> {
    parse_attrs_off(pkt)
        .into_iter()
        .map(|(_, ty, v)| (ty, v))
        .collect()
}

/// 带偏移的属性解析：返回 (attr 头偏移, 类型, 值)。
pub fn parse_attrs_off(pkt: &[u8]) -> Vec<(usize, u16, Vec<u8>)> {
    let mut out = Vec::new();
    let mut i = 20usize;
    while i + 4 <= pkt.len() {
        let ty = u16::from_be_bytes([pkt[i], pkt[i + 1]]);
        let len = u16::from_be_bytes([pkt[i + 2], pkt[i + 3]]) as usize;
        let end = (i + 4 + len).min(pkt.len());
        out.push((i, ty, pkt[i + 4..end].to_vec()));
        i = (i + 4 + len + 3) & !3;
    }
    out
}

pub fn find_attr(pkt: &[u8], ty: u16) -> Option<Vec<u8>> {
    parse_attrs(pkt)
        .into_iter()
        .find(|(t, _)| *t == ty)
        .map(|(_, v)| v)
}

/// 解析 401/200 公共属性：返回 (realm, nonce, error_code)。
pub fn parse_common(pkt: &[u8]) -> Result<CommonAttrs, String> {
    let mut realm = None;
    let mut nonce = None;
    let mut err = None;
    for (ty, v) in parse_attrs(pkt) {
        match ty {
            ATTR_REALM => realm = Some(String::from_utf8_lossy(&v).to_string()),
            ATTR_NONCE => nonce = Some(String::from_utf8_lossy(&v).to_string()),
            ATTR_ERROR_CODE if v.len() >= 4 => {
                let class = (v[2] & 0x07) as u16;
                let number = v[3] as u16;
                err = Some(class * 100 + number);
            }
            _ => {}
        }
    }
    Ok((realm, nonce, err))
}

pub fn stun_method(pkt: &[u8]) -> u16 {
    if pkt.len() < 2 {
        return 0;
    }
    u16::from_be_bytes([pkt[0], pkt[1]]) & 0x3fff
}

/// 解析 XOR-MAPPED / XOR-PEER / XOR-RELAYED 地址（IPv4/IPv6）。
/// RFC 5389 §15.1：byte0 = 0（保留），byte1 = Family。
pub fn parse_xor_addr(v: &[u8]) -> Option<SocketAddr> {
    if v.len() < 8 {
        return None;
    }
    let family = v[1];
    let port = u16::from_be_bytes([v[2], v[3]]) ^ ((STUN_MAGIC >> 16) as u16);
    let magic = STUN_MAGIC.to_be_bytes();
    match family {
        0x01 => {
            let mut a = [0u8; 4];
            a.copy_from_slice(&v[4..8]);
            for i in 0..4 {
                a[i] ^= magic[i];
            }
            Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(a)), port))
        }
        0x02 if v.len() >= 20 => {
            let mut a = [0u8; 16];
            a.copy_from_slice(&v[4..20]);
            for i in 0..16 {
                a[i] ^= magic[i % 4];
            }
            Some(SocketAddr::new(IpAddr::V6(a.into()), port))
        }
        _ => None,
    }
}

pub fn encode_xor_peer(addr: SocketAddr) -> Vec<u8> {
    let mut v = Vec::with_capacity(20);
    let magic = STUN_MAGIC.to_be_bytes();
    match addr {
        SocketAddr::V4(a) => {
            v.push(0x00);
            v.push(0x01);
            v.extend_from_slice(&(a.port() ^ ((STUN_MAGIC >> 16) as u16)).to_be_bytes());
            for (i, b) in a.ip().octets().iter().enumerate() {
                v.push(b ^ magic[i]);
            }
        }
        SocketAddr::V6(a) => {
            v.push(0x00);
            v.push(0x02);
            v.extend_from_slice(&(a.port() ^ ((STUN_MAGIC >> 16) as u16)).to_be_bytes());
            for (i, b) in a.ip().octets().iter().enumerate() {
                v.push(b ^ magic[i % 4]);
            }
        }
    }
    v
}

/// ERROR-CODE 属性值（class 低 3 bit + number）。
pub fn encode_error_code(code: u16) -> Vec<u8> {
    vec![0, 0, (code / 100) as u8, (code % 100) as u8]
}
