//! 最小 TURN Allocate 客户端（RFC 5766 长时凭证）。
//!
//! 里程碑 1（#157）：完成 Allocate 拿到 relayed 地址（含 401→REALM/NONCE 重试、
//! MESSAGE-INTEGRITY HMAC-SHA1）。数据通路（str0m `is` TURN socket 补丁 + 真实
//! coturn 联调）为里程碑 2，见 ADR-0005。

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::time::Duration;

use md5::Md5;
use sha1::{Digest, Sha1};

const STUN_MAGIC: u32 = 0x2112_a442;
const MSG_ALLOCATE: u16 = 0x0003;
const MSG_SUCCESS: u16 = 0x0103;
const MSG_ERROR: u16 = 0x0113;

const ATTR_MAPPED_ADDRESS: u16 = 0x0001;
const ATTR_USERNAME: u16 = 0x0006;
const ATTR_MESSAGE_INTEGRITY: u16 = 0x0008;
const ATTR_ERROR_CODE: u16 = 0x0009;
const ATTR_REALM: u16 = 0x0014;
const ATTR_NONCE: u16 = 0x0015;
const ATTR_REQUESTED_TRANSPORT: u16 = 0x0019;
const ATTR_XOR_RELAYED_ADDRESS: u16 = 0x0022;
const ATTR_SOFTWARE: u16 = 0x8022;

/// Allocate 成功后的分配信息。
pub struct TurnAllocation {
    /// TURN 服务器分配的 relayed 传输地址（对端发往该地址即中继到本客户端）。
    pub relayed_addr: SocketAddr,
    /// TURN 服务器地址（响应来源）。
    pub server_addr: SocketAddr,
    /// 维持 allocation 的 UDP socket（调用方需持有；Drop 后分配过期）。
    pub socket: UdpSocket,
}

/// 发起 TURN Allocate（UDP），返回 relayed 地址。
///
/// 流程：无凭证 Allocate → 401（REALM/NONCE）→ 带 USERNAME/REALM/NONCE/
/// MESSAGE-INTEGRITY 重试 → 200（XOR-RELAYED-ADDRESS）。
pub fn allocate(
    server: SocketAddr,
    username: &str,
    password: &str,
    timeout: Duration,
) -> Result<TurnAllocation, String> {
    let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("bind: {e}"))?;
    socket
        .set_read_timeout(Some(timeout))
        .map_err(|e| format!("set_read_timeout: {e}"))?;

    // 1) 无凭证 Allocate，期望 401 拿 REALM/NONCE
    let txid = random_txid();
    let req = build_allocate(txid, None, None)?;
    socket
        .send_to(&req, server)
        .map_err(|e| format!("send allocate (unauth): {e}"))?;
    let (resp, _) = recv_stun(&socket, txid)?;
    let (realm, nonce, err) = parse_common(&resp)?;
    if err != Some(401) {
        return Err(format!("allocate expected 401, got error={err:?}"));
    }
    let realm = realm.ok_or("no realm in 401")?;
    let nonce = nonce.ok_or("no nonce in 401")?;

    // 2) 带长时凭证重试
    let txid2 = random_txid();
    let req2 = build_allocate(txid2, Some((username, password, &realm, &nonce)), None)?;
    socket
        .send_to(&req2, server)
        .map_err(|e| format!("send allocate (auth): {e}"))?;
    let (resp2, from) = recv_stun(&socket, txid2)?;
    let (_, _, err) = parse_common(&resp2)?;
    if err.is_some() && err != Some(401) {
        return Err(format!("allocate auth failed: error={err:?}"));
    }
    let relayed = parse_xor_relayed(&resp2)
        .ok_or_else(|| "no XOR-RELAYED-ADDRESS in allocate response".to_string())?;

    Ok(TurnAllocation {
        relayed_addr: relayed,
        server_addr: from,
        socket,
    })
}

// ---------- STUN 编码/解码 ----------

fn random_txid() -> [u8; 12] {
    let mut t = [0u8; 12];
    // 无 rand 依赖：用时间 + 栈地址熵（TURN 场景足够；生产可换 rand）
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let addr = &t as *const _ as usize as u64;
    t[..8].copy_from_slice(&(now as u64).to_le_bytes());
    t[8..].copy_from_slice(&addr.to_le_bytes()[..4]);
    t
}

