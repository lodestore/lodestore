# SPDX-License-Identifier: Apache-2.0
package PVE::Storage::Custom::LodestorePlugin;

# The storage plugin that lets vzdump and `pct restore` reach the Lodestore
# backup provider. It stores nothing itself and offers backups only. Deleting
# a backup is the Lodestore server's job, so every way to delete one through
# Proxmox VE is refused here.

use strict;
use warnings;

use IO::Socket::UNIX;
use PVE::Storage;
use PVE::Storage::Plugin;
use PVE::BackupProvider::Plugin::Lodestore;

use base qw(PVE::Storage::Plugin);

use constant STAGING => '/var/lib/lodestore/pve-staging';

my $NOT_SUPPORTED = "not supported by Lodestore storage\n";
my $NO_DELETE = "deletion is managed by the Lodestore server\n";

# 11 is the first storage API with backup providers; 17 is the newest one the
# provider was tested on.
sub api {
    my ($min, $max) = (11, 17);
    my $version = PVE::Storage::APIVER;
    return $version if $version >= $min && $version <= $max;
    return $max;
}

sub type {
    return 'lodestore';
}

sub plugindata {
    return {
        content => [{ backup => 1, none => 1 }, { backup => 1 }],
        features => { 'backup-provider' => 1 },
        'sensitive-properties' => {},
    };
}

sub properties {
    return {};
}

sub options {
    return {
        content => { optional => 1 },
        nodes => { optional => 1 },
        disable => { optional => 1 },
        'prune-backups' => { optional => 1 },
    };
}

sub parse_volname {
    my ($class, $volname) = @_;
    if ($volname =~ m!^backup/(ct/(\d+)/(\d+))\z!) {
        return ('backup', $1, $2 + 0);
    }
    if ($volname =~ m!^backup/(restore/[0-9a-f]{32})\z!) {
        return ('backup', $1, undef);
    }
    die "unable to parse Lodestore volume name '$volname'\n";
}

sub path {
    my ($class, $scfg, $volname, $storeid, $snapname) = @_;
    my ($type, $name, $vmid) = $class->parse_volname($volname);
    $name =~ s!^restore/!!;
    return (STAGING . "/lodestore-restore-$name/data", $vmid, $type);
}

# A socket file left behind by an agent that stopped does not count. A caller
# outside the socket's group (pveproxy runs as www-data) is refused with EACCES
# by a listening agent, which still counts as running.
sub agent_running {
    my $sock = IO::Socket::UNIX->new(
        Peer => PVE::BackupProvider::Plugin::Lodestore::socket_path(),
        Timeout => 2,
    ) or return ($!{EACCES} || $!{EPERM}) ? 1 : 0;
    close($sock);
    return 1;
}

sub status {
    my ($class, $storeid, $scfg, $cache) = @_;
    return (1 << 40, 1 << 40, 0, agent_running() ? 1 : 0);
}

sub activate_storage {
    my ($class, $storeid, $scfg, $cache) = @_;
    die "the Lodestore agent is not running on this node\n" if !agent_running();
    return 1;
}

sub deactivate_storage {
    return 1;
}

sub activate_volume {
    return 1;
}

sub deactivate_volume {
    return 1;
}

sub list_volumes {
    return [];
}

sub list_images {
    return [];
}

sub get_volume_attribute {
    return;
}

sub volume_size_info {
    return wantarray ? (0, 'directory', 0, undef, time()) : 0;
}

sub volume_has_feature {
    return 0;
}

sub free_image {
    die $NO_DELETE;
}

sub prune_backups {
    die $NO_DELETE;
}

sub alloc_image {
    die $NOT_SUPPORTED;
}

sub clone_image {
    die $NOT_SUPPORTED;
}

sub create_base {
    die $NOT_SUPPORTED;
}

sub volume_snapshot {
    die $NOT_SUPPORTED;
}

sub volume_snapshot_rollback {
    die $NOT_SUPPORTED;
}

sub volume_snapshot_delete {
    die $NOT_SUPPORTED;
}

sub volume_resize {
    die $NOT_SUPPORTED;
}

sub volume_import {
    die $NOT_SUPPORTED;
}

sub volume_export {
    die $NOT_SUPPORTED;
}

sub update_volume_attribute {
    die $NOT_SUPPORTED;
}

sub new_backup_provider {
    my ($class, $scfg, $storeid, $log_function) = @_;
    return PVE::BackupProvider::Plugin::Lodestore->new($class, $scfg, $storeid, $log_function);
}

1;
