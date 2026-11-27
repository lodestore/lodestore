# SPDX-License-Identifier: Apache-2.0
package PVE::BackupProvider::Plugin::Base;

# A stand-in for the base class of Proxmox VE, for tests off a Proxmox VE node.

use strict;
use warnings;

sub new {
    my ($class, $storage_plugin, $scfg, $storeid, $log_function) = @_;
    return bless {
        storage_plugin => $storage_plugin,
        scfg => $scfg,
        storeid => $storeid,
        log_function => $log_function,
    }, $class;
}

sub log {
    my ($self, $level, $message) = @_;
    $self->{log_function}->($level, $message) if $self->{log_function};
    return;
}

1;
