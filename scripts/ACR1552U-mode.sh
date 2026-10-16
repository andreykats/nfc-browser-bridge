#!/bin/bash
# Read or change the host interface of an ACS ACR1552U NFC reader on macOS:
# HID keyboard (types card UIDs), CCID smart card reader, or both.
#
# Uses only what ships with macOS: osascript's JavaScript bridge to Apple's
# IOUSBHost framework. No Homebrew, Python or Xcode needed.
#
# It sends the vendor escape commands from the ACR1552U reference manual
# (section 6.1.14.7/8, Get/Set Host Interface) as raw CCID messages. macOS's
# own CCID driver refuses escape commands, so the script briefly takes the
# reader away from the smart card service, which needs an admin password.
set -euo pipefail

usage() {
    cat >&2 <<EOF
Usage: ${0##*/} MODE

  get    show the current mode
  hid    HID keyboard only
  ccid   CCID smart card reader only (factory default)
  both   HID keyboard + CCID smart card reader
EOF
    exit 2
}

[[ $# -eq 1 ]] || usage
case "$1" in get|hid|ccid|both) ;; *) usage ;; esac
[[ "$(uname -s)" == Darwin ]] || { echo "error: this script only runs on macOS" >&2; exit 1; }

if [[ $EUID -ne 0 ]]; then
    echo "An administrator password is needed to take the reader from macOS's smart card service."
    exec sudo /bin/bash "$0" "$@"
fi

if [[ $1 == hid ]]; then
    cat <<'EOF'
Warning: in HID-only mode the reader has no CCID interface, so this script
cannot switch it back. Undoing it would likely need ACS's Windows tool.
Use "both" to keep keyboard output and still be able to change modes here.
EOF
    read -r -p "Switch to HID keyboard only? [y/N] " answer </dev/tty
    [[ $answer == [yY]* ]] || { echo "Cancelled."; exit 1; }
fi

exec /usr/bin/osascript -l JavaScript - "$1" <<'JS'
ObjC.import('Foundation');
ObjC.import('stdlib');
ObjC.import('IOKit');
ObjC.import('IOUSBHost');
// Return the CFTypeRef as an object so NSNumber values can be read directly.
ObjC.bindFunction('IORegistryEntryCreateCFProperty', ['id', ['unsigned int', 'id', 'void *', 'unsigned int']]);
// IOUSBHost objects are created through objc_msgSend with the block and error
// parameters typed as plain pointers. The JXA bridge turns any JS value passed
// for a block parameter, even null, into a block, and IOUSBHost then aborts
// the process when it calls that block on the first device notification.
ObjC.bindFunction('objc_getClass', ['id', ['char *']]);
ObjC.bindFunction('class_createInstance', ['id', ['id', 'unsigned long']]);
ObjC.bindFunction('sel_registerName', ['void *', ['char *']]);
ObjC.bindFunction('objc_msgSend', ['id', ['id', 'void *', 'unsigned int', 'unsigned long', 'id', 'void *', 'void *']]);
const INIT_SEL = $.sel_registerName('initWithIOService:options:queue:error:interestHandler:');

const ACS_VID = 0x072F;
const ACR1552_PIDS = [0x2303, 0x2308];
const CONFIG_VALUE = 1;
const CCID_CLASS = 0x0B;
const DEVICE_CAPTURE = 1;            // IOUSBHostObjectInitOptionsDeviceCapture, root only
const PC_TO_RDR_ESCAPE = 0x6B;
const RDR_TO_PC_ESCAPE = 0x83;
const TIMEOUT_S = 3;
const READ_SIZE = 512;
const LATIN1 = 5;                    // NSISOLatin1StringEncoding: maps bytes 0-255 1:1

const MODES = { hid: 0x00, ccid: 0x01, both: 0x02 };
const MODE_TEXT = {
    0x00: 'HID keyboard only',
    0x01: 'CCID reader only (factory default)',
    0x02: 'HID keyboard + CCID reader',
};

class ReaderError extends Error {}

function hex(bytes) {
    return bytes.map(b => b.toString(16).padStart(2, '0')).join(' ');
}

function toNSData(bytes) {
    return $(String.fromCharCode(...bytes)).dataUsingEncoding(LATIN1);
}

function fromNSData(data) {
    return Array.from(ObjC.unwrap($.NSString.alloc.initWithDataEncoding(data, LATIN1)), c => c.charCodeAt(0));
}

function readBytes(ptr, length) {
    return fromNSData($.NSData.dataWithBytesLength(ptr, length));
}

// Returns an IOUSBHostDevice/IOUSBHostInterface, or a nil object on failure.
// Default queue, no error object, no interest handler.
function openUSBObject(className, service, options) {
    const instance = $.class_createInstance($.objc_getClass(className), 0);
    return $.objc_msgSend(instance, INIT_SEL, service, options, $(), null, null);
}

function isAttached(service) {
    return $.IORegistryEntryInPlane(service, 'IOService') !== 0;
}

function intProperty(service, key) {
    const value = $.IORegistryEntryCreateCFProperty(service, $(key), null, 0);
    return value.isNil() ? null : value.intValue;
}

function iterate(iterator) {
    const services = [];
    let s;
    while ((s = $.IOIteratorNext(iterator)) !== 0) services.push(s);
    $.IOObjectRelease(iterator);
    return services;
}

function findReader() {
    const it = Ref();
    if ($.IOServiceGetMatchingServices(0, $.IOServiceMatching('IOUSBHostDevice'), it) !== 0) {
        throw new ReaderError('IOKit device lookup failed');
    }
    const acs = iterate(it[0])
        .filter(s => intProperty(s, 'idVendor') === ACS_VID)
        .map(s => ({ service: s, pid: intProperty(s, 'idProduct') }));
    const readers = acs.filter(d => ACR1552_PIDS.includes(d.pid));
    if (readers.length === 1) return readers[0].service;
    if (readers.length > 1) throw new ReaderError('more than one ACR1552U is connected; unplug the others');
    const pids = acs.map(d => d.pid.toString(16).padStart(4, '0'));
    throw new ReaderError(pids.length
        ? `no ACR1552U in CCID mode found (ACS USB product IDs present: ${pids.join(', ')}). If it is in HID-only mode, this script cannot reach it.`
        : 'no ACR1552U found; is it plugged in?');
}

function ccidInterfaceService(deviceService) {
    const it = Ref();
    if ($.IORegistryEntryGetChildIterator(deviceService, 'IOService', it) !== 0) return null;
    const ccid = iterate(it[0])
        .filter(s => $.IOObjectConformsTo(s, 'IOUSBHostInterface'))
        .map(s => ({ service: s, number: intProperty(s, 'bInterfaceNumber'), cls: intProperty(s, 'bInterfaceClass') }))
        .filter(i => i.cls === CCID_CLASS)
        .sort((a, b) => a.number - b.number);
    // The lowest-numbered CCID interface is the PICC (contactless) one, which
    // is where the manual says escape commands must be sent.
    return ccid.length ? ccid[0] : null;
}

function openCcidInterface(device, deviceService) {
    // Capturing the device detaches every driver. Reselect the configuration
    // without registering interfaces for matching, so the smart card service
    // cannot grab them back, then open the PICC interface ourselves.
    if (!device.configureWithValueMatchInterfacesError(CONFIG_VALUE, false, null)) {
        throw new ReaderError('could not configure the reader');
    }
    for (let attempt = 0; attempt < 50; attempt++) {
        const found = ccidInterfaceService(deviceService);
        if (found) {
            const intf = openUSBObject('IOUSBHostInterface', found.service, 0);
            if (!intf.isNil()) return { intf, service: found.service, number: found.number };
        }
        delay(0.1);
    }
    throw new ReaderError("could not open the reader's CCID interface");
}

function bulkPipes(device, intf, interfaceNumber) {
    const ptr = device.configurationDescriptor;
    const total = (b => b[2] | (b[3] << 8))(readBytes(ptr, 9));
    const cfg = readBytes(ptr, total);
    let inInterface = false;
    let outAddr = null;
    let inAddr = null;
    for (let i = 0; i + 4 < cfg.length && cfg[i] > 0; i += cfg[i]) {
        const type = cfg[i + 1];
        if (type === 0x04) {
            inInterface = cfg[i + 2] === interfaceNumber && cfg[i + 3] === 0;
        } else if (type === 0x05 && inInterface && (cfg[i + 3] & 0x03) === 0x02) {
            const addr = cfg[i + 2];
            if (addr & 0x80) inAddr = inAddr ?? addr;
            else outAddr = outAddr ?? addr;
        }
    }
    if (outAddr === null || inAddr === null) throw new ReaderError('CCID interface is missing its bulk endpoints');
    const pipe = (addr) => {
        const p = intf.copyPipeWithAddressError(addr, null);
        if (p.isNil()) throw new ReaderError(`could not open endpoint 0x${addr.toString(16)}`);
        return p;
    };
    return { out: pipe(outAddr), in: pipe(inAddr) };
}

function escape(pipes, payload, seq) {
    const len = payload.length;
    const msg = [PC_TO_RDR_ESCAPE, len & 0xFF, (len >> 8) & 0xFF, (len >> 16) & 0xFF, len >>> 24, 0, seq, 0, 0, 0, ...payload];
    if (!pipes.out.sendIORequestWithDataBytesTransferredCompletionTimeoutError(toNSData(msg).mutableCopy, null, TIMEOUT_S, null)) {
        throw new ReaderError('sending the command to the reader failed');
    }
    for (;;) {
        const buf = $.NSMutableData.dataWithLength(READ_SIZE);
        if (!pipes.in.sendIORequestWithDataBytesTransferredCompletionTimeoutError(buf, null, TIMEOUT_S, null)) {
            throw new ReaderError('no reply from the reader');
        }
        const r = fromNSData(buf);
        if (r[0] !== RDR_TO_PC_ESCAPE || r[6] !== seq) throw new ReaderError(`unexpected reply: ${hex(r.slice(0, 16))}`);
        const status = r[7] >> 6;
        if (status === 2) continue;  // reader asked for more time
        if (status !== 0) throw new ReaderError(`reader rejected the command (bStatus 0x${hex([r[7]])}, bError 0x${hex([r[8]])})`);
        const dataLen = (r[1] | (r[2] << 8) | (r[3] << 16) | (r[4] << 24)) >>> 0;
        if (10 + dataLen > READ_SIZE) throw new ReaderError(`reply too long (${dataLen} bytes)`);
        return r.slice(10, 10 + dataLen);
    }
}

function hostInterface(reply) {
    if (reply.length !== 6 || hex(reply.slice(0, 5)) !== 'e1 00 00 00 01' || !(reply[5] in MODE_TEXT)) {
        throw new ReaderError(`unexpected reply: ${hex(reply)}`);
    }
    return reply[5];
}

function main(cmd) {
    const deviceService = findReader();
    const device = openUSBObject('IOUSBHostDevice', deviceService, DEVICE_CAPTURE);
    if (device.isNil()) throw new ReaderError('could not take control of the reader (the script must run as root)');
    let opened = null;
    try {
        opened = openCcidInterface(device, deviceService);
        const pipes = bulkPipes(device, opened.intf, opened.number);

        const current = hostInterface(escape(pipes, [0xE0, 0x00, 0x00, 0x93, 0x00], 0));
        console.log(`Current mode: ${MODE_TEXT[current]}`);
        if (cmd === 'get') return;

        const wanted = MODES[cmd];
        if (current === wanted) {
            console.log('Nothing to change.');
            return;
        }
        const now = hostInterface(escape(pipes, [0xE0, 0x00, 0x00, 0x93, 0x01, wanted], 1));
        if (now !== wanted) throw new ReaderError(`reader reports ${MODE_TEXT[now]} after the change`);
        console.log(`New mode: ${MODE_TEXT[now]}`);
        console.log('If macOS does not pick up the change, unplug the reader and plug it back in.');
    } finally {
        // destroy hangs if the reader already re-enumerated (its registry entry
        // is gone); exiting releases those handles anyway.
        if (opened !== null && isAttached(opened.service)) opened.intf.destroy;
        if (isAttached(deviceService)) device.destroy;  // resets the reader and hands it back to macOS
    }
}

function run(argv) {
    try {
        main(argv[0]);
    } catch (e) {
        console.log(`error: ${e.message}`);
        $.exit(1);
    }
}
JS
