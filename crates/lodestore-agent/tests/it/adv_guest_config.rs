#![cfg(target_os = "linux")]
//! The snapshot is written by the host, so its `pct.conf` can carry
//! anything a root user on the host puts there. Whatever it holds, the
//! configuration `pct restore` receives over the provider must be exactly
//! `filter_ct` of the snapshot's bytes: no root-only key, bind or device
//! mount point, other guest's volume, unknown bridge or firewall reaches
//! Proxmox VE, and the argv stays fixed.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};

use lodestore_format::ct::config_lines;
use lodestore_format::guest_config::{
    FilterInput, Filtered, MountKind, classify_mountpoint, filter_ct, guest_identity,
    parse_property, same_guest,
};
use lodestore_proto::agent::v1::JobOutcome;
use proptest::prelude::*;
use proptest::test_runner::TestRunner;

use crate::ct_restore::{Lab, NODE, Ran, TARGET_MAC, UID_X, UUID, new_ct, replace, succeeded};

const OTHER_MAC: &str = "BC:24:11:00:00:03";

const HOSTILE: &[&str] = &[
    "lxc.hook.pre-start: /var/lib/x.sh",
    "lxc.hook.mount = /x",
    "lxc.mount.entry: /dev dev none bind,create=dir",
    "lxc.apparmor.profile: unconfined",
    "LXC.cgroup2.devices.allow: a",
    "mp0: /,mp=/host",
    "mp1: /dev/sda1,mp=/d",
    "mp2: mp=/data,volume=/srv",
    "mp3: local-lvm:4,volume=/etc,mp=/e",
    "mp4: size=1G,volume=local-lvm:vm-999-disk-1,mp=/other",
    "mp5:  local-lvm:vm-200-disk-1,mp=/ok,size=2G,backup=0",
    "MP6: local-lvm:1,mp=/upper",
    " mp7: local-lvm:1,mp=/space",
    "mp8 : local-lvm:1,mp=/space2",
    "rootfs: /,size=8G",
    "rootfs: volume=local-lvm:vm-999-disk-0,size=8G",
    "rootfs: local-lvm:vm-200-disk-0,size=8G,mountoptions=noatime",
    "unused0: local-lvm:vm-200-disk-9",
    "net0: name=eth0,bridge=vmbr0,hwaddr=AA:BB:CC:00:00:01,firewall=1,tag=40",
    "net1: name=eth1,bridge=vmbr9,firewall=0",
    "net2: name=eth2,bridge=vmbr0,type=phys",
    "net3: bridge=vmbr0,tag=4095,trunks=1;2",
    "net31: name=eth9,bridge=vmbr0,link_down=1",
    "dev0: /dev/sda,mode=0666",
    "dev1: path=/dev/kvm",
    "hookscript: local:snippets/x.sh",
    "unprivileged: 0",
    "unprivileged: no",
    "onboot: 1",
    "protection: 1",
    "features: keyctl=1,fuse=1,mount=nfs",
    "features: nesting=1,mknod=1",
    "args: -x",
    "hostpci0: 01:00.0",
    "usb0: host=1-1",
    "env: LD_PRELOAD=/x",
    "lock: backup",
    "meta: ctime=1,uid=abcdefghijklmnopqrst,rtime=2",
    "meta: uid=bbbbbbbbbbbbbbbbbbbb",
    "hostname: other",
    "Hostname: upper",
    "description: hello",
    "# comment",
    "garbage line",
    "",
    "[pve:pending]",
    "[snap1]",
];

fn planted_config() -> impl Strategy<Value = String> {
    let line = prop_oneof![
        6 => prop::sample::select(HOSTILE).prop_map(str::to_owned),
        1 => "[a-z.]{1,12}: [ -~]{0,40}",
    ];
    (
        prop::collection::vec(line, 0..24),
        any::<bool>(),
        any::<prop::sample::Index>(),
    )
        .prop_map(|(mut lines, rootfs, at)| {
            if rootfs {
                let at = at.index(lines.len() + 1);
                lines.insert(at, "rootfs: local-lvm:vm-200-disk-0,size=8G".into());
            }
            let mut text = format!("arch: amd64\n{}", lines.join("\n"));
            text.push('\n');
            text
        })
}

