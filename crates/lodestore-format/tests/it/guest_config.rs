use std::collections::BTreeSet;
use std::fs;
use std::io::Write;

use lodestore_format::ct::{self, CtMeta, CtMode, CtSnapshot};
use lodestore_format::guest_config::{
    DropReason, Dropped, FilterInput, Filtered, GuestIdentity, KEEP_KEYS, MountKind, Volume,
    classify_mountpoint, filter_ct, guest_identity, needs_identity_mismatch_flag, parse_property,
    same_guest,
};
use lodestore_format::source::Meta;
use lodestore_format::writer::{BackupOptions, CtBackup};
use proptest::prelude::*;
use proptest::strategy::ValueTree;
use proptest::test_runner::{FileFailurePersistence, TestRunner};

use crate::ct::{init_repo, key, open_dir, reader, writer};

fn filter(config: &str) -> Filtered {
    filter_ct(&FilterInput {
        config: config.as_bytes(),
        firewall: None,
        target: None,
        bridges: None,
    })
    .unwrap()
}

fn reasons<'a>(f: &'a Filtered, key: &str) -> Vec<(&'a str, DropReason)> {
    f.dropped
        .iter()
        .filter(|d| d.key == key)
        .map(|d| (d.value.as_str(), d.reason))
        .collect()
}

fn keys(text: &str) -> Vec<&str> {
    text.lines()
        .filter(|l| !l.starts_with('#'))
        .map(|l| l.split_once(':').map_or(l, |(k, _)| k))
        .collect()
}

const PLANTED: &str = "\
# web server
arch: amd64
hostname: web
lxc.hook.pre-start: /var/lib/x.sh
mp0: /,mp=/host
hookscript: local:snippets/x.sh
dev0: /dev/sda
unprivileged: 0
features: nesting=1,keyctl=1,mount=nfs
args: -x
hostpci0: 01:00
usb0: host=1-1
env: A=1
rootfs: local-lvm:vm-200-disk-0,size=8G
meta: ctime=1,rtime=2,uid=abcdefghijklmnopqrst
onboot: 1

[snap1]
lxc.hook.mount: /x
hostname: other
";

#[tokio::test]
async fn guest_config_planted_snapshot() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo_dir, root) = (tmp.path().join("repo"), tmp.path().join("ct"));
    fs::create_dir(&repo_dir).unwrap();
    fs::create_dir_all(root.join("etc")).unwrap();
    fs::write(root.join("etc/hostname"), b"web\n").unwrap();
    let store = init_repo(&repo_dir).await;
    let mut w = writer(&store).await;
    let ct = CtBackup {
        key: key(),
        meta: CtMeta {
            mode: CtMode::Stop,
            unprivileged: false,
            idmap: None,
        },
        source_meta: Meta::from([("vmid".to_owned(), "200".to_owned())]),
        config: PLANTED.to_owned(),
        firewall: Some("[OPTIONS]\nenable: 1\n\n[RULES]\nIN ACCEPT\n".to_owned()),
        exclude: None,
    };
    let pending = w
        .backup_container(&open_dir(&root), ct, BackupOptions::default(), &|_| {})
        .await
        .unwrap();
    let id = pending.commit(&mut w).await.unwrap().snapshot_id;
    let snap = CtSnapshot::open(&mut reader(&store).await, &id)
        .await
        .unwrap();

    let f = filter_ct(&FilterInput {
        config: snap.config.as_bytes(),
        firewall: snap.firewall.as_deref().map(str::as_bytes),
        target: None,
        bridges: None,
    })
    .unwrap();
    assert_eq!(
        f.text,
        "# web server\narch: amd64\nhostname: web\nfeatures: nesting=1\n\
         rootfs: local-lvm:8,size=8G\nmeta: ctime=1,rtime=2,uid=abcdefghijklmnopqrst\n\
         unprivileged: 1\nonboot: 0\n"
    );
    for (key, value, reason) in [
        ("lxc.hook.pre-start", "/var/lib/x.sh", DropReason::Lxc),
        ("mp0", "/,mp=/host", DropReason::BindMount),
        ("hookscript", "local:snippets/x.sh", DropReason::Hookscript),
        ("dev0", "/dev/sda", DropReason::Device),
        ("unprivileged", "0", DropReason::Privileged),
        ("features", "keyctl=1,mount=nfs", DropReason::Features),
        ("args", "-x", DropReason::Args),
        ("hostpci0", "01:00", DropReason::Device),
        ("usb0", "host=1-1", DropReason::Device),
        ("env", "A=1", DropReason::Lxc),
        ("[snap1]", "3 lines", DropReason::Section),
        ("pct.fw", "4 lines", DropReason::Firewall),
    ] {
        assert_eq!(reasons(&f, key), [(value, reason)], "{key}");
    }
    assert_eq!(f.dropped.len(), 12, "{:?}", f.dropped);
    assert_eq!(
        f.volumes,
        [Volume {
            key: "rootfs".into(),
            mp: "/".into(),
            size_bytes: 8 << 30,
            storage: "local-lvm".into(),
        }]
    );
    assert_eq!(snap.config, PLANTED);
}

