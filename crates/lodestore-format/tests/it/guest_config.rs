use std::fs;

use lodestore_format::ct::{CtMeta, CtMode, CtSnapshot};
use lodestore_format::guest_config::{
    DropReason, Dropped, FilterInput, Filtered, Volume, filter_ct,
};
use lodestore_format::source::Meta;
use lodestore_format::writer::{BackupOptions, CtBackup};

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
