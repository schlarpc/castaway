"""Count the mirroring RTP that actually crossed the segment (#225).

The receiver's own journal can say a mirroring session was negotiated; it cannot say the
phone then sent anything, and "negotiated and silent" is a real failure mode — it is what
half of #184 looked like from the room. So this reads the segment capture and counts what
the *sender* put on the wire, which is the half a log line cannot fake.

Deliberately a hand-rolled pcap reader: the parse is thirty lines of struct, and the
alternative is a dependency in a build sandbox for Ethernet/IPv4/UDP framing that has not
changed since 1981.

    android-cast-rtp.py <pcap> <src-ip> <dst-ip> <dst-port> <min-packets> <min-bytes>
"""

import struct
import sys

# libpcap's file magic, in both endiannesses and both timestamp resolutions.
MAGICS = {
    0xA1B2C3D4: ("<", 1),
    0xD4C3B2A1: (">", 1),
    0xA1B23C4D: ("<", 1),
    0x4D3CB2A1: (">", 1),
}
LINKTYPE_ETHERNET = 1
ETHERTYPE_IPV4 = 0x0800
IPPROTO_UDP = 17


def packets(path):
    """Yield each record's captured bytes, whatever the file's endianness."""
    with open(path, "rb") as f:
        header = f.read(24)
        if len(header) < 24:
            raise SystemExit(f"FAIL: {path} is too short to be a pcap")
        (magic,) = struct.unpack("<I", header[:4])
        if magic not in MAGICS:
            raise SystemExit(f"FAIL: {path} is not a pcap (magic {magic:#x})")
        endian, _ = MAGICS[magic]
        (link,) = struct.unpack(endian + "I", header[20:24])
        if link != LINKTYPE_ETHERNET:
            raise SystemExit(f"FAIL: {path} is linktype {link}, not Ethernet")
        while True:
            record = f.read(16)
            if len(record) < 16:
                return
            _, _, captured, _ = struct.unpack(endian + "IIII", record)
            data = f.read(captured)
            if len(data) < captured:
                return  # a capture cut off mid-record; what we have is still counted
            yield data


def udp_payload(frame, src, dst, port):
    """The UDP payload of `frame` if it is the datagram we are counting, else None."""
    if len(frame) < 14 or struct.unpack("!H", frame[12:14])[0] != ETHERTYPE_IPV4:
        return None
    ip = frame[14:]
    if len(ip) < 20 or ip[0] >> 4 != 4:
        return None
    ihl = (ip[0] & 0x0F) * 4
    if ip[9] != IPPROTO_UDP or len(ip) < ihl + 8:
        return None
    if ".".join(str(b) for b in ip[12:16]) != src:
        return None
    if ".".join(str(b) for b in ip[16:20]) != dst:
        return None
    udp = ip[ihl:]
    if struct.unpack("!H", udp[2:4])[0] != port:
        return None
    length = struct.unpack("!H", udp[4:6])[0]
    return udp[8:length] if 8 <= length <= len(udp) else udp[8:]


def main():
    pcap, src, dst, port, min_packets, min_bytes = sys.argv[1:7]
    port, min_packets, min_bytes = int(port), int(min_packets), int(min_bytes)

    count = 0
    total = 0
    # RTP's version is the top two bits of the first octet and is 2 for every packet a
    # Cast sender emits. Counting it separately is what distinguishes "UDP arrived on the
    # port" from "the port carried the protocol we negotiated for it".
    rtp = 0
    for frame in packets(pcap):
        payload = udp_payload(frame, src, dst, port)
        if payload is None:
            continue
        count += 1
        total += len(payload)
        if payload and payload[0] >> 6 == 2:
            rtp += 1

    print(f"{src} -> {dst}:{port}  packets={count} rtp={rtp} payload_bytes={total}")
    if count < min_packets or total < min_bytes:
        raise SystemExit(
            f"FAIL: the phone negotiated a mirror and then sent almost nothing "
            f"(wanted >={min_packets} packets and >={min_bytes} bytes)"
        )
    if rtp != count:
        raise SystemExit(
            f"FAIL: {count - rtp} of {count} datagrams on the media port are not RTP"
        )
    print("ok: the phone's screen reached the segment")
    return 0


if __name__ == "__main__":
    sys.exit(main())