#[test]
fn guest_config_volume_key_anywhere() {
    let lines = [
        (
            "mp0: mp=/host,volume=/,size=1G",
            "mp0",
            DropReason::BindMount,
        ),
        (
            "mp1: volume=/dev/sda1,mp=/d,size=1G",
            "mp1",
            DropReason::Device,
        ),
        ("mp2: mp=/x, volume=/y,size=1G", "mp2", DropReason::Unknown),
        (
            "mp3: local:a,volume=local:b,mp=/z,size=1G",
            "mp3",
            DropReason::Unknown,
        ),
        (
            "mp4: mp=/a,mp=/b,local:x,size=1G",
            "mp4",
            DropReason::Unknown,
        ),
        ("mp5: foo,mp=/e,size=1G", "mp5", DropReason::Unknown),
        ("MP6: local:x,mp=/f,size=1G", "", DropReason::Unknown),
        ("mp7: mp=/g,size=1G,LOCAL:x", "mp7", DropReason::Unknown),
        (
            "mp8: local:x,mp=/a/../b,size=1G",
            "mp8",
            DropReason::Unknown,
        ),
        (
            "mp9: volume=0,/,mp=/h,size=1G",
            "mp9",
            DropReason::BindMount,
        ),
    ];
    for (line, key, reason) in lines {
        let f = filter(&format!("{line}\n"));
        assert_eq!(f.text, "unprivileged: 1\nonboot: 0\n", "{line}");
        assert_eq!(
            f.dropped,
            [Dropped {
                key: key.into(),
                value: line
                    .split_once(": ")
                    .filter(|_| !key.is_empty())
                    .map_or(line, |(_, v)| v)
                    .into(),
                reason,
            }],
            "{line}"
        );
        assert!(f.volumes.is_empty());
    }
}

