#!/usr/bin/perl
# SPDX-License-Identifier: Apache-2.0
#
# Calls the Lodestore provider in the order vzdump does for one container, so
# the provider and the agent can be tested off a Proxmox VE node:
#
#   fake-vzdump.pl {vmid} --storage S --mode M --remove 0 [--protected 0]
#
# The scenario is a JSON file named by $ENV{LODESTORE_FAKE_VZDUMP} with the keys
# dir, config, excludes, mode, sources, guest_uid, fail_after_data, pause (seconds
# to wait after the open) and term_file (created when SIGTERM arrives).

use strict;
use warnings;

use JSON::PP;
use POSIX ();

use PVE::BackupProvider::Plugin::Lodestore;

$| = 1;

my @args = @ARGV;
my $vmid = shift @args;
die "usage: fake-vzdump.pl vmid [options]\n" if !defined($vmid) || $vmid !~ /^(\d+)\z/;
$vmid = $1;
my %opt;
while (my $name = shift @args) {
    die "unknown argument '$name'\n" if $name !~ /^--(storage|mode|remove|protected)\z/;
    $opt{$1} = shift @args;
}

my $scenario_file = $ENV{LODESTORE_FAKE_VZDUMP} // die "LODESTORE_FAKE_VZDUMP is not set\n";
open(my $fh, '<', $scenario_file) or die "cannot read $scenario_file: $!\n";
my $scenario = JSON::PP->new->utf8->decode(do { local $/; <$fh> });
close($fh);

if (defined(my $term_file = $scenario->{term_file})) {
    ($term_file) = $term_file =~ m{^(/.*)\z}s;
    $SIG{TERM} = sub {
        if (open(my $marker, '>', $term_file)) {
            close($marker);
        }
        POSIX::_exit(143);
    };
}

my $start_time = 1732600000;
my $mode = $scenario->{mode} // $opt{mode} // 'snapshot';
my $info = {
    directory => $scenario->{dir},
    sources => $scenario->{sources} // ['./'],
    'backup-user-id' => 100000,
};
$info->{'guest-uid'} = $scenario->{guest_uid} if defined($scenario->{guest_uid});

my $provider = PVE::BackupProvider::Plugin::Lodestore->new(
    'PVE::Storage::Custom::LodestorePlugin', {}, $opt{storage} // 'lodestore', sub { print "INFO: $_[1]\n" });

my $size;
my $ok = eval {
    $provider->job_init($start_time);
    $provider->backup_init($vmid, 'lxc', $start_time);
    print "INFO: backup mode: $mode\n";
    $provider->backup_get_mechanism($vmid, 'lxc');
    $provider->backup_container_prepare($vmid, $info);
    sleep($scenario->{pause}) if $scenario->{pause};

    # vzdump runs backup_container in a forked child, and the provider must not fork there.
    my $pid = fork() // die "fork failed: $!\n";
    if (!$pid) {
        my $status = eval {
            $provider->backup_container($vmid, $scenario->{config}, $scenario->{excludes} // [], $info);
            1;
        } ? 0 : 1;
        print STDERR "ERROR: $@" if $status;
        POSIX::_exit($status);
    }
    waitpid($pid, 0);
    my $child_failed = $? != 0;

    my $error;
    $error = "backup_container failed" if $child_failed;
    $error = "failure injected after the data" if !$child_failed && $scenario->{fail_after_data};
    my $stats = $provider->backup_cleanup($vmid, 'lxc', $error ? 0 : 1, { error => $error });
    $size = $stats->{stats}->{'archive-size'};
    die "$error\n" if $error;
    1;
};
my $error = $@;
eval { $provider->job_cleanup() };

if (!$ok) {
    print STDERR "ERROR: Backup job failed - $error";
    exit 255;
}
print "INFO: archive-size: $size\n";
exit 0;