const FIREWALL: &str = "[OPTIONS]\nenable: 1\nmacfilter: 0\n\n[RULES]\nIN ACCEPT -p tcp\n";

fn node_bridges() -> BTreeSet<String> {
    BTreeSet::from(["vmbr0".to_owned()])
}

fn runner() -> TestRunner {
    TestRunner::new(ProptestConfig {
        cases: 64,
        failure_persistence: None,
        ..ProptestConfig::default()
    })
}

/// (a)-(h) of a restore whose filter oracle is `oracle`.
fn check(ran: &Ran, oracle: &Filtered) -> Result<(), TestCaseError> {
    let config = ran.config.as_deref().unwrap_or_default();
    // (a) byte for byte what the filter gives for the snapshot's bytes
    prop_assert_eq!(config, oracle.text.as_str());
    // (b) no root-only key
    for line in config.lines() {
        for bad in ["lxc.", "hookscript", "dev", "args", "hostpci", "usb", "env"] {
            prop_assert!(!line.starts_with(bad), "{} in {}", line, config);
        }
    }
    // (c) unprivileged and onboot forced, once
    let lines: Vec<&str> = config.lines().collect();
    let count = |p: &str| lines.iter().filter(|l| l.starts_with(p)).count();
    prop_assert_eq!(count("unprivileged"), 1);
    prop_assert_eq!(count("unprivileged: 1"), 1);
    prop_assert_eq!(count("onboot"), 1);
    prop_assert_eq!(count("onboot: 0"), 1);
    // (d) the fixed argv
    let argv = ran.argv.clone().unwrap_or_default();
    for pair in argv.windows(2) {
        prop_assert!(pair[0] != "--ha-managed", "{:?}", argv);
    }
    for (flag, value) in [("--unprivileged", "1"), ("--onboot", "0"), ("--start", "0")] {
        let at = argv.iter().position(|a| a == flag);
        prop_assert!(at.is_some(), "{:?}", argv);
        prop_assert_eq!(argv.get(at.unwrap() + 1).map(String::as_str), Some(value));
    }
    // (e) the report lists what the filter dropped
    let reported: Vec<(String, String, String)> = ran
        .guest()
        .dropped
        .iter()
        .map(|d| (d.key.clone(), d.value.clone(), d.reason.clone()))
        .collect();
    let dropped: Vec<(String, String, String)> = oracle
        .dropped
        .iter()
        .map(|d| (d.key.clone(), d.value.clone(), d.reason.as_str().to_owned()))
        .collect();
    prop_assert_eq!(reported, dropped);
    // (f) each mount point is one allocation on a storage
    // (g) each interface on a bridge of the node
    let bridges = node_bridges();
    for (key, value) in config_lines(config) {
        if key == "rootfs" || key.starts_with("mp") {
            let defaults = value.split(',').filter(|p| !p.contains('=')).count();
            prop_assert_eq!(defaults, 1, "{}", value);
            let props = parse_property(value, Some("volume")).unwrap_or_default();
            let volume = props.get("volume").cloned().unwrap_or_default();
            prop_assert_eq!(classify_mountpoint(&volume), MountKind::Volume, "{}", value);
            let (storage, size) = volume.split_once(':').unwrap_or_default();
            prop_assert!(!storage.is_empty(), "{}", volume);
            prop_assert!(
                !size.is_empty() && size.bytes().all(|b| b.is_ascii_digit()),
                "{}",
                volume
            );
        }
        if key.starts_with("net") {
            let props = parse_property(value, None).unwrap_or_default();
            prop_assert!(
                props.get("bridge").is_none_or(|b| bridges.contains(b)),
                "{}",
                value
            );
        }
    }
    // (h) no firewall reaches Proxmox VE
    prop_assert!(!ran.firewall);
    Ok(())
}

