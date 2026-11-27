# SPDX-License-Identifier: Apache-2.0
package PVE::Storage;

# A stand-in for the storage API version constants of Proxmox VE, for tests off
# a Proxmox VE node. Set LODESTORE_FAKE_APIVER to try another version.

use strict;
use warnings;

use constant APIVER => $ENV{LODESTORE_FAKE_APIVER} // 17;
use constant APIAGE => 8;

1;
