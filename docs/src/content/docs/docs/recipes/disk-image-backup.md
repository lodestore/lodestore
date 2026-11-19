---
title: Back up a disk image
description: Back up a raw disk image file or a block device with lode, and restore it to a file or to a block device.
---

`lode backup --image` reads a disk image whole and stores it as a snapshot of its own. The restore needs only the bucket and the repository password or recovery key; no server is involved.

## What it backs up

- A raw image file, or a block device such as a logical volume, read from the first byte to the last.
- Fixed chunks of 4 MiB. A chunk of zeros is stored as a hole and takes no space in the repository, and a chunk seen before is stored once.
- The image is split into segments of 256 GiB in the snapshot. The largest image is 16 TiB (64 segments).

Back up a quiet disk. A device that a running virtual machine or database writes to is read while it changes, and the copy is not consistent. Take a snapshot of the volume first (for example an LVM snapshot) or stop the machine, and back up that.

## Back up

```sh
lode backup --image /dev/vg/snap --name db-disk
lode backup --image disk.raw
```

The second command names the image `disk.raw`, the file name of the source. The command prints `snapshot ID saved` and a line with the key of the image (`image/db-disk`), its size and how it was read. With `--json` the `backup_summary` carries `image.key`, `image.size` and `image.read_mode` (`full`).

The name is what identifies the disk. It is 1 to 200 bytes of UTF-8, with no `/`, no control characters and not `.` or `..`; a name made mostly of non-ASCII characters can exceed the 512-byte limit of the key once it is percent-encoded, and is refused before anything is written. A wrong name, or `--image` together with paths, `--stdin`, `--parent` or `--exclude`, exits with code 2.

Use one name for one disk. Two different disks backed up under the same name are one instance: they share retention and `latest@image/NAME`. The name, not the device path, is the identity, because `/dev/sdX` changes between boots and a disk can have several paths.

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