fn filter(config: &str, target: Option<&str>) -> Option<Filtered> {
    let bridges = node_bridges();
    filter_ct(&FilterInput {
        config: config.as_bytes(),
        firewall: Some(FIREWALL.as_bytes()),
        target: target.map(str::as_bytes),
        bridges: Some(&bridges),
    })
    .ok()
    .filter(|f| f.volumes.iter().any(|v| v.key == "rootfs"))
}

fn refused_before_pct(ran: &Ran) -> Result<(), TestCaseError> {
    prop_assert_eq!(
        ran.done.outcome,
        JobOutcome::Failed as i32,
        "{:?}",
        ran.done
    );
    prop_assert_eq!(&ran.done.error_code, "restore_config_invalid");
    prop_assert!(ran.argv.is_none());
    Ok(())
}

#[test]
fn adv_guest_config_planted() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let lab = rt.block_on(Lab::new());
    let ran_pct = AtomicUsize::new(0);
    runner()
        .run(&planted_config(), |config| {
            let _ = fs::remove_file(lab.pve.join(format!("nodes/{NODE}/lxc/105.conf")));
            let snap = rt.block_on(lab.snapshot(&config, Some(FIREWALL), |_| {}, &[]));
            let ran = rt.block_on(lab.run(&snap, lab.spec(&snap, new_ct(105))));
            match filter(&config, None) {
                None => refused_before_pct(&ran),
                Some(oracle) => {
                    succeeded(&ran);
                    ran_pct.fetch_add(1, Ordering::Relaxed);
                    check(&ran, &oracle)
                }
            }
        })
        .unwrap();
    let ran_pct = ran_pct.into_inner();
    assert!(ran_pct >= 16, "only {ran_pct} cases reached pct");
}

#[test]
fn adv_guest_config_replace_keeps_target_macs() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let lab = rt.block_on(Lab::new());
    lab.allow_replace();
    let target_net1 = format!("net1: name=eth1,bridge=vmbr0,hwaddr={OTHER_MAC}\n");
    let ran_pct = AtomicUsize::new(0);
    runner()
        .run(&planted_config(), |config| {
            lab.target(Some(UID_X), UUID, &target_net1);
            let target =
                fs::read_to_string(lab.pve.join(format!("nodes/{NODE}/lxc/200.conf"))).unwrap();
            let snap = rt.block_on(lab.snapshot(&config, Some(FIREWALL), |_| {}, &[]));
            let Some(oracle) = filter(&config, Some(&target)) else {
                let ran = rt.block_on(lab.run(&snap, lab.spec(&snap, replace(UID_X, None))));
                return refused_before_pct(&ran);
            };
            let snapshot_uid = guest_identity(oracle.text.as_bytes()).map(|i| i.uid);
            let confirm = (!same_guest(snapshot_uid.as_deref(), Some(UID_X)))
                .then(|| (snapshot_uid.clone().unwrap_or_default(), UID_X.to_owned()));
            let guest = replace(
                UID_X,
                confirm.as_ref().map(|(s, t)| (s.as_str(), t.as_str())),
            );
            let ran = rt.block_on(lab.run(&snap, lab.spec(&snap, guest)));
            succeeded(&ran);
            ran_pct.fetch_add(1, Ordering::Relaxed);
            check(&ran, &oracle)?;
            let from_target = BTreeMap::from([
                ("net0", (Some(TARGET_MAC), Some("1"))),
                ("net1", (Some(OTHER_MAC), None)),
            ]);
            for (key, value) in config_lines(ran.config.as_deref().unwrap_or_default()) {
                if !key.starts_with("net") {
                    continue;
                }
                let props = parse_property(value, None).unwrap_or_default();
                let (mac, firewall) = from_target.get(key).copied().unwrap_or((None, None));
                prop_assert_eq!(props.get("hwaddr").map(String::as_str), mac, "{}", value);
                prop_assert_eq!(
                    props.get("firewall").map(String::as_str),
                    firewall,
                    "{}",
                    value
                );
            }
            prop_assert!(ran.guest().replaced);
            Ok(())
        })
        .unwrap();
    let ran_pct = ran_pct.into_inner();
    assert!(ran_pct >= 16, "only {ran_pct} cases reached pct");
}
