//! 本机 host candidate 地址枚举与筛选。
//!
//! 背景（2026-10-08 实测，P0）：候选原先只取**默认路由的出接口地址**（绑 0.0.0.0 连
//! 公共地址后读 local_addr）。只要默认路由落在 TUN/VPN 上（Clash TUN、WireGuard、
//! 企业 VPN、Tailscale……），唯一的 host candidate 就是隧道地址——两端即便在同一台
//! 机器上、各自都通告 `198.18.0.1`，ICE 也只能把包发给隧道，永远连不上。
//!
//! 这里改为**枚举所有本机接口地址**并给出稳定次序：物理接口优先，隧道/容器/虚拟网桥
//! 排在最后（不删——只用 VPN 的主机仍需它兜底），明显不可用的地址直接剔除。

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// 隧道/虚拟/容器接口名前缀（排到最后）。
const VIRTUAL_PREFIXES: &[&str] = &[
    "utun",
    "ipsec",
    "tun",
    "tap",
    "wg",
    "tailscale",
    "npcap",
    "docker",
    "podman",
    "br-",
    "veth",
    "vmnet",
    "vboxnet",
    "virbr",
    "llw",
    "awdl",
    "anpi",
    "bridge",
];

/// 物理接口名前缀（排在最前：macOS `en*`、Linux `eth*`/`wlan*`/`wl*`、Windows 常见名）。
const PHYSICAL_PREFIXES: &[&str] = &[
    "en",
    "eth",
    "wlan",
    "wl",
    "wifi",
    "wi-fi",
    "ethernet",
    "local area connection",
];

/// 是否隧道/虚拟/容器接口（大小写不敏感）。
pub fn is_virtual_iface(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    VIRTUAL_PREFIXES.iter().any(|p| n.starts_with(p))
}

/// 是否物理接口（大小写不敏感）。
pub fn is_physical_iface(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    PHYSICAL_PREFIXES.iter().any(|p| n.starts_with(p))
}

/// 该地址能否作为 host candidate。
///
/// 剔除：未指定、组播、广播、链路本地（169.254/16、fe80::/10）、
/// **fake-IP 段 198.18.0.0/15**（Clash 之类代理的假地址，任何对端都不可达）、
/// 以及 IPv6 未指定。
pub fn is_usable_candidate_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            !v4.is_unspecified()
                && !v4.is_multicast()
                && !v4.is_broadcast()
                && !v4.is_link_local()
                && !is_fake_ip_v4(v4)
        }
        IpAddr::V6(v6) => {
            !v6.is_unspecified()
                && !v6.is_multicast()
                && !is_link_local_v6(v6)
                // IPv4-mapped（::ffff:a.b.c.d）按 v4 规则判。
                && match v6.to_ipv4_mapped() {
                    Some(v4) => is_usable_candidate_ip(IpAddr::V4(v4)),
                    None => true,
                }
        }
    }
}

/// 198.18.0.0/15：RFC 2544 基准测试段，被 Clash 等代理当 fake-IP 池用——不是真实可达地址。
fn is_fake_ip_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] == 198 && (o[1] == 18 || o[1] == 19)
}

fn is_link_local_v6(ip: Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xffc0) == 0xfe80
}

/// 地址次序权重：物理接口 0、未知 1、虚拟/隧道 2。
pub fn iface_rank(name: &str) -> u8 {
    if is_virtual_iface(name) {
        2
    } else if is_physical_iface(name) {
        0
    } else {
        1
    }
}

/// 枚举本机可用的 host candidate 地址（物理优先，虚拟/隧道最后；去重且保序）。
///
/// 接口枚举失败（极少见）时返回空 Vec——调用方负责回退到出接口探测 / loopback。
pub fn local_host_candidates() -> Vec<IpAddr> {
    let mut entries: Vec<(u8, IpAddr)> = Vec::new();
    for iface in if_addrs::get_if_addrs().unwrap_or_default() {
        let ip = iface.addr.ip();
        if !is_usable_candidate_ip(ip) {
            continue;
        }
        // 回环不进候选（只有显式绑回环的调用方才会自己加）。
        if ip.is_loopback() {
            continue;
        }
        entries.push((iface_rank(&iface.name), ip));
    }
    entries.sort_by_key(|(rank, _)| *rank);
    let mut seen = std::collections::HashSet::new();
    entries
        .into_iter()
        .map(|(_, ip)| ip)
        .filter(|ip| seen.insert(*ip))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_and_physical_classification() {
        for n in [
            "utun4",
            "wg0",
            "tailscale0",
            "docker0",
            "podman0",
            "br-3ec8",
            "vmnet8",
        ] {
            assert!(is_virtual_iface(n), "{n} 应判为虚拟");
        }
        for n in ["en0", "en1", "eth0", "wlan0", "wl0", "Ethernet"] {
            assert!(!is_virtual_iface(n), "{n} 不应判为虚拟");
            assert!(is_physical_iface(n), "{n} 应判为物理");
        }
        assert!(iface_rank("en0") < iface_rank("zz9"));
        assert!(iface_rank("zz9") < iface_rank("utun4"));
    }

    #[test]
    fn unusable_ips_are_rejected() {
        let bad = [
            "0.0.0.0",
            "169.254.1.1",
            "224.0.0.1",
            "198.18.0.1",   // Clash fake-IP（实测就是它把 ICE 卡死的）
            "198.19.255.1", // fake-IP 段另一半
            "::",
            "fe80::1",
            "ff02::1",
        ];
        for s in bad {
            assert!(
                !is_usable_candidate_ip(s.parse().unwrap()),
                "{s} 不应作为候选"
            );
        }
        for s in [
            "192.168.1.5",
            "10.0.0.9",
            "172.19.52.111",
            "100.64.0.7",
            "2001:db8::1",
        ] {
            assert!(
                is_usable_candidate_ip(s.parse().unwrap()),
                "{s} 应可作为候选"
            );
        }
    }
}