struct Attr {
    ty: u16,
    value: Vec<u8>,
}

fn encode_header(msg_type: u16, txid: [u8; 12], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(20 + body.len());
    out.extend_from_slice(&msg_type.to_be_bytes());
    out.extend_from_slice(&(body.len() as u16).to_be_bytes());
    out.extend_from_slice(&STUN_MAGIC.to_be_bytes());
    out.extend_from_slice(&txid);
    out.extend_from_slice(body);
    out
}

fn build_allocate(
    txid: [u8; 12],
    auth: Option<(&str, &str, &str, &str)>, // (username, password, realm, nonce)
    _lifetime: Option<u32>,
) -> Result<Vec<u8>, String> {
    let mut attrs: Vec<Attr> = Vec::new();
    // REQUESTED-TRANSPORT: UDP (17)
    attrs.push(Attr {
        ty: ATTR_REQUESTED_TRANSPORT,
        value: vec![17, 0, 0, 0],
    });
    if let Some((username, _password, realm, nonce)) = auth {
        attrs.push(Attr {
            ty: ATTR_USERNAME,
            value: username.as_bytes().to_vec(),
        });
        attrs.push(Attr {
            ty: ATTR_REALM,
            value: realm.as_bytes().to_vec(),
        });
        attrs.push(Attr {
            ty: ATTR_NONCE,
            value: nonce.as_bytes().to_vec(),
        });
    }
    attrs.push(Attr {
        ty: ATTR_SOFTWARE,
        value: b"aerodesk".to_vec(),
    });

    let mut body = Vec::new();
    for a in attrs {
        let len = a.value.len();
        let pad = (4 - (len % 4)) % 4;
        body.extend_from_slice(&a.ty.to_be_bytes());
        body.extend_from_slice(&(len as u16).to_be_bytes());
        body.extend_from_slice(&a.value);
        body.extend(std::iter::repeat(0).take(pad));
    }
    // MESSAGE-INTEGRITY 必须最后（RFC 5389 §15.4）；HMAC-SHA1(key=MD5(user:realm:pass))
    if let Some((username, password, realm, _nonce)) = auth {
        let key = md5_key(username, realm, password);
        body.extend_from_slice(&ATTR_MESSAGE_INTEGRITY.to_be_bytes());
        body.extend_from_slice(&(20u16).to_be_bytes());
        let mi_start = body.len();
        body.extend(std::iter::repeat(0).take(20));
        let msg = encode_header(MSG_ALLOCATE, txid, &body);
        // RFC 5389 §15.4：MESSAGE-INTEGRITY 覆盖到 MI 属性值之前（不含 20 字节值）。
        let mac = hmac_sha1(&key, &msg[..msg.len() - 20]);
        body[mi_start..mi_start + 20].copy_from_slice(&mac);
    }
    Ok(encode_header(MSG_ALLOCATE, txid, &body))
}

fn md5_key(username: &str, realm: &str, password: &str) -> Vec<u8> {
    Md5::digest(format!("{username}:{realm}:{password}").as_bytes()).to_vec()
}

