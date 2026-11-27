#!/usr/bin/perl
# SPDX-License-Identifier: Apache-2.0
#
# Calls the Lodestore provider in the order `pct restore` does for a container,
# so the provider and the agent can be tested off a Proxmox VE node:
#
#   fake-pct.pl restore {vmid} {volid} [--unique 1] [--force 1] [--start 0]
#       [--unprivileged 1] [--onboot 0] [--storage S] [--allow-identity-mismatch 1]
#
# The scenario is a JSON file named by $ENV{LODESTORE_FAKE_PCT} with the keys
# out_config (where the configuration PVE would write goes), out_argv (the
# arguments as received), copy_to (where the archive directory is copied) and
# out_fw (where a firewall configuration would go; nothing is written when the
# provider returns none). Run it with `perl -t` so the archive directory is
# checked for taint as `pct` does.

use strict;
use warnings;

use JSON::PP;
use Scalar::Util ();

use PVE::BackupProvider::Plugin::Lodestore;

my @args = @ARGV;
my $command = shift @args;
die "usage: fake-pct.pl restore vmid volid [options]\n" if ($command // '') ne 'restore';
my $vmid = shift @args;
my $volid = shift @args;
die "usage: fake-pct.pl restore vmid volid [options]\n"
    if !defined($vmid) || $vmid !~ /^\d+\z/ || !defined($volid);
while (my $name = shift @args) {
    die "unknown argument '$name'\n"
        if $name !~ /^--(unique|force|start|unprivileged|onboot|storage|allow-identity-mismatch)\z/;
    shift @args // die "missing value for $name\n";
}
my ($storeid, $volname) = $volid =~ /^([a-z][a-z0-9._-]*):(.+)\z/
    or die "'$volid' is not a volume id\n";

my $scenario_file = $ENV{LODESTORE_FAKE_PCT} // die "LODESTORE_FAKE_PCT is not set\n";
open(my $fh, '<', $scenario_file) or die "cannot read $scenario_file: $!\n";
my $scenario = JSON::PP->new->utf8->decode(do { local $/; <$fh> });
close($fh);

sub write_file {
    my ($path, $bytes) = @_;
    ($path) = $path =~ m{^(.*)\z}s;
    open(my $out, '>:raw', $path) or die "cannot write $path: $!\n";
    print {$out} $bytes;
    close($out) or die "cannot write $path: $!\n";
}

write_file($scenario->{out_argv}, JSON::PP->new->utf8->encode(\@ARGV)) if $scenario->{out_argv};

my $provider = PVE::BackupProvider::Plugin::Lodestore->new(
    'PVE::Storage::Custom::LodestorePlugin', {}, $storeid, undef);

my $ok = eval {
    my ($mechanism, $type) = $provider->restore_get_mechanism($volname);
    die "unexpected restore mechanism '$mechanism' for '$type'\n"
        if $mechanism ne 'directory' || $type ne 'lxc';

    my $first = $provider->archive_get_guest_config($volname);
    my $firewall = $provider->archive_get_firewall_config($volname);
    my $init = $provider->restore_container_init($volname, {});
    my $directory = $init->{'archive-directory'};
    # PVE runs pct with taint checks and hands the directory to tar.
    die "Insecure dependency: the archive directory is tainted\n"
        if defined($directory) && ${^TAINT} && Scalar::Util::tainted($directory);
    if ($scenario->{copy_to} && defined($directory)) {
        my ($copy_to) = $scenario->{copy_to} =~ m{^(.*)\z}s;
        local $ENV{PATH} = '/usr/bin:/bin';
        system('cp', '-a', "$directory/.", $copy_to) == 0
            or die "cannot copy $directory\n";
    }
    my $second = $provider->archive_get_guest_config($volname);
    die "backup changed during the restore\n" if $first ne $second;

    write_file($scenario->{out_config}, $second) if $scenario->{out_config};
    write_file($scenario->{out_fw}, $firewall) if defined($firewall) && $scenario->{out_fw};
    $provider->restore_container_cleanup($volname, {});
    1;
};
if (!$ok) {
    print STDERR "ERROR: unable to restore CT $vmid - $@";
    exit 255;
}
exit 0;
