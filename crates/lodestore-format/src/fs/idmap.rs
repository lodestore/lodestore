//! Translating owners between the node's and a container's view (ADR 0008,
//! "CT walk"): `st_uid`/`st_gid`, the ids in POSIX ACL xattrs and the root id
//! of file capabilities, the way the kernel translates them for a reader in
//! the container's user namespace. An id outside the map becomes
//! [`OVERFLOW_ID`] of the other view and is counted.

use crate::ct::{IdKind, IdMap};
use crate::error::{Error, Result};
use crate::pb;

/// What the kernel shows for an id the namespace does not map.
pub const OVERFLOW_ID: u32 = 65_534;

pub const ACL_ACCESS: &[u8] = b"system.posix_acl_access";
pub const ACL_DEFAULT: &[u8] = b"system.posix_acl_default";
pub const CAPABILITY: &[u8] = b"security.capability";

const ACL_VERSION: u32 = 2;
const ACL_HEADER_LEN: usize = 4;
const ACL_ENTRY_LEN: usize = 8;
const ACL_USER: u16 = 0x02;
const ACL_GROUP: u16 = 0x08;

const CAP_REVISION_MASK: u32 = 0xFF00_0000;
const CAP_REVISION_2: u32 = 0x0200_0000;
const CAP_REVISION_3: u32 = 0x0300_0000;
const CAP_V2_LEN: usize = 20;
const CAP_V3_LEN: usize = 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// A backup: node ids into the container's view.
    HostToCt,
    /// A restore onto the node: the container's ids into node ids.
    CtToHost,
}

/// One id in the other view; outside the map it is [`OVERFLOW_ID`] and
/// `unmapped` grows by one. Onto the node that is the node id of the
/// container's [`OVERFLOW_ID`], what the container shows for the id; only
/// when the map has none either is it the node's own [`OVERFLOW_ID`].
pub fn translate_id(map: &IdMap, kind: IdKind, dir: Direction, id: u32, unmapped: &mut u64) -> u32 {
    shift_id(map, kind, dir, id, unmapped).unwrap_or(OVERFLOW_ID)
}

/// [`translate_id`], with `None` where the node has no id for the
/// container's view of it.
fn shift_id(map: &IdMap, kind: IdKind, dir: Direction, id: u32, unmapped: &mut u64) -> Option<u32> {
    let mapped = match dir {
        Direction::HostToCt => map.to_ct(kind, id),
        Direction::CtToHost => map.to_host(kind, id),
    };
    if mapped.is_some() {
        return mapped;
    }
    *unmapped += 1;
    match dir {
        Direction::HostToCt => Some(OVERFLOW_ID),
        Direction::CtToHost => map.to_host(kind, OVERFLOW_ID),
    }
}

/// Whether the owner `id` of the container has no node id at all: it falls
/// back to the node's [`OVERFLOW_ID`].
pub(crate) fn owner_lost(map: &IdMap, kind: IdKind, id: u32) -> bool {
    shift_id(map, kind, Direction::CtToHost, id, &mut 0).is_none()
}