fn hmac_sha1(key: &[u8], msg: &[u8]) -> [u8; 20] {
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

fn recv_stun(socket: &UdpSocket, txid: [u8; 12]) -> Result<(Vec<u8>, SocketAddr), String> {
    let mut buf = [0u8; 2048];
    let (n, from) = socket
        .recv_from(&mut buf)
        .map_err(|e| format!("recv: {e}"))?;
    let pkt = buf[..n].to_vec();
    if pkt.len() < 20 {
        return Err("short STUN packet".into());
    }
    if pkt[4..8] != STUN_MAGIC.to_be_bytes() {
        return Err("bad STUN magic".into());
    }
    if pkt[8..20] != txid {
        return Err("STUN transaction id mismatch".into());
    }
    Ok((pkt, from))
}

fn parse_attrs(pkt: &[u8]) -> Vec<(u16, Vec<u8>)> {
    let mut out = Vec::new();
    let mut i = 20usize;
    while i + 4 <= pkt.len() {
        let ty = u16::from_be_bytes([pkt[i], pkt[i + 1]]);
        let len = u16::from_be_bytes([pkt[i + 2], pkt[i + 3]]) as usize;
        let end = (i + 4 + len).min(pkt.len());
        out.push((ty, pkt[i + 4..end].to_vec()));
        i = (i + 4 + len + 3) & !3;
    }
    out
}

/// 解析 401/200 公共属性：返回 (realm, nonce, error_code)。
fn parse_common(pkt: &[u8]) -> Result<(Option<String>, Option<String>, Option<u16>), String> {
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

fn parse_xor_relayed(pkt: &[u8]) -> Option<SocketAddr> {
    for (ty, v) in parse_attrs(pkt) {
        if ty == ATTR_XOR_RELAYED_ADDRESS && v.len() >= 8 {
            let family = v[0];
            let port = u16::from_be_bytes([v[2], v[3]]) ^ ((STUN_MAGIC >> 16) as u16);
            if family == 0x01 {
                let mut addr = [0u8; 4];
                addr.copy_from_slice(&v[4..8]);
                let magic = STUN_MAGIC.to_be_bytes();
                for i in 0..4 {
                    addr[i] ^= magic[i];
                }
                return Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(addr)), port));
            }
            // IPv6 简化为不支持（v1）
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::UdpSocket;

    /// 极简 mock TURN 服务器：首个无凭证 Allocate → 401（REALM/NONCE）；
    /// 带凭证 Allocate → 校验 MESSAGE-INTEGRITY → 200 + XOR-RELAYED-ADDRESS。
    fn spawn_mock_turn(
        sock: UdpSocket,
        username: &str,
        password: &str,
    ) -> std::thread::JoinHandle<()> {
        let username = username.to_string();
        let password = password.to_string();
        std::thread::spawn(move || {
            let mut buf = [0u8; 2048];
            for _ in 0..4 {
                let (n, from) = sock.recv_from(&mut buf).unwrap();
                let pkt = &buf[..n];
                let txid = pkt[8..20].to_vec();
                let attrs = parse_attrs(pkt);
                let has_username = attrs.iter().any(|(t, _)| *t == ATTR_USERNAME);
                let has_mi = attrs.iter().any(|(t, _)| *t == ATTR_MESSAGE_INTEGRITY);
                let mut body: Vec<u8> = Vec::new();
                if !has_username || !has_mi {
                    // 401 + REALM + NONCE + ERROR-CODE
                    body.extend_from_slice(&ATTR_REALM.to_be_bytes());
                    let r = b"aerodesk.test";
                    body.extend_from_slice(&(r.len() as u16).to_be_bytes());
                    body.extend_from_slice(r);
                    body.extend(std::iter::repeat(0).take((4 - (r.len() % 4)) % 4));
                    body.extend_from_slice(&ATTR_NONCE.to_be_bytes());
                    let n2 = b"nonce-1";
                    body.extend_from_slice(&(n2.len() as u16).to_be_bytes());
                    body.extend_from_slice(n2);
                    body.extend(std::iter::repeat(0).take((4 - (n2.len() % 4)) % 4));
                    body.extend_from_slice(&ATTR_ERROR_CODE.to_be_bytes());
                    body.extend_from_slice(&4u16.to_be_bytes());
                    body.extend_from_slice(&[0, 0, 4, 1]); // 401 Unauthorized
                    let mut txid_arr = [0u8; 12];
                    txid_arr.copy_from_slice(&txid);
                    let msg = encode_header(MSG_ERROR, txid_arr, &body);
                    sock.send_to(&msg, from).unwrap();
                } else {
                    // 校验 MI：key = MD5(username:realm:password)，realm 固定
                    let realm = "aerodesk.test";
                    let key = md5_key(&username, realm, &password);
                    let mi_start = pkt.len() - 20;
                    let pkt2 = pkt.to_vec();
                    let mac = hmac_sha1(&key, &pkt2[..mi_start]);
                    if pkt2[mi_start..] != mac {
                        // 密码错误 → 401
                        body.extend_from_slice(&ATTR_ERROR_CODE.to_be_bytes());
                        body.extend_from_slice(&4u16.to_be_bytes());
                        body.extend_from_slice(&[0, 0, 4, 1]);
                        let mut txid_arr = [0u8; 12];
                        txid_arr.copy_from_slice(&txid);
                        let msg = encode_header(MSG_ERROR, txid_arr, &body);
                        sock.send_to(&msg, from).unwrap();
                        continue;
                    }
                    // 200 + XOR-MAPPED + XOR-RELAYED-ADDRESS (203.0.113.5:5000)
                    let relayed = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 5)), 5000);
                    let mut xaddr = Vec::new();
                    xaddr.push(0x01u8);
                    xaddr.push(0x00u8);
                    // RFC 5766：端口与 (magic>>16) XOR 后传输
                    xaddr.extend_from_slice(
                        &(relayed.port() ^ ((STUN_MAGIC >> 16) as u16)).to_be_bytes(),
                    );
                    let magic = STUN_MAGIC.to_be_bytes();
                    match relayed.ip() {
                        IpAddr::V4(ip4) => {
                            for (i, b) in ip4.octets().iter().enumerate() {
                                xaddr.push(b ^ magic[i]);
                            }
                        }
                        _ => unreachable!(),
                    }
                    body.extend_from_slice(&ATTR_XOR_RELAYED_ADDRESS.to_be_bytes());
                    body.extend_from_slice(&(xaddr.len() as u16).to_be_bytes());
                    body.extend_from_slice(&xaddr);
                    body.extend(std::iter::repeat(0).take((4 - (xaddr.len() % 4)) % 4));
                    let mut txid_arr = [0u8; 12];
                    txid_arr.copy_from_slice(&txid);
                    let msg = encode_header(MSG_SUCCESS, txid_arr, &body);
                    sock.send_to(&msg, from).unwrap();
                }
            }
        })
    }

    #[test]
    fn allocate_gets_relayed_address() {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let server = sock.local_addr().unwrap();
        let _srv = spawn_mock_turn(sock, "user1", "pass1");
        std::thread::sleep(Duration::from_millis(100));
        let alloc = allocate(server, "user1", "pass1", Duration::from_secs(2)).expect("allocate");
        assert_eq!(
            alloc.relayed_addr,
            "203.0.113.5:5000".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(alloc.server_addr, server);
    }

    #[test]
    fn allocate_rejects_bad_password() {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let server = sock.local_addr().unwrap();
        let _srv = spawn_mock_turn(sock, "user1", "pass1");
        std::thread::sleep(Duration::from_millis(100));
        let res = allocate(server, "user1", "wrong", Duration::from_secs(2));
        assert!(res.is_err(), "wrong password must fail");
    }

    #[test]
    fn md5_key_hmac_matches_reference() {
        // 与 Python hashlib/hmac 参考值一致（RFC 5766 长时凭证 key = MD5(user:realm:pass)）
        let key = md5_key("user1", "aerodesk.test", "pass1");
        assert_eq!(
            key.iter().map(|b| format!("{b:02x}")).collect::<String>(),
            "f68e2c7bcdbfed125ca188fe968af6ba"
        );
        let mac = hmac_sha1(&key, b"Hello");
        assert_eq!(
            mac.iter().map(|b| format!("{b:02x}")).collect::<String>(),
            "48d2d4be79e9b9166ab3a68c14e5cd58cc57ea15"
        );
    }

    #[test]
    fn hmac_sha1_rfc2202_vector() {
        // RFC 2202 test case 1: key=0x0b*20, data="Hi There"
        let key = [0x0bu8; 20];
        let mac = hmac_sha1(&key, b"Hi There");
        let expect: [u8; 20] = [
            0xb6, 0x17, 0x31, 0x86, 0x55, 0x05, 0x72, 0x64, 0xe2, 0x8b, 0xc0, 0xb6, 0xfb, 0x37,
            0x8c, 0x8e, 0xf1, 0x46, 0xbe, 0x00,
        ];
        assert_eq!(mac, expect);
    }
}
