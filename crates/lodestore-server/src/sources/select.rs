//! Which discovered instances a policy's `spec.source` selects.
//!
//! The form hints, the job gate and the dead-man check all ask here, so they
//! cannot disagree.

use lodestore_policy::source::Source;

use super::{Db, DbType, Discovery, Guest, GuestType, Stored, is_fresh};

/// A guest is selected when any of vmids, pool or tags matches it; tags match
/// when the guest has any of them. The kind of the source must match too.
pub fn selects_guest(source: &Source, guest: &Guest) -> bool {
    let (kind, wanted) = match source {
        Source::ProxmoxVm(g) => (GuestType::Vm, g),
        Source::ProxmoxCt(g) => (GuestType::Ct, g),
        _ => return false,
    };
    guest.kind == kind
        && (wanted.vmids.contains(&guest.vmid)
            || wanted
                .pool
                .as_ref()
                .is_some_and(|pool| guest.pool.as_ref() == Some(pool))
            || wanted.tags.iter().any(|tag| guest.tags.contains(tag)))
}

/// An empty `databases` selects every database of the server; `excludeDatabases` is subtracted.
pub fn selects_db(source: &Source, kind: DbType, db: &Db) -> bool {
    let wanted = match (source, kind) {
        (Source::Postgres(d), DbType::Postgres) | (Source::Mysql(d), DbType::Mysql) => d,
        _ => return false,
    };
    (wanted.databases.is_empty() || wanted.databases.contains(&db.name))
        && !wanted.exclude_databases.contains(&db.name)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    /// No fresh data, or the list is incomplete and nothing was selected.
    Unknown,
    /// A fresh, complete list has no instance of the type.
    NoSource,
    /// A fresh, complete list has the type but the selector picks nothing.
    Nothing,
    /// `complete` is false when the list may hold more matching instances.
    Some { keys: Vec<String>, complete: bool },
}

/// `source` is never `files`; callers check `source::normalized` first.
pub fn select(source: &Source, stored: Option<&Stored>, now_ms: i64) -> Selection {
    let Some(stored) = stored.filter(|s| is_fresh(s.collected_at, now_ms)) else {
        return Selection::Unknown;
    };
    let found: &Discovery = &stored.discovery;
    let mut keys = Vec::new();
    let mut complete = !found.truncated;
    match source {
        Source::Files(_) => return Selection::Unknown,
        Source::ProxmoxVm(_) | Source::ProxmoxCt(_) => {
            let guests = found.proxmox.iter().flat_map(|p| &p.guests);
            keys.extend(
                guests
                    .filter(|g| selects_guest(source, g))
                    .map(|g| g.key.clone()),
            );
        }
        Source::Postgres(_) | Source::Mysql(_) => {
            let kind = if matches!(source, Source::Postgres(_)) {
                DbType::Postgres
            } else {
                DbType::Mysql
            };
            for server in found.servers.iter().filter(|s| s.kind == kind) {
                match &server.databases {
                    Some(databases) => keys.extend(
                        databases
                            .iter()
                            .filter(|d| selects_db(source, kind, d))
                            .map(|d| d.key.clone()),
                    ),
                    None => complete = false,
                }
            }
        }
    }
    if !keys.is_empty() {
        Selection::Some { keys, complete }
    } else if !complete {
        Selection::Unknown
    } else if !found.has(source.type_name()) {
        Selection::NoSource
    } else {
        Selection::Nothing
    }
}

#[cfg(test)]
mod tests {
    use lodestore_policy::source::{DatabaseSource, ProxmoxGuests};

    use super::*;
    use crate::sources::{DbServer, ProxmoxNode, ScopeKind};

    const NOW: i64 = 10_000_000;

    fn guest(kind: GuestType, vmid: u32, pool: Option<&str>, tags: &[&str]) -> Guest {
        Guest {
            kind,
            vmid,
            name: None,
            pool: pool.map(str::to_string),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            key: format!("key-{vmid}"),
            ..Default::default()
        }
    }

