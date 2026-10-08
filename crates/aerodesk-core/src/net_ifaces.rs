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
/// 剔除：未指定、组播、广播、链路本地（169.254/16、fe80::/10）、**回环**、
/// **fake-IP 段 198.18.0.0/15**（Clash 之类代理的假地址，任何对端都不可达），
/// 以及 IPv6 未指定。
///
/// 口径说明（独立评审指出此前有缝）：回环曾经在这层返回 `true`（只在
/// [`local_host_candidates`] 里过滤），而本函数的文档口径是「能不能当 host
/// candidate」——回环不能，故纠正。需要显式绑回环的调用方由自己加，不经此函数。
pub fn is_usable_candidate_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            !v4.is_unspecified()
                && !v4.is_multicast()
                && !v4.is_broadcast()
                && !v4.is_link_local()
                && !v4.is_loopback()
                && !is_fake_ip_v4(v4)
        }
        IpAddr::V6(v6) => {
            !v6.is_unspecified()
                && !v6.is_multicast()
                && !is_link_local_v6(v6)
                && !v6.is_loopback()
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

/// 候选排序 + 去重的**纯函数**形态（测试注入用）：给定 `(接口名, 地址)` 序列 →
/// 物理优先、未知次之、隧道/虚拟最后的候选地址；不可用地址剔除、同地址去重保序。
///
/// 独立评审用变异证明排序/去重此前零覆盖（反转排序、去掉去重后测试仍全绿），
/// 故把这一层从 [`local_host_candidates`] 里拆出来单测。
fn order_candidates(entries: impl IntoIterator<Item = (String, IpAddr)>) -> Vec<IpAddr> {
    let mut ranked: Vec<(u8, IpAddr)> = entries
        .into_iter()
        .filter(|(_, ip)| is_usable_candidate_ip(*ip))
        .map(|(name, ip)| (iface_rank(&name), ip))
        .collect();
    // stable sort：同 rank 内保持接口枚举顺序。
    ranked.sort_by_key(|(rank, _)| *rank);
    let mut seen = std::collections::HashSet::new();
    ranked
        .into_iter()
        .map(|(_, ip)| ip)
        .filter(|ip| seen.insert(*ip))
        .collect()
}

/// 枚举本机可用的 host candidate 地址（物理优先，虚拟/隧道最后；去重且保序）。
///
/// 接口枚举失败（极少见）时返回空 Vec——调用方负责回退到出接口探测 / loopback。
pub fn local_host_candidates() -> Vec<IpAddr> {
    order_candidates(
        if_addrs::get_if_addrs()
            .unwrap_or_default()
            .into_iter()
            .map(|iface| (iface.name, iface.addr.ip())),
    )
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
        use std::net::IpAddr;
        let bad = [
            "0.0.0.0",
            "169.254.1.1",
            "224.0.0.1",
            "198.18.0.1",        // Clash fake-IP（实测就是它把 ICE 卡死的）
            "198.19.255.1",      // fake-IP 段另一半
            "127.0.0.1",         // 回环：此前这层误返 true（口径缝，已纠）
            "::1",               // 回环 v6
            "::ffff:198.18.0.1", // IPv4-mapped fake-IP：按 v4 规则应被拒
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
            "::ffff:192.168.1.5", // IPv4-mapped 真实地址：可用
        ] {
            assert!(
                is_usable_candidate_ip(s.parse::<IpAddr>().unwrap()),
                "{s} 应可作为候选"
            );
        }
    }

    /// 回归（独立评审变异 M3/M4 证明此前零覆盖）：候选**排序**与**去重**。
    /// 注入合成接口序列，不依赖本机真实网卡。
    #[test]
    fn candidates_are_ordered_physical_first_and_deduped_in_order() {
        let entries: Vec<(String, std::net::IpAddr)> = vec![
            ("utun4".into(), "198.18.0.1".parse().unwrap()), // fake-IP → 剔
            ("en0".into(), "192.168.1.5".parse().unwrap()),
            ("docker0".into(), "172.17.0.1".parse().unwrap()),
            ("utun4".into(), "10.8.0.2".parse().unwrap()),
            ("en0".into(), "192.168.1.5".parse().unwrap()), // 重复 → 只留一次
            ("en1".into(), "10.0.0.9".parse().unwrap()),
            ("lo0".into(), "127.0.0.1".parse().unwrap()), // 回环 → 剔
            ("zz9".into(), "203.0.113.7".parse().unwrap()), // 未知名 → rank 1
        ];
        let got = order_candidates(entries);
        let want: Vec<std::net::IpAddr> = [
            "192.168.1.5", // rank 0（物理），按枚举序
            "10.0.0.9",    // rank 0
            "203.0.113.7", // rank 1（未知）
            "172.17.0.1",  // rank 2（容器/虚拟），按枚举序
            "10.8.0.2",    // rank 2
        ]
        .iter()
        .map(|s| s.parse().unwrap())
        .collect();
        assert_eq!(got, want, "物理优先 / 未知次之 / 虚拟最后，且去重保序");
    }

    /// 同一地址从两个接口上报（如 en0 与 utun 同 IP）时只保留一次，且保留首次出现的
    /// 位置（首次来自物理接口 → 仍排在前）。
    #[test]
    fn same_address_from_two_interfaces_collapses_to_one() {
        let entries: Vec<(String, std::net::IpAddr)> = vec![
            ("en0".into(), "10.1.2.3".parse().unwrap()),
            ("utun2".into(), "10.1.2.3".parse().unwrap()),
        ];
        let want: Vec<std::net::IpAddr> = vec!["10.1.2.3".parse::<std::net::IpAddr>().unwrap()];
        assert_eq!(order_candidates(entries), want);
    }

    /// 全不可用（或枚举失败）→ 空列表：调用方据此回退出接口探测 / loopback。
    #[test]
    fn all_unusable_entries_yield_empty_list() {
        let entries: Vec<(String, std::net::IpAddr)> = vec![
            ("lo0".into(), "127.0.0.1".parse().unwrap()),
            ("utun4".into(), "198.18.0.1".parse().unwrap()),
            ("en0".into(), "169.254.9.9".parse().unwrap()),
        ];
        assert!(order_candidates(entries).is_empty());
        assert!(order_candidates(Vec::new()).is_empty());
    }
}