#[test]
fn guest_config_keeps_volumes() {
    let f = filter(
        "rootfs: local-lvm:vm-200-disk-0,size=8G\n\
         mp0: mp=/data,backup=1,size=32G,volume=local-lvm:vm-200-disk-1\n\
         mp1: local:vm-200-disk-2,mp=/l,mountoptions=noatime;nosuid,ro=yes,size=1536M,idmap=u:0:0:1\n\
         mp2: local:x,mp=/s,size=1\n",
    );
    assert_eq!(
        f.text,
        "rootfs: local-lvm:8,size=8G\n\
         mp0: local-lvm:32,mp=/data,backup=1,size=32G\n\
         mp1: local:2,mp=/l,mountoptions=noatime;nosuid,ro=1,size=1536M\n\
         mp2: local:1,mp=/s,size=1\n\
         unprivileged: 1\nonboot: 0\n"
    );
    assert_eq!(
        f.dropped,
        [Dropped {
            key: "mp1".into(),
            value: "idmap=u:0:0:1".into(),
            reason: DropReason::Unknown,
        }]
    );
    let vol = |key: &str, mp: &str, size_bytes: u64, storage: &str| Volume {
        key: key.into(),
        mp: mp.into(),
        size_bytes,
        storage: storage.into(),
    };
    assert_eq!(
        f.volumes,
        [
            vol("rootfs", "/", 8 << 30, "local-lvm"),
            vol("mp0", "/data", 32 << 30, "local-lvm"),
            vol("mp1", "/l", 1536 << 20, "local"),
            vol("mp2", "/s", 1, "local"),
        ]
    );

    for line in [
        "rootfs: local-lvm:vm-200-disk-0",
        "rootfs: local-lvm:vm-200-disk-0,size=0",
        "mp1: local-lvm:x,mp=/big,size=65T",
        "mp1: local-lvm:x,size=1G",
        "mp1: local-lvm:x,mp=relative,size=1G",
    ] {
        let f = filter(&format!("{line}\n"));
        assert_eq!(f.text, "unprivileged: 1\nonboot: 0\n", "{line}");
        assert_eq!(f.dropped.len(), 1, "{line}");
        assert_eq!(f.dropped[0].reason, DropReason::Unknown, "{line}");
    }
    assert_eq!(
        keys(&filter("mp0: local:x,mp=/a,size=64T\n").text),
        ["mp0", "unprivileged", "onboot"]
    );
}