    fn vm(vmid: u32, pool: Option<&str>, tags: &[&str]) -> Guest {
        guest(GuestType::Vm, vmid, pool, tags)
    }

    fn vms(vmids: &[u32], pool: Option<&str>, tags: &[&str]) -> Source {
        Source::ProxmoxVm(ProxmoxGuests {
            vmids: vmids.to_vec(),
            pool: pool.map(str::to_string),
            tags: tags.iter().map(|t| t.to_string()).collect(),
        })
    }

    fn cts(vmids: &[u32], pool: Option<&str>) -> Source {
        Source::ProxmoxCt(ProxmoxGuests {
            vmids: vmids.to_vec(),
            pool: pool.map(str::to_string),
            tags: Vec::new(),
        })
    }

    fn db(name: &str) -> Db {
        Db {
            name: name.to_string(),
            oid: None,
            key: format!("db-{name}"),
        }
    }

    fn server(kind: DbType, names: &[&str]) -> DbServer {
        DbServer {
            kind,
            port: 5432,
            version: None,
            unreachable: false,
            databases: Some(names.iter().map(|n| db(n)).collect()),
        }
    }

    fn stored(discovery: Discovery, age_ms: i64) -> Stored {
        Stored {
            discovery,
            collected_at: Some(NOW - age_ms),
        }
    }

    fn node(guests: Vec<Guest>, truncated: bool) -> Discovery {
        Discovery {
            truncated,
            proxmox: Some(ProxmoxNode {
                scope: ScopeKind::Cluster,
                name: "pve".into(),
                guests,
                ..Default::default()
            }),
            servers: Vec::new(),
        }
    }