fn u16_at(blob: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(blob.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(blob: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(blob.get(at..at + 4)?.try_into().ok()?))
}

fn bad_acl(reason: &str) -> Error {
    Error::corrupt("POSIX ACL", reason)
}

/// A POSIX ACL xattr (version 2: a u32 header, then entries of tag u16,
/// perm u16, id u32, all little-endian) with the ids of its named user and
/// group entries translated. An entry outside the map becomes
/// [`OVERFLOW_ID`] of the other view on its own; onto the node, an entry
/// with no such id is left out. Only a malformed blob fails.
pub fn translate_acl(
    blob: &[u8],
    dir: Direction,
    map: &IdMap,
    unmapped: &mut u64,
) -> Result<Vec<u8>> {
    shift_acl(blob, dir, map, unmapped).map(|(out, _)| out)
}

/// [`translate_acl`] and how many entries it left out.
fn shift_acl(
    blob: &[u8],
    dir: Direction,
    map: &IdMap,
    unmapped: &mut u64,
) -> Result<(Vec<u8>, usize)> {
    if u32_at(blob, 0) != Some(ACL_VERSION) {
        return Err(bad_acl("not a version 2 ACL"));
    }
    let entries = blob.get(ACL_HEADER_LEN..).unwrap_or_default();
    if entries.len() % ACL_ENTRY_LEN != 0 {
        return Err(bad_acl("the length is not a whole number of entries"));
    }
    let mut out = Vec::with_capacity(blob.len());
    out.extend_from_slice(&ACL_VERSION.to_le_bytes());
    let mut left_out = 0;
    for entry in entries.as_chunks::<ACL_ENTRY_LEN>().0 {
        let kind = match u16_at(entry, 0) {
            Some(ACL_USER) => IdKind::User,
            Some(ACL_GROUP) => IdKind::Group,
            _ => {
                out.extend_from_slice(entry);
                continue;
            }
        };
        let id = u32_at(entry, 4).ok_or_else(|| bad_acl("truncated entry"))?;
        let Some(new) = shift_id(map, kind, dir, id, unmapped) else {
            left_out += 1;
            continue;
        };
        out.extend_from_slice(entry.get(..4).ok_or_else(|| bad_acl("truncated entry"))?);
        out.extend_from_slice(&new.to_le_bytes());
    }
    Ok((out, left_out))
}

fn bad_caps(reason: &str) -> Error {
    Error::corrupt("file capabilities", reason)
}

/// A `security.capability` xattr in the other view. Version 2 carries no
/// root id and means the root of the view it is read in; version 3 names it.
/// The result is version 2 when the root id becomes 0, version 3 otherwise.
/// `Ok(None)` means the root id is outside the map: the capabilities are not
/// to be applied, and `unmapped` grows by one.
pub fn translate_caps(
    blob: &[u8],
    dir: Direction,
    map: &IdMap,
    unmapped: &mut u64,
) -> Result<Option<Vec<u8>>> {
    let magic = u32_at(blob, 0).ok_or_else(|| bad_caps("truncated"))?;
    let rootid = match (magic & CAP_REVISION_MASK, blob.len()) {
        (CAP_REVISION_2, CAP_V2_LEN) => 0,
        (CAP_REVISION_3, CAP_V3_LEN) => {
            u32_at(blob, CAP_V2_LEN).ok_or_else(|| bad_caps("truncated"))?
        }
        _ => return Err(bad_caps("not a version 2 or 3 capability set")),
    };
    let mapped = match dir {
        Direction::HostToCt => map.to_ct(IdKind::User, rootid),
        Direction::CtToHost => map.to_host(IdKind::User, rootid),
    };
    let Some(mapped) = mapped else {
        *unmapped += 1;
        return Ok(None);
    };
    let body = blob
        .get(4..CAP_V2_LEN)
        .ok_or_else(|| bad_caps("truncated"))?;
    let flags = magic & !CAP_REVISION_MASK;
    let mut out = Vec::with_capacity(CAP_V3_LEN);
    if mapped == 0 {
        out.extend_from_slice(&(CAP_REVISION_2 | flags).to_le_bytes());
        out.extend_from_slice(body);
    } else {
        out.extend_from_slice(&(CAP_REVISION_3 | flags).to_le_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(&mapped.to_le_bytes());
    }
    Ok(Some(out))
}

/// Translates the owner and the id-carrying xattrs of `node` in place.
/// A malformed ACL or capability xattr is removed and returned with the
/// reason, as is an ACL that lost entries; capabilities whose root id is
/// outside the map are removed and only counted in `unmapped`.
pub fn shift_node(
    node: &mut pb::Node,
    dir: Direction,
    map: &IdMap,
    unmapped: &mut u64,
) -> Vec<(Vec<u8>, Error)> {
    node.uid = translate_id(map, IdKind::User, dir, node.uid, unmapped);
    node.gid = translate_id(map, IdKind::Group, dir, node.gid, unmapped);
    let mut dropped = Vec::new();
    node.xattrs.retain_mut(|x| {
        let translated = if x.name == ACL_ACCESS || x.name == ACL_DEFAULT {
            shift_acl(&x.value, dir, map, unmapped).map(|(value, left_out)| {
                if left_out > 0 {
                    dropped.push((
                        x.name.clone(),
                        Error::InvalidInput(format!(
                            "{left_out} entries name an id the container's id map has no node id for; they were left out"
                        )),
                    ));
                }
                Some(value)
            })
        } else if x.name == CAPABILITY {
            translate_caps(&x.value, dir, map, unmapped)
        } else {
            return true;
        };
        match translated {
            Ok(Some(value)) => {
                x.value = value;
                true
            }
            Ok(None) => false,
            Err(e) => {
                dropped.push((x.name.clone(), e));
                false
            }
        }
    });
    dropped
}