#[test]
fn guest_config_table() {
    use DropReason::*;
    let rows: &[(&str, Option<&str>, Option<DropReason>)] = &[
        ("arch: amd64", Some("arch: amd64"), None),
        ("ostype: debian", Some("ostype: debian"), None),
        ("cores: 2", Some("cores: 2"), None),
        ("cpulimit: 1.5", Some("cpulimit: 1.5"), None),
        ("cpuunits: 100", Some("cpuunits: 100"), None),
        ("memory: 512", Some("memory: 512"), None),
        ("swap: 512", Some("swap: 512"), None),
        ("hostname:web  ", Some("hostname: web"), None),
        ("description: a%20b", Some("description: a%20b"), None),
        ("# a comment", Some("# a comment"), None),
        (
            "searchdomain: example.org",
            Some("searchdomain: example.org"),
            None,
        ),
        ("nameserver: 1.1.1.1", Some("nameserver: 1.1.1.1"), None),
        ("timezone: host", Some("timezone: host"), None),
        ("tags: a;b", Some("tags: a;b"), None),
        ("console: 1", Some("console: 1"), None),
        ("tty: 2", Some("tty: 2"), None),
        ("cmode: shell", Some("cmode: shell"), None),
        ("startup: order=1,up=5", Some("startup: order=1,up=5"), None),
        ("protection: 1", Some("protection: 1"), None),
        ("debug: 1", Some("debug: 1"), None),
        (
            "entrypoint: /sbin/init",
            Some("entrypoint: /sbin/init"),
            None,
        ),
        ("meta: ctime=1, x", Some("meta: ctime=1, x"), None),
        (
            "rootfs: local:vm-1-disk-0,size=1G,acl=on",
            Some("rootfs: local:1,acl=1,size=1G"),
            None,
        ),
        (
            "mp0: local:x,mp=/a,size=1G,replicate=0,quota=1",
            Some("mp0: local:1,mp=/a,quota=1,replicate=0,size=1G"),
            None,
        ),
        ("mp0: /srv,mp=/a,size=1G", None, Some(BindMount)),
        ("mp0: /dev/sdb,mp=/a", None, Some(Device)),
        (
            "net0: name=eth0,bridge=vmbr0,type=veth",
            Some("net0: name=eth0,bridge=vmbr0,type=veth"),
            None,
        ),
        ("net0: name=eth0,type=macvlan", None, Some(Network)),
        ("net0: bridge=vmbr0", None, Some(Network)),
        ("features: nesting=yes", Some("features: nesting=1"), None),
        ("features: nesting=0", None, None),
        ("features: fuse=1", None, Some(Features)),
        ("features: force_rw_sys=1", None, Some(Features)),
        ("features: mknod=1", None, Some(Features)),
        ("features: oops", None, Some(Features)),
        ("unprivileged: 1", None, None),
        ("unprivileged: 0", None, Some(Privileged)),
        ("unprivileged: maybe", None, Some(Privileged)),
        ("onboot: 1", None, None),
        ("hookscript: local:snippets/a.sh", None, Some(Hookscript)),
        ("lxc.idmap: u 0 100000 65536", None, Some(Lxc)),
        ("lxc.apparmor.profile = unconfined", None, Some(Lxc)),
        ("lxc.environment.runtime: LD_PRELOAD=/x", None, Some(Lxc)),
        ("env: A=1", None, Some(Lxc)),
        ("dev0: /dev/kvm", None, Some(Device)),
        ("args: -x", None, Some(Args)),
        ("hostpci0: 01:00", None, Some(Device)),
        ("usb0: host=1-1", None, Some(Device)),
        ("lock: backup", None, None),
        ("template: 1", None, None),
        ("parent: snap", None, None),
        ("snaptime: 1", None, None),
        ("unused0: local:vm-1-disk-3", None, None),
        ("digest: abc", None, None),
        ("pending: x", None, None),
        ("frobnicate: 1", None, Some(Unknown)),
        ("mp256: local:x,mp=/a,size=1G", None, Some(Unknown)),
        ("mp01: local:x,mp=/a,size=1G", None, Some(Unknown)),
        ("net32: name=eth0", None, Some(Unknown)),
        ("Hostname: web", None, Some(Unknown)),
        ("hostname = web", None, Some(Unknown)),
        ("hostname:", None, Some(Unknown)),
        ("[snap]", None, Some(Section)),
    ];
    for (line, kept, reason) in rows {
        let f = filter(&format!("{line}\n"));
        let mut expected = kept.map(|k| format!("{k}\n")).unwrap_or_default();
        expected.push_str("unprivileged: 1\nonboot: 0\n");
        assert_eq!(f.text, expected, "{line}");
        let got: Vec<_> = f.dropped.iter().map(|d| d.reason).collect();
        assert_eq!(got, reason.iter().copied().collect::<Vec<_>>(), "{line}");
    }

    let f = filter_ct(&FilterInput {
        config: b"arch: amd64\nhostname: \xff\nhostname: web\n",
        firewall: None,
        target: None,
        bridges: None,
    })
    .unwrap();
    assert_eq!(
        f.text,
        "arch: amd64\nhostname: web\nunprivileged: 1\nonboot: 0\n"
    );
    assert_eq!(f.dropped.len(), 1);
    assert_eq!(f.dropped[0].reason, Unknown);

    let f = filter("hostname: a\nhostname: b\n");
    assert_eq!(f.text, "hostname: b\nunprivileged: 1\nonboot: 0\n");
    assert_eq!(reasons(&f, "hostname"), [("a", Unknown)]);

    let long = format!("args: {}", "é".repeat(300));
    let f = filter(&long);
    assert!(f.dropped[0].value.len() <= 256);
    assert!(f.dropped[0].value.chars().all(|c| c == 'é'));
    let big = vec![b'#'; ct::MAX_CT_CONFIG_BYTES + 1];
    let input = FilterInput {
        config: &big,
        firewall: None,
        target: None,
        bridges: None,
    };
    assert!(filter_ct(&input).is_err());
}