    fn databases(source: &Source, kind: DbType) -> Vec<String> {
        ["app", "billing", "scratch"]
            .into_iter()
            .filter(|n| selects_db(source, kind, &db(n)))
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn guests_match_any_criterion_and_any_tag() {
        let web = vm(101, Some("prod"), &["web"]);
        let other = vm(102, Some("dev"), &["db"]);
        assert!(selects_guest(&vms(&[101], None, &[]), &web));
        assert!(!selects_guest(&vms(&[101], None, &[]), &other));
        assert!(!selects_guest(
            &vms(&[101], None, &[]),
            &guest(GuestType::Ct, 101, None, &[])
        ));
        assert!(selects_guest(&vms(&[], Some("prod"), &[]), &web));
        assert!(!selects_guest(&vms(&[], Some("prod"), &[]), &other));
        assert!(selects_guest(&vms(&[], None, &["db", "web"]), &web));
        assert!(selects_guest(&vms(&[], None, &["db", "web"]), &other));
        assert!(!selects_guest(&vms(&[], None, &["mail"]), &web));
        let mixed = vms(&[101], Some("dev"), &[]);
        assert!(selects_guest(&mixed, &web));
        assert!(selects_guest(&mixed, &other));
        assert!(!selects_guest(&mixed, &vm(103, Some("qa"), &[])));
    }

    #[test]
    fn kind_must_match() {
        let ct = guest(GuestType::Ct, 200, Some("prod"), &[]);
        assert!(!selects_guest(&vms(&[200], Some("prod"), &[]), &ct));
        assert!(selects_guest(&cts(&[200], None), &ct));
        let pg = Source::Postgres(DatabaseSource::default());
        assert!(!selects_db(&pg, DbType::Mysql, &db("app")));
        assert!(!selects_db(
            &Source::Mysql(Default::default()),
            DbType::Postgres,
            &db("app")
        ));
        assert!(!selects_guest(&pg, &ct));
    }

    #[test]
    fn databases_all_pick_except() {
        let all = Source::Postgres(DatabaseSource::default());
        assert_eq!(
            databases(&all, DbType::Postgres),
            ["app", "billing", "scratch"]
        );
        assert!(databases(&all, DbType::Mysql).is_empty());
        let named = Source::Postgres(DatabaseSource {
            databases: vec!["app".into()],
            exclude_databases: Vec::new(),
        });
        assert_eq!(databases(&named, DbType::Postgres), ["app"]);
        let except = Source::Postgres(DatabaseSource {
            databases: Vec::new(),
            exclude_databases: vec!["scratch".into()],
        });
        assert_eq!(databases(&except, DbType::Postgres), ["app", "billing"]);
    }

    #[test]
    fn stale_or_missing_is_unknown() {
        let source = vms(&[101], None, &[]);
        assert_eq!(select(&source, None, NOW), Selection::Unknown);
        let discovery = node(vec![vm(101, None, &[])], false);
        let old = stored(discovery.clone(), 16 * 60_000);
        assert_eq!(select(&source, Some(&old), NOW), Selection::Unknown);
        let fresh = stored(discovery, 60_000);
        assert!(matches!(
            select(&source, Some(&fresh), NOW),
            Selection::Some { complete: true, .. }
        ));
        let never = Stored {
            discovery: Discovery::default(),
            collected_at: None,
        };
        assert_eq!(select(&source, Some(&never), NOW), Selection::Unknown);
    }

    #[test]
    fn incomplete_nothing_is_unknown() {
        let only_vms = vec![vm(101, None, &[])];
        let truncated = stored(node(only_vms.clone(), true), 0);
        assert_eq!(
            select(&vms(&[999], None, &[]), Some(&truncated), NOW),
            Selection::Unknown
        );
        assert_eq!(
            select(&cts(&[200], None), Some(&truncated), NOW),
            Selection::Unknown
        );
        let complete = stored(node(only_vms, false), 0);
        assert_eq!(
            select(&vms(&[999], None, &[]), Some(&complete), NOW),
            Selection::Nothing
        );
        let mut unreachable = server(DbType::Postgres, &[]);
        unreachable.unreachable = true;
        unreachable.databases = None;
        let down = stored(
            Discovery {
                servers: vec![unreachable],
                ..Default::default()
            },
            0,
        );
        assert_eq!(
            select(&Source::Postgres(Default::default()), Some(&down), NOW),
            Selection::Unknown
        );
        let truncated_hit = stored(node(vec![vm(101, None, &[])], true), 0);
        assert_eq!(
            select(&vms(&[101], None, &[]), Some(&truncated_hit), NOW),
            Selection::Some {
                keys: vec!["key-101".into()],
                complete: false
            }
        );
    }

    #[test]
    fn no_source_and_nothing() {
        let none = stored(Discovery::default(), 0);
        assert_eq!(
            select(&cts(&[1], None), Some(&none), NOW),
            Selection::NoSource
        );
        let vm_only = stored(node(vec![vm(101, Some("prod"), &[])], false), 0);
        assert_eq!(
            select(&cts(&[1], None), Some(&vm_only), NOW),
            Selection::NoSource
        );
        assert_eq!(
            select(&cts(&[], Some("prod")), Some(&vm_only), NOW),
            Selection::NoSource
        );
        let with_ct = stored(
            node(
                vec![vm(101, None, &[]), guest(GuestType::Ct, 200, None, &[])],
                false,
            ),
            0,
        );
        assert_eq!(
            select(&cts(&[1], None), Some(&with_ct), NOW),
            Selection::Nothing
        );
        let two = stored(node(vec![vm(101, None, &[]), vm(102, None, &[])], false), 0);
        assert_eq!(
            select(&vms(&[101, 102], None, &[]), Some(&two), NOW),
            Selection::Some {
                keys: vec!["key-101".into(), "key-102".into()],
                complete: true
            }
        );
        let pg = stored(
            Discovery {
                servers: vec![server(DbType::Postgres, &["app"])],
                ..Default::default()
            },
            0,
        );
        assert_eq!(
            select(&Source::Mysql(Default::default()), Some(&pg), NOW),
            Selection::NoSource
        );
        assert_eq!(
            select(
                &Source::Postgres(DatabaseSource {
                    databases: vec!["x".into()],
                    exclude_databases: Vec::new()
                }),
                Some(&pg),
                NOW
            ),
            Selection::Nothing
        );
    }
}
