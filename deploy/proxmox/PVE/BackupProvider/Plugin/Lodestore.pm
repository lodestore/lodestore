# SPDX-License-Identifier: Apache-2.0
package PVE::BackupProvider::Plugin::Lodestore;

# A thin backup provider: it forwards what vzdump and `pct restore` tell it to
# the Lodestore agent over the agent's local socket and returns the answers.
# It holds no repository keys and writes no backup data. A backup can only
# start from Lodestore, which puts a job token into the environment of vzdump.

use strict;
use warnings;

use IO::Socket::UNIX;
use JSON::PP;

use base qw(PVE::BackupProvider::Plugin::Base);

use constant PROTOCOL => 1;
use constant DEFAULT_SOCKET => '/run/lodestore/agent.sock';
use constant AGENT_CONFIG => '/etc/lodestore/agent.yaml';

my $json = JSON::PP->new->utf8->canonical;

# vzdump runs with taint checks on, so everything from %ENV is checked here.
sub untaint {
    my ($value, $pattern) = @_;
    return undef if !defined($value);
    return $value =~ $pattern ? $1 : undef;
}

my $SOCKET_PATH = qr{^(/[A-Za-z0-9._/-]+)\z};

sub agent_config {
    return AGENT_CONFIG;
}

# The top-level `socket:` of agent.yaml, for callers that vzdump did not start
# and so have no LODESTORE_AGENT_SOCKET (pvestatd, pvedaemon).
sub configured_socket {
    open(my $fh, '<', agent_config()) or return undef;
    my $path;
    while (my $line = <$fh>) {
        next if $line !~ /^socket:[ \t]*(['"]?)([^'"#\s]+)\1[ \t]*(?:#.*)?$/;
        $path = untaint($2, $SOCKET_PATH);
        last;
    }
    close($fh);
    return $path;
}

sub socket_path {
    return untaint($ENV{LODESTORE_AGENT_SOCKET}, $SOCKET_PATH) // configured_socket()
        // DEFAULT_SOCKET;
}

sub job_token {
    return untaint($ENV{LODESTORE_JOB_TOKEN}, qr/^([0-9a-f]{64})\z/);
}

sub required_token {
    return job_token()
        // die "restores from this storage are started by Lodestore; start this restore from Lodestore\n";
}

sub agent_connect {
    my $path = socket_path();
    my $sock = IO::Socket::UNIX->new(Peer => $path)
        or die "cannot reach the Lodestore agent at $path: $!\n";
    return $sock;
}

sub request {
    my ($sock, $op, %args) = @_;
    my $line = $json->encode({ v => PROTOCOL, op => $op, %args });
    print {$sock} $line, "\n"
        or die "cannot write to the Lodestore agent: $!\n";
    my $reply = <$sock>;
    die "the Lodestore agent closed the connection\n" if !defined($reply);
    my $answer = eval { $json->decode($reply) };
    die "the Lodestore agent sent an answer that is not valid\n" if ref($answer) ne 'HASH';
    if (!$answer->{ok}) {
        my $text = ($answer->{message} // 'no reason given') . " ($answer->{code})";
        die "the Lodestore agent refused the request: $text\n";
    }
    return $answer;
}

sub call {
    my ($op, %args) = @_;
    my $sock = agent_connect();
    my $answer = eval { request($sock, $op, token => required_token(), %args) };
    my $error = $@;
    close($sock);
    die $error if $error;
    return $answer;
}

sub text {
    my ($bytes) = @_;
    my $copy = $bytes;
    if (!utf8::is_utf8($copy)) {
        utf8::decode($copy) or die "the guest configuration is not UTF-8 text\n";
    }
    return $copy;
}

sub info_message {
    my ($info) = @_;
    my %out = (
        directory => "$info->{directory}",
        sources => [map { "$_" } @{ $info->{sources} // [] }],
    );
    for my $key (qw(backup-user-id bandwidth-limit guest-rtime)) {
        $out{$key} = $info->{$key} + 0 if defined($info->{$key});
    }
    for my $key (qw(firewall-config guest-uid cluster-id)) {
        $out{$key} = "$info->{$key}" if defined($info->{$key});
    }
    return \%out;
}

sub restore_id {
    my ($volname) = @_;
    return $1 if $volname =~ m!(?:^|/)restore/([0-9a-f]{32})\z!;
    die "'$volname' is not a Lodestore restore volume\n";
}

sub new {
    my ($class, $storage_plugin, $scfg, $storeid, $log_function) = @_;
    return bless {
        storage_plugin => $storage_plugin,
        scfg => $scfg,
        storeid => $storeid,
        log_function => $log_function,
    }, $class;
}

sub provider_name {
    return 'lodestore';
}

sub job_init {
    my ($self, $start_time) = @_;
    die "backups to this storage are started by Lodestore; start this backup from Lodestore\n"
        if !defined(job_token());
    call('pve_job_check');
    return;
}

sub job_cleanup {
    return;
}

sub backup_init {
    my ($self, $vmid, $vmtype, $start_time) = @_;
    die "virtual machines are backed up by Lodestore from a later release\n" if $vmtype ne 'lxc';
    return { 'archive-name' => "ct/$vmid/$start_time" };
}

sub backup_get_mechanism {
    my ($self, $vmid, $vmtype) = @_;
    die "virtual machines are backed up by Lodestore from a later release\n" if $vmtype ne 'lxc';
    return 'directory';
}

# One connection serves a whole backup of one container: prepare opens it as
# root, backup_container (a child of the same process) uses the inherited
# descriptor and never forks, and backup_cleanup finishes it.
sub backup_container_prepare {
    my ($self, $vmid, $info) = @_;
    my $sock = agent_connect();
    eval {
        request($sock, 'pve_ct_open',
            token => required_token(), vmid => $vmid + 0, info => info_message($info));
    };
    if (my $error = $@) {
        close($sock);
        die $error;
    }
    $self->{sock} = $sock;
    return;
}

sub backup_container {
    my ($self, $vmid, $guest_config, $exclude_patterns, $info) = @_;
    my $sock = $self->{sock} // die "the backup was not prepared\n";
    request($sock, 'pve_ct_data',
        token => required_token(),
        config => text($guest_config),
        excludes => [map { "$_" } @{ $exclude_patterns // [] }]);
    return;
}

sub backup_cleanup {
    my ($self, $vmid, $vmtype, $success, $info) = @_;
    my $sock = delete $self->{sock};
    return { stats => { 'archive-size' => 0 } } if !defined($sock);
    my $answer = eval {
        request($sock, 'pve_ct_finish',
            token => required_token(),
            success => ($success ? JSON::PP::true : JSON::PP::false),
            error => (ref($info) eq 'HASH' && defined($info->{error}) ? "$info->{error}" : undef));
    };
    my $error = $@;
    close($sock);
    die $error if $error;
    return { stats => { 'archive-size' => $answer->{archive_size} + 0 } };
}

sub backup_handle_log_file {
    return;
}

sub restore_get_mechanism {
    my ($self, $volname) = @_;
    restore_id($volname);
    return ('directory', 'lxc');
}

# PVE asks for the configuration twice per restore and fails if the answers
# differ, so the agent registers its answer once and this returns it as is.
sub archive_get_guest_config {
    my ($self, $volname) = @_;
    my $answer = call('pve_restore_config', restore_id => restore_id($volname));
    my $config = $answer->{config};
    utf8::encode($config);
    return $config;
}

# PVE writes whatever this returns to /etc/pve/firewall/{vmid}.fw without
# checks, so the firewall rules of a snapshot are never handed over.
sub archive_get_firewall_config {
    return undef;
}

sub restore_container_init {
    my ($self, $volname, $info) = @_;
    my $answer = call('pve_restore_open', restore_id => restore_id($volname));
    return { 'archive-directory' => $answer->{directory} };
}

sub restore_container_cleanup {
    my ($self, $volname, $info) = @_;
    call('pve_restore_close', restore_id => restore_id($volname));
    return;
}

sub restore_vm_init {
    die "virtual machines are restored by Lodestore from a later release\n";
}

sub restore_vm_cleanup {
    return;
}

sub restore_vm_volume_init {
    die "virtual machines are restored by Lodestore from a later release\n";
}

sub restore_vm_volume_cleanup {
    return;
}

1;