#[test]
fn guest_config_replace_macs() {
    let snapshot = "net0: name=eth0,bridge=vmbr0,hwaddr=AA:BB:CC:00:00:01,firewall=0\n\
                    net1: name=eth1,bridge=vmbr0,hwaddr=AA:BB:CC:00:00:02\n";
    let target = "net0: name=eth0,bridge=vmbr1,hwaddr=BC:24:11:00:00:99,firewall=1\n\
                  net2: name=eth2,hwaddr=BC:24:11:00:00:98\n";
    let f = filter_ct(&FilterInput {
        config: snapshot.as_bytes(),
        firewall: None,
        target: Some(target.as_bytes()),
        bridges: None,
    })
    .unwrap();
    assert_eq!(
        f.text,
        "net0: name=eth0,bridge=vmbr0,firewall=1,hwaddr=BC:24:11:00:00:99\n\
         net1: name=eth1,bridge=vmbr0\nunprivileged: 1\nonboot: 0\n"
    );
    assert!(f.dropped.is_empty(), "{:?}", f.dropped);

    let f = filter(snapshot);
    assert_eq!(
        f.text,
        "net0: name=eth0,bridge=vmbr0\nnet1: name=eth1,bridge=vmbr0\nunprivileged: 1\nonboot: 0\n"
    );
    assert!(f.dropped.is_empty(), "{:?}", f.dropped);
}

#[test]
fn guest_config_network() {
    let bridges = BTreeSet::from(["vmbr0".to_owned()]);
    let config = "net0: name=eth0,bridge=vmbr0,tag=40,firewall=1,link_down=1\n\
                  net1: name=eth1,bridge=vmbr9\n";
    let f = filter_ct(&FilterInput {
        config: config.as_bytes(),
        firewall: None,
        target: None,
        bridges: Some(&bridges),
    })
    .unwrap();
    assert_eq!(
        f.text,
        "net0: name=eth0,bridge=vmbr0,tag=40\nunprivileged: 1\nonboot: 0\n"
    );
    assert_eq!(
        f.network,
        [(
            "net0".to_owned(),
            "name=eth0,bridge=vmbr0,tag=40".to_owned()
        )]
    );
    assert_eq!(
        reasons(&f, "net0"),
        [
            ("link_down=1", DropReason::Network),
            (
                "firewall=1 (the firewall flag is not restored)",
                DropReason::Network
            ),
        ]
    );
    let net1 = reasons(&f, "net1");
    assert_eq!(net1.len(), 1);
    assert_eq!(net1[0].1, DropReason::Network);
    assert!(net1[0].0.contains("vmbr9"), "{}", net1[0].0);

    let f = filter(config);
    assert_eq!(f.network.len(), 2);
    assert!(f.text.contains("net1: name=eth1,bridge=vmbr9\n"));
}

#[test]
fn guest_config_identity() {
    let uid = "abcdefghijklmnopqrst";
    assert_eq!(
        guest_identity(b"meta: creation-qemu=x,uid=abcdefghijklmnopqrst,rtime=5\n"),
        Some(GuestIdentity {
            uid: uid.to_owned(),
            rtime: Some(5),
        })
    );
    assert_eq!(
        guest_identity(b"meta: uid=aaaaaaaaaaaaaaaaaaaa\nmeta: uid=abcdefghijklmnopqrst\n")
            .unwrap()
            .uid,
        uid
    );
    assert_eq!(
        guest_identity(b"meta: uid=abcdefghijklmnopqrst\nmeta:  \n"),
        None
    );
    assert_eq!(
        guest_identity(b"meta: uid=abcdefghijklmnopqrst,uid=abcdefghijklmnopqrst\n"),
        None
    );
    assert_eq!(guest_identity(b"meta: uid=abcdefghijklmnopqrs1\n"), None);
    assert_eq!(guest_identity(b"meta: uid=ABCDEFGHIJKLMNOPQRST\n"), None);
    assert_eq!(
        guest_identity(b"arch: amd64\n[snap]\nmeta: uid=abcdefghijklmnopqrst\n"),
        None
    );
    assert_eq!(
        guest_identity(b"meta: uid=abcdefghijklmnopqrst,rtime=5,rtime=6\n"),
        Some(GuestIdentity {
            uid: uid.to_owned(),
            rtime: None,
        })
    );

    // The filter keeps the identity Proxmox VE reads from the snapshot.
    for config in [
        "meta: uid=aaaaaaaaaaaaaaaaaaaa\nmeta: uid=abcdefghijklmnopqrst\n",
        "meta: uid=abcdefghijklmnopqrst\nmeta:  \n",
        "meta:uid=abcdefghijklmnopqrst\nmeta:\n",
    ] {
        assert_eq!(
            guest_identity(filter(config).text.as_bytes()),
            guest_identity(config.as_bytes()),
            "{config}"
        );
    }

    let (a, b) = (Some("abcdefghijklmnopqrst"), Some("aaaaaaaaaaaaaaaaaaaa"));
    assert!(same_guest(a, a));
    assert!(!same_guest(a, b));
    assert!(!same_guest(None, None));
    assert!(!same_guest(a, None));
    assert!(!same_guest(None, a));
    assert!(needs_identity_mismatch_flag(a, b));
    assert!(!needs_identity_mismatch_flag(a, a));
    assert!(!needs_identity_mismatch_flag(a, None));
    assert!(!needs_identity_mismatch_flag(None, b));
    assert!(!needs_identity_mismatch_flag(None, None));
}

