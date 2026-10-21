#!/bin/sh
# Apply the BitBox02 udev rules without a reboot. Never fail the install.
if command -v udevadm >/dev/null 2>&1; then
    udevadm control --reload-rules >/dev/null 2>&1 || true
    udevadm trigger --subsystem-match=hidraw >/dev/null 2>&1 || true
    udevadm trigger --subsystem-match=usb --attr-match=idVendor=03eb >/dev/null 2>&1 || true
fi
exit 0
