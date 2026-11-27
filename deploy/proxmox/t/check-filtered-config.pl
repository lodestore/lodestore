#!/usr/bin/perl
# SPDX-License-Identifier: Apache-2.0
# Differential check of the container configuration filter against the parser
# of Proxmox VE. Runs on a Proxmox VE node, where PVE::LXC::Config is installed:
#
#   perl check-filtered-config.pl corpus.jsonl
#
# Each line of the corpus is {"input": ..., "output": ..., "bridges": [...]}, as
# the ignored test guest_config_corpus_dump of lodestore-format writes it. For
# every output, as Proxmox VE parses it: each rootfs and mpN parses and is of
# type volume in the allocation form `{storage}:{size}`, there is no lxc, env,
# hookscript or devN key, features holds nesting only, unprivileged is 1, onboot
# is 0, there are no snapshot or pending sections, and each netN with a bridge
# uses a bridge of the line. For every input: no mount point that Proxmox VE classifies as a bind
# mount or a device keeps its key in the output. Prints each violation and exits
# with 1 when there is any.
use strict;
use warnings;

use JSON::PP;

my $json = JSON::PP->new->allow_nonref;

my $path = shift @ARGV;
die "usage: $0 corpus.jsonl\n" if !defined($path) || @ARGV;

require PVE::LXC::Config;

my $allocation = qr/^[a-z][a-z0-9._-]*:[0-9]+$/;

sub parse {
    my ($raw) = @_;
    local $SIG{__WARN__} = sub { };
    return PVE::LXC::Config::parse_pct_config('/lxc/999.conf', $raw);
}

sub check_output {
    my ($conf, $bridges) = @_;
    my @bad;
    for my $key (qw(lxc env hookscript)) {
        push @bad, "keeps $key" if exists $conf->{$key};
    }
    push @bad, "keeps $_" for grep { /^dev\d+$/ } sort keys %$conf;
    push @bad, 'has snapshot sections' if %{ $conf->{snapshots} // {} };
    push @bad, 'has pending changes' if %{ $conf->{pending} // {} };
    push @bad, 'unprivileged is not 1' if ($conf->{unprivileged} // '') ne '1';
    push @bad, 'onboot is not 0' if ($conf->{onboot} // '') ne '0';

    for my $key (grep { /^(rootfs|mp\d+)$/ } sort keys %$conf) {
        push @bad, "$key does not parse: $conf->{$key}"
            if !PVE::LXC::Config->parse_volume($key, $conf->{$key}, 1);
    }
    PVE::LXC::Config->foreach_volume(
        $conf,
        sub {
            my ($key, $mp) = @_;
            push @bad, "$key is of type $mp->{type}" if $mp->{type} ne 'volume';
            push @bad, "$key is not an allocation: $mp->{volume}"
                if $mp->{volume} !~ $allocation;
        },
    );

    if (defined(my $features = $conf->{features})) {
        my $parsed = eval { PVE::LXC::Config->parse_features($features) };
        if (!$parsed) {
            push @bad, "features does not parse: $features";
        } else {
            push @bad, "features keeps $_" for grep { $_ ne 'nesting' } sort keys %$parsed;
        }
    }

    for my $key (grep { /^net\d+$/ } sort keys %$conf) {
        my $net = eval { PVE::LXC::Config->parse_lxc_network($conf->{$key}, 1) };
        if (!$net) {
            push @bad, "$key does not parse: $conf->{$key}";
        } elsif (defined($net->{bridge}) && !$bridges->{ $net->{bridge} }) {
            push @bad, "$key uses a bridge that is not on the node: $net->{bridge}";
        }
    }
    return @bad;
}

sub check_input {
    my ($input, $output) = @_;
    my $conf = eval { parse($input) } or return;
    my @bad;
    PVE::LXC::Config->foreach_volume(
        $conf,
        sub {
            my ($key, $mp) = @_;
            push @bad, "keeps $key, a $mp->{type} mount point in the input"
                if $mp->{type} ne 'volume' && exists $output->{$key};
        },
    );
    return @bad;
}

open(my $fh, '<', $path) or die "$path: $!\n";
my ($count, $violations) = (0, 0);
while (my $line = <$fh>) {
    $count++;
    my $case = $json->decode($line);
    my %bridges = map { $_ => 1 } @{ $case->{bridges} };
    my $output = eval { parse($case->{output}) };
    my @bad;
    if (!$output) {
        @bad = ('the output does not parse: ' . ($@ || 'no configuration'));
    } else {
        @bad = (check_output($output, \%bridges), check_input($case->{input}, $output));
    }
    next if !@bad;
    $violations += @bad;
    print "line $count: $_\n" for @bad;
    print "  input:  ", $json->encode($case->{input}), "\n";
    print "  output: ", $json->encode($case->{output}), "\n";
}
close($fh);
die "$path holds no configurations\n" if !$count;
print "checked $count configurations against PVE::LXC::Config: $violations violations\n";
exit($violations ? 1 : 0);