const VOLUME_PARTS: &[&str] = &[
    "local-lvm:vm-1-disk-0",
    "local:5",
    "volume=local:x",
    "volume=0",
    "/",
    "/srv/data",
    "/dev/sda",
    "volume=/",
    "volume=/dev/sdb1",
    "Volume=local:y",
    " volume=local:z",
    "volume =local:w",
    "foo",
    "LOCAL:x",
    "mp=/data",
    "mp=/a/../b",
    " mp=/x",
    "mp=relative",
    "size=1G",
    "size=1.5G",
    "size=65T",
    "size=8",
    "size=0",
    "backup=1",
    "ro=yes",
    "acl=maybe",
    "mountoptions=noatime;nosuid",
    "mountoptions=bind",
    "idmap=passthrough",
    "shared=1",
    "",
    " ",
    "=x",
    "k=",
];

const NET_PARTS: &[&str] = &[
    "name=eth0",
    "name=e th",
    "bridge=vmbr0",
    "bridge=vmbr9",
    "hwaddr=AA:BB:CC:00:00:01",
    "firewall=1",
    "firewall=off",
    "tag=40",
    "trunks=1;2",
    "link_down=1",
    "type=veth",
    "type=phys",
    "ip=dhcp",
    "eth0",
];

const LINES: &[&str] = &[
    "arch: amd64",
    "hostname: web",
    "hostname: db\r",
    "cores: 4",
    "# description",
    "description: text",
    "meta: ctime=1,rtime=2,uid=abcdefghijklmnopqrst",
    "meta: uid=aaaaaaaaaaaaaaaaaaaa",
    "meta:  ",
    "hookscript: local:snippets/x.sh",
    "lxc.hook.pre-start: /x",
    "lxc.mount.entry = /dev dev none bind",
    "lxc.idmap: u 0 0 65536",
    "env: LD_PRELOAD=/x",
    "dev0: /dev/sda",
    "args: -x",
    "hostpci0: 01:00",
    "usb0: host=1-1",
    "unprivileged: 0",
    "unprivileged: 1",
    "unprivileged: no",
    "onboot: 1",
    "features: nesting=1",
    "features: nesting=1,keyctl=1,mount=nfs",
    "features: fuse=1",
    "lock: backup",
    "unused0: local:vm-1-disk-9",
    "Hostname: x",
    "mp0 : local:x",
    "hostname:",
    "",
    "   ",
    "[snap]",
    "[pve:pending]",
];

fn mount_line() -> impl Strategy<Value = String> {
    (
        prop::sample::select(&["rootfs", "mp0", "mp1", "mp255", "MP2", " mp3", "mp01"][..]),
        prop::collection::vec((prop::sample::select(VOLUME_PARTS), any::<bool>()), 0..6),
    )
        .prop_map(|(key, parts)| {
            let value: Vec<String> = parts
                .into_iter()
                .map(|(p, space)| if space { format!(" {p}") } else { p.to_owned() })
                .collect();
            format!("{key}: {}", value.join(","))
        })
}

