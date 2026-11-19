---
title: Back up a disk image
description: Back up a raw disk image file or a block device with lode, and restore it to a file or to a block device.
---

`lode backup --image` reads a disk image whole and stores it as a snapshot of its own. The restore needs only the bucket and the repository password or recovery key; no server is involved.

## What it backs up

- A raw image file, or a block device such as a logical volume, read from the first byte to the last.
- Fixed chunks of 4 MiB. A chunk of zeros is stored as a hole and takes no space in the repository, and a chunk seen before is stored once.
- The image is split into segments of 256 GiB in the snapshot. The largest image is 16 TiB (64 segments).

Back up a quiet disk. A device that a running virtual machine or database writes to is read while it changes, and the copy is not consistent. Take a snapshot of the volume first (for example an LVM snapshot) or stop the machine, and back up that. An image of a disk in use is crash-consistent at best, like a disk after a power cut. `lode` warns when the device is mounted or held by another device, and checks the source again after the last read: when a file changed its size, modification time or change time, or a device changed its size, the backup fails and writes no snapshot. A write that leaves the size of a device as it was is not seen.

## Back up

```sh
lode backup --image /dev/vg/snap --name db-disk
lode backup --image disk.raw
```

The second command names the image `disk.raw`, the file name of the source. The command prints `snapshot ID saved` and a line with the key of the image (`image/db-disk`), its size and how it was read. With `--json` the `backup_summary` carries `image.key`, `image.size` and `image.read_mode` (`full`, or `incremental` for a backup by a dirty bitmap, below).

The name is what identifies the disk. It is 1 to 200 bytes of UTF-8, with no `/`, no control characters and not `.` or `..`; once percent-encoded it is at most 506 bytes (each byte outside `A-Z a-z 0-9 - . _ ~` counts as three), so a name of non-ASCII characters is shorter: 84 characters like `ü` at most. A wrong name, or `--image` together with paths, `--stdin`, `--parent` or `--exclude`, exits with code 2.

Use one name for one disk. Two different disks backed up under the same name are one instance: they share retention and `latest@image/NAME`. The name, not the device path, is the identity, because `/dev/sdX` changes between boots and a disk can have several paths.

## Incremental backups from a dirty bitmap

A disk that qemu exports over NBD can keep a dirty bitmap, a record of which parts of the disk changed. `lode backup --image nbd+unix://... --bitmap NAME` reads only those parts and takes the rest from the previous snapshot. The result is an ordinary snapshot of the image that restores like any other, and it is never trusted blindly: when anything about the bitmap, the previous snapshot or the disk is in doubt, `lode` reads the whole image and says why.

The commands below are the ones the test `qemu_incremental_matches_the_disk_at_every_granularity` runs, on a qcow2 file that no virtual machine uses. It was run with qemu 10.1 and 10.2; older versions have not been checked. Managing the bitmaps of a disk that a running machine uses, and managers that reset a bitmap when a client connects (the Proxmox provider does), are not covered and not supported.

### The rotation

`lode` supports one way of managing bitmaps, a rotation with a generation number `N`. The bitmaps are named `lode-{repo16}-{inst16}-g{N}`; `lode` prints the names to use, you do not make them up.

1. The export offers two bitmaps, one `-B` option of `qemu-nbd` each: `g{N}`, which has collected the changes since the previous snapshot, and `g{N+1}`, created before the read, which collects the changes from now on. `lode backup --bitmap lode-...-g{N}` reads `g{N}` and binds the new snapshot to `g{N+1}`.
2. When the snapshot is saved, delete `g{N}`. If the backup failed, delete nothing and repeat it with the same `N`.
3. The next backup uses `g{N+1}` and a new `g{N+2}`.

The rules that make this safe:

- A bitmap name is never reused and the generation only grows. A bitmap that you recreate gets the next generation, not its old name.
- `g{N}` is deleted only after a successful backup.
- `g{N+1}` must exist before the read starts and be exported with `g{N}`. If it was not exported, the run is still incremental, but it records no binding and prints a note, and the next backup reads the image in full.
- Use one socket path for one disk. The path is part of the identity of the source, and another path is a changed source (`source_changed`).
- After a resize, or when a bitmap was lost, remove all `lode-...` bitmaps of the disk and continue with the next generation: `lode` prints it. The test does exactly this, and it does not depend on what qemu does with a bitmap on resize. The next backup is a full read (`gen_mismatch`) and the one after it is incremental again.

`lode` cannot check more than that `g{N}` and `g{N+1}` exist. It cannot see a bitmap that was recreated under the same name or reset by something else, and an increment made from such a bitmap misses writes. Keep to the rules.

### The commands

The first backup has no bitmap and reads the image in full. Give the name with `--name`; the export has none. Wait for the socket to appear before `lode` connects; the commands below leave that out.

```sh
SOCK=/run/db-disk.nbd
qemu-nbd -r -f qcow2 -k "$SOCK" -t disk.qcow2 &
NBD=$!
lode backup --image "nbd+unix:///?socket=$SOCK" --name db-disk --json
kill "$NBD"
```

The summary names the next bitmaps: `image.next_bitmap` (`CUR`, `g1`) is the one to pass next time, and `image.bitmap_to_create` (`NEXT`, `g2`) is the one to create before the next backup. The human output says the same: `next backup: create NEXT before it starts, then pass --bitmap CUR`. Create `CUR` at once, so that it starts collecting changes:

```sh
qemu-img bitmap --add --granularity 65536 disk.qcow2 "$CUR"
```

Before each later backup, create `NEXT`, export both bitmaps, back up and stop the export. Then delete `CUR` and take the two names from the summary again:

```sh
qemu-img bitmap --add --granularity 65536 disk.qcow2 "$NEXT"
qemu-nbd -r -f qcow2 -B "$CUR" -B "$NEXT" -k "$SOCK" -t disk.qcow2 &
NBD=$!
lode backup --image "nbd+unix:///?socket=$SOCK" --name db-disk --bitmap "$CUR" --json
kill "$NBD"
qemu-img bitmap --remove disk.qcow2 "$CUR"
```

The first backup with a bitmap is a full read (`no_state`), because the first backup bound no bitmap; it binds `NEXT`, and from the next backup on the reads are incremental. The granularity is the size of the blocks the bitmap tracks. The test ran 512 B, 64 KiB, 4 MiB and 16 MiB, and the result was the same as a full read each time.

A backup that fails writes no snapshot. Delete nothing and repeat it with the same `CUR`; a script should delete `CUR` only when `lode` exited with 0.

### What the output says

With `--json` the `image` object carries `read_mode` (`full` or `incremental`), and for an NBD export `full_reason` after a full read, `bitmap` (the bitmap that was read), `next_bitmap` and `bitmap_to_create`; an increment also has `parent`, the snapshot it was made against, at the top of the summary. Without `--json` the line about the image reads `incremental read from ID` or `full read (REASON)`.

When two disks on one host are exported under the same name, `lode` takes them for one instance: without `--name` the name of the export is the name of the image. If its own state shows that the export changed, `lode` stops and asks for `--name`. Without state (the first backup on a machine, a lost cache) it cannot see this, so give every disk its own `--name`.

### Why a backup read the whole image

| `full_reason` | Meaning |
|---------------|---------|
| `no_bitmap` | No `--bitmap` was given. |
| `bitmap_missing` | The export does not offer the bitmap named by `--bitmap`. |
| `no_state` | This machine has no record of the previous backup of this image: a first backup with a bitmap, a lost or cleaned cache, or a previous run that recorded no binding. |
| `state_unreadable` | The record on this machine is damaged or unreadable. |
| `source_changed` | The export is not the one the previous backup read (another socket path or address), and `--name` was given. |
| `no_parent` | The repository holds no snapshot of this image from this host. |
| `parent_not_newest` | The newest snapshot of this image is not the one this machine recorded: another machine, or another run, wrote a newer one. |
| `parent_unreadable` | The previous snapshot has no bitmap binding or no size, or its contents cannot be read. |
| `name_changed` | The bitmap recorded in the previous snapshot is not named for this repository, host and image. |
| `gen_mismatch` | The generation of `--bitmap` is not the one the previous snapshot recorded, or the chain of generations has forked. This is also the answer after a resize that skipped a generation. |
| `size_changed` | The disk has another size than at the previous backup. |

A full read is not a failure. It is deduplicated, so it reads the whole disk and adds to the repository only what changed. When the cache is lost, the next backup is a full read of an unchanged disk and stores nothing new. After a refused previous snapshot `lode` reuses nothing from it.

The restore of an increment is the same `lode restore` as for any snapshot of an image; it needs only the bucket and the password or recovery key.

## Restore to a file

```sh
lode restore ID --target /restore
```

This writes one sparse file, `/restore/db-disk.img`. Holes stay holes where the file system supports them. `ID` is a snapshot id or any unique start of it; run `lode snapshots --source-type image` to list them.

To restore the segments as ordinary files, name their folder. They land directly in the target as `000000`, `000001` and so on:

```sh
lode restore ID:/disks/db-disk --target /restore
```

`latest@image/db-disk` names the newest signed snapshot of the image. It needs signatures: a repository that a server has opened, or one that `lode backup` has written to with the password. In a repository without them, name the snapshot by its id.

## Restore to a block device

```sh
lode restore ID --to-device /dev/sdX
```

This works on Linux only and needs permission to open the device for writing, normally root. Holes are written as zeros. If the device is larger than the image, the bytes after the end of the image are not changed. Before the first byte is written `lode` refuses a device that:

- is not a block device;
- is smaller than the image;
- is mounted, or has a partition that is mounted;
- is used as swap, or is held by another device (for example a device-mapper volume built on it).

In a repository with signatures, a snapshot that nothing signed is refused unless you pass `--accept-unverified`. The same applies when the signatures cannot be read. In a repository with no signature at all, nothing is unverified.

The check has a limit. `lode` opens the device exclusively, which a mounted file system or another exclusive user makes fail. A process that has the device open without an exclusive lock, such as a virtual machine that uses it as a disk, looks the same as a free device. Stop such machines and processes before the restore.

After the write, back up the device again and compare it with the snapshot you restored from:

```sh
lode backup --image /dev/sdX --name db-disk
lode snapshots --json
```

`lode restore --verify` (to a file) and `lode check --level restore` also verify the files of an image, by slices of 4 MiB.

## Older versions of lode

A `lode` of 0.2 or an earlier build reads the snapshot of an image only by its id; the record of the image is unreadable to it. It restores the segments as ordinary files, and joining them in the order of their names gives the image:

```sh
lode restore ID:/disks/db-disk --target /restore
cat /restore/* > db-disk.img
```
