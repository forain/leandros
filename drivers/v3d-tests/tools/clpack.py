"""Minimal V3D 7.1 control-list packer driven by Mesa's cle/v3d_packet.xml.

Same rules as Mesa's gen_pack_header.py: packet field starts are relative to the byte after the
opcode, `default=` values apply, `minus_one` fields store value-1, and an address field holds the
whole 32-bit byte address placed at the field's first byte (its low bits must be zero).
Used by gen_selftest_cl.py; the result is checked in as drivers/src/v3d/selftest_cl.rs.
"""
import re, struct, sys
import xml.etree.ElementTree as ET

VER = 71

def norm(s):
    return re.sub(r'[^a-z0-9]+', '_', s.lower()).strip('_')

def ver_ok(e):
    lo = int(e.get('min_ver', '0')); hi = int(e.get('max_ver', '999'))
    return lo <= VER <= hi

class Spec:
    def __init__(self, path):
        root = ET.parse(path).getroot()
        self.enums = {}
        for e in root.iter('enum'):
            if ver_ok(e):
                self.enums.setdefault(e.get('name'), {}).update(
                    {norm(v.get('name')): int(v.get('value')) for v in e.iter('value') if ver_ok(v)})
        self.packets = {}
        for p in root.iter('packet'):
            if ver_ok(p):
                self.packets[norm(p.get('name'))] = p

    def fields(self, name):
        p = self.packets[norm(name)]
        out = []
        for f in p.iter('field'):
            if not ver_ok(f):
                continue
            st = f.get('start')
            start = int(st[:-1]) * 8 if st.endswith('b') else int(st)
            out.append(dict(name=norm(f.get('name')), start=start + 8, size=int(f.get('size')),
                            type=f.get('type'), minus_one=f.get('minus_one') == 'true',
                            default=f.get('default'),
                            values={norm(v.get('name')): int(v.get('value')) for v in f.iter('value')}))
        return int(p.get('code')), out

    def length(self, name):
        code, fields = self.fields(name)
        end = max([f['start'] + f['size'] for f in fields] + [8])
        return (end + 7) // 8

    def pack(self, name, **vals):
        """Returns (bytes, {field: byte_offset}) for address fields."""
        code, fields = self.fields(name)
        n = self.length(name)
        bits = code
        addrs = {}
        names = {f['name'] for f in fields}
        for k in vals:
            if k not in names:
                sys.exit('%s: unknown field %s (have %s)' % (name, k, sorted(names)))
        for f in fields:
            v = vals.get(f['name'])
            if v is None and f['default'] is not None:
                v = int(f['default'])
            if v is None:
                if f['minus_one']:
                    sys.exit('%s: minus_one field %s needs a value' % (name, f['name']))
                v = 0
            if isinstance(v, str):
                table = f['values'] or self.enums.get(f['type'], {})
                v = table[norm(v)]
            t = f['type']
            if t == 'address':
                fb = f['start'] // 8 * 8
                assert v & ((1 << (f['start'] - fb)) - 1) == 0, 'misaligned address'
                if v:
                    addrs[f['name']] = fb // 8
                bits |= v << fb
                continue
            if t == 'float':
                v = struct.unpack('<I', struct.pack('<f', v))[0]
            elif t == 'f187':
                v = struct.unpack('<I', struct.pack('<f', v))[0] >> 16
            if f['minus_one']:
                v -= 1
            assert 0 <= v < (1 << f['size']), (name, f['name'], v)
            bits |= v << f['start']
        return bits.to_bytes(n, 'little'), addrs