/// A volume, `mp` and `size` in any order, sometimes with a hostile part.
fn likely_mount_line() -> impl Strategy<Value = String> {
    (
        prop::sample::select(&["rootfs", "mp0", "mp1", "mp255"][..]),
        prop::sample::select(&["local-lvm:vm-1-disk-0", "volume=local:x", "local:5"][..]),
        prop::sample::select(&["size=1G", "size=1.5G", "size=8", "size=64T"][..]),
        prop::collection::vec(prop::sample::select(VOLUME_PARTS), 0..2),
    )
        .prop_flat_map(|(key, volume, size, extra)| {
            let mut parts = vec![volume, size];
            if key != "rootfs" {
                parts.push("mp=/data");
            }
            parts.extend(extra);
            (Just(key), Just(parts).prop_shuffle())
        })
        .prop_map(|(key, parts)| format!("{key}: {}", parts.join(",")))
}

fn net_line() -> impl Strategy<Value = String> {
    (
        prop::sample::select(&["net0", "net1", "net31", "net32"][..]),
        prop::collection::vec(prop::sample::select(NET_PARTS), 0..6),
        any::<bool>(),
    )
        .prop_map(|(key, mut parts, named)| {
            if named {
                parts.retain(|p| !p.starts_with("name="));
                parts.insert(0, "name=eth0");
            }
            format!("{key}: {}", parts.join(","))
        })
}

fn config() -> impl Strategy<Value = String> {
    let line = prop_oneof![
        4 => prop::sample::select(LINES).prop_map(str::to_owned),
        3 => mount_line(),
        3 => likely_mount_line(),
        2 => net_line(),
        1 => "[a-z]{1,8}: [ -~]{0,40}",
        1 => "[ -~]{300,400}".prop_map(|s| format!("hostname: {s}")),
    ];
    prop::collection::vec(line, 0..24).prop_map(|lines| lines.join("\n"))
}

fn bridges() -> BTreeSet<String> {
    BTreeSet::from(["vmbr0".to_owned()])
}

fn filter_with_bridges(config: &str, bridges: &BTreeSet<String>) -> Filtered {
    filter_ct(&FilterInput {
        config: config.as_bytes(),
        firewall: None,
        target: None,
        bridges: Some(bridges),
    })
    .unwrap()
}

fn allowed(key: &str) -> bool {
    let indexed = |prefix: &str, max: u32| {
        key.strip_prefix(prefix)
            .and_then(|n| n.parse::<u32>().ok().filter(|i| i.to_string() == n))
            .is_some_and(|i| i < max)
    };
    KEEP_KEYS.contains(&key)
        || ["rootfs", "features", "unprivileged", "onboot"].contains(&key)
        || indexed("mp", 256)
        || indexed("net", 32)
}

proptest! {
    #![proptest_config(ProptestConfig {
        failure_persistence: Some(Box::new(FileFailurePersistence::Direct(
            "tests/proptest-regressions/guest_config.txt",
        ))),
        ..ProptestConfig::default()
    })]

    #[test]
    fn guest_config_props(input in config(), with_bridges in any::<bool>()) {
        let set = bridges();
        let run = |text: &str| {
            if with_bridges { filter_with_bridges(text, &set) } else { filter(text) }
        };
        let out = run(&input);

        // (a) only allowed keys, every line a key or a comment
        for line in out.text.lines() {
            if line.starts_with('#') {
                continue;
            }
            let parsed = ct::config_lines(line).next();
            prop_assert!(parsed.is_some(), "{:?}", line);
            let (key, _) = parsed.unwrap();
            prop_assert!(allowed(key), "{} in {:?}", key, out.text);
        }
        // (b) deterministic
        prop_assert_eq!(&run(&input), &out);
        // (c) idempotent
        let again = run(&out.text);
        prop_assert_eq!(&again.text, &out.text);
        prop_assert!(again.dropped.is_empty(), "{:?}", again.dropped);
        // (d) the meta line and the identity stay
        let main = input.split('\n').take_while(|l| !l.starts_with('['));
        let last_meta = main.filter(|l| l.starts_with("meta:") && l.len() > 5).last();
        let kept: Vec<_> = out.text.split('\n').filter(|l| l.starts_with("meta:")).collect();
        match last_meta {
            Some(l) if ct::config_lines(l).next().is_some() => prop_assert_eq!(kept, [l]),
            _ => prop_assert!(kept.is_empty()),
        }
        prop_assert_eq!(guest_identity(out.text.as_bytes()), guest_identity(input.as_bytes()));
        // (e) unprivileged and onboot forced, once
        let count = |p: &str| out.text.lines().filter(|l| l.starts_with(p)).count();
        prop_assert_eq!(count("unprivileged"), 1);
        prop_assert_eq!(count("onboot"), 1);
        prop_assert_eq!(count("unprivileged: 1"), 1);
        prop_assert_eq!(count("onboot: 0"), 1);
        // (f) every mount point is an allocation on a storage
        let mut mounts = 0;
        for (key, value) in ct::config_lines(&out.text) {
            if key != "rootfs" && !key.starts_with("mp") {
                continue;
            }
            mounts += 1;
            let defaults = value.split(',').filter(|p| !p.contains('=')).count();
            prop_assert_eq!(defaults, 1, "{}", value);
            let props = parse_property(value, Some("volume"));
            prop_assert!(props.is_some(), "{}", value);
            let props = props.unwrap();
            let volume = &props["volume"];
            prop_assert_eq!(classify_mountpoint(volume), MountKind::Volume);
            let (storage, size) = volume.split_once(':').unwrap_or_default();
            prop_assert!(storage.starts_with(|c: char| c.is_ascii_lowercase()), "{}", volume);
            prop_assert!(
                storage.bytes().all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-')),
                "{}", volume
            );
            prop_assert!(!size.is_empty() && size.bytes().all(|b| b.is_ascii_digit()), "{}", volume);
        }
        prop_assert_eq!(out.volumes.len(), mounts);
    }
}

/// Writes the corpus of the differential test against
/// `PVE::LXC::Config::parse_pct_config` on a nested Proxmox VE: one JSON
/// object per line, `{"input": "<pct.conf>", "output": "<filtered pct.conf>",
/// "bridges": ["vmbr0"]}`, the output filtered with those bridges. 512 cases
/// of the property generator (fixed seed), then the lines of
/// `guest_config_volume_key_anywhere`. Runs only with
/// `LODE_GUEST_CONFIG_CORPUS=<path>`.
#[test]
#[ignore]
fn guest_config_corpus_dump() {
    let Some(path) = std::env::var_os("LODE_GUEST_CONFIG_CORPUS") else {
        return;
    };
    let mut inputs: Vec<String> = Vec::new();
    let mut runner = TestRunner::deterministic();
    let strategy = config();
    for _ in 0..512 {
        inputs.push(strategy.new_tree(&mut runner).unwrap().current());
    }
    inputs.extend(
        [
            "mp0: mp=/host,volume=/,size=1G",
            "mp1: volume=/dev/sda1,mp=/d,size=1G",
            "mp2: mp=/x, volume=/y,size=1G",
            "mp3: local:a,volume=local:b,mp=/z,size=1G",
            "mp4: mp=/a,mp=/b,local:x,size=1G",
            "mp5: foo,mp=/e,size=1G",
            "MP6: local:x,mp=/f,size=1G",
            "mp7: mp=/g,size=1G,LOCAL:x",
            "mp8: local:x,mp=/a/../b,size=1G",
            "mp9: volume=0,/,mp=/h,size=1G",
        ]
        .map(|l| format!("{l}\n")),
    );
    let set = bridges();
    let mut file = std::io::BufWriter::new(fs::File::create(path).unwrap());
    for input in inputs {
        let output = filter_with_bridges(&input, &set).text;
        let line = serde_json::json!({"input": input, "output": output, "bridges": ["vmbr0"]});
        writeln!(file, "{line}").unwrap();
    }
    file.flush().unwrap();
}
