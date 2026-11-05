#!/usr/bin/env python3
"""KFS, Kern's on-disk filesystem: host-side tool.

    kfs.py mkfs IMAGE SIZE_MIB [--force]   create a disk image with an empty KFS
    kfs.py check IMAGE                     verify the whole structure (exit 1 on errors)
    kfs.py ls IMAGE [PATH]                 list a directory
    kfs.py cat IMAGE PATH                  print a file
    kfs.py put IMAGE HOST_FILE PATH        copy a file in (its directory must exist)

The format (all integers little-endian; the kernel side is kernel/src/fs/kfs.rs):

  Blocks are 4096 bytes. Block 0 is the superblock, then the inode bitmap,
  the block bitmap, the inode table, and the data blocks.

  Superblock: magic "KernFS01" (8 bytes), block_size u32, inode_size u32,
  then u64s: total_blocks, inode_count, inode_bitmap_start,
  inode_bitmap_blocks, block_bitmap_start, block_bitmap_blocks,
  inode_table_start, inode_table_blocks, data_start, root_inode, created.

  Bitmaps: bit n of byte n/8 (LSB first) set = inode / block n in use.
  Inode 0 is never used (0 means "no inode"), inode 1 is the root
  directory. Every block before data_start is marked in use.

  Inode, 128 bytes: kind u16 (0 free, 1 file, 2 directory), links u16
  (directory entries naming it), reserved u32, size u64, mtime u64 (Unix
  seconds), 12 direct block pointers u32, an indirect pointer u32 (a block
  of 1024 pointers) and a double-indirect pointer u32. Pointer 0 = a hole
  (reads as zeros). No pointer exists past the end of the file.

  Directory: a file of 256-byte entries: inode u32 (0 = free slot),
  kind u8, name_len u8, reserved u16, name (up to 248 bytes, no '/' or NUL).
  There are no "." or ".." entries.
"""
import os
import struct
import sys
import time

BLOCK = 4096
INODE_SIZE = 128
ENTRY_SIZE = 256
MAX_NAME = 248
MAGIC = b"KernFS01"
DIRECT = 12
PTRS = BLOCK // 4
FREE, FILE, DIR = 0, 1, 2
ROOT = 1

SB_FORMAT = "<8sII11Q"  # magic, block_size, inode_size, then 11 u64s
SB_FIELDS = ("total_blocks", "inode_count", "inode_bitmap_start", "inode_bitmap_blocks",
             "block_bitmap_start", "block_bitmap_blocks", "inode_table_start",
             "inode_table_blocks", "data_start", "root_inode", "created")
INODE_FORMAT = "<HHIQQ12III"  # kind, links, reserved, size, mtime, direct[12], indirect, double


class FsError(Exception):
    pass


def layout(total_blocks):
    """The superblock fields for a fresh filesystem of `total_blocks`."""
    inode_count = max(64, (total_blocks // 4 + 31) // 32 * 32)  # one inode per 16 KiB
    bits = BLOCK * 8
    sb = {"total_blocks": total_blocks, "inode_count": inode_count}
    at = 1
    sb["inode_bitmap_start"], sb["inode_bitmap_blocks"] = at, (inode_count + bits - 1) // bits
    at += sb["inode_bitmap_blocks"]
    sb["block_bitmap_start"], sb["block_bitmap_blocks"] = at, (total_blocks + bits - 1) // bits
    at += sb["block_bitmap_blocks"]
    sb["inode_table_start"], sb["inode_table_blocks"] = at, inode_count * INODE_SIZE // BLOCK
    at += sb["inode_table_blocks"]
    sb["data_start"] = at
    sb["root_inode"] = ROOT
    sb["created"] = int(time.time())
    if at + 16 > total_blocks:
        raise FsError("disk too small")
    return sb


class Image:
    def __init__(self, path, writable=False):
        self.f = open(path, "r+b" if writable else "rb")
        raw = self.read_block(0)
        fields = struct.unpack_from(SB_FORMAT, raw)
        if fields[0] != MAGIC:
            raise FsError(f"{path}: not a KFS image (bad magic)")
        if fields[1] != BLOCK or fields[2] != INODE_SIZE:
            raise FsError(f"{path}: unsupported block or inode size")
        self.sb = dict(zip(SB_FIELDS, fields[3:]))

    # ---------------------------------------------------------------- blocks
    def read_block(self, n):
        self.f.seek(n * BLOCK)
        data = self.f.read(BLOCK)
        return data.ljust(BLOCK, b"\0")

    def write_block(self, n, data):
        assert len(data) == BLOCK
        self.f.seek(n * BLOCK)
        self.f.write(data)

    def bit(self, start, n):
        block = self.read_block(start + n // (BLOCK * 8))
        i = n % (BLOCK * 8)
        return block[i // 8] >> (i % 8) & 1

    def set_bit(self, start, n, value):
        b = start + n // (BLOCK * 8)
        block = bytearray(self.read_block(b))
        i = n % (BLOCK * 8)
        if value:
            block[i // 8] |= 1 << (i % 8)
        else:
            block[i // 8] &= ~(1 << (i % 8))
        self.write_block(b, bytes(block))

    def alloc(self, start, count, first):
        for n in range(first, count):
            if not self.bit(start, n):
                self.set_bit(start, n, 1)
                return n
        raise FsError("no space left")

    def alloc_block(self):
        n = self.alloc(self.sb["block_bitmap_start"], self.sb["total_blocks"], self.sb["data_start"])
        self.write_block(n, b"\0" * BLOCK)
        return n

    # ---------------------------------------------------------------- inodes
    def inode(self, ino):
        per = BLOCK // INODE_SIZE
        block = self.read_block(self.sb["inode_table_start"] + ino // per)
        v = struct.unpack_from(INODE_FORMAT, block, (ino % per) * INODE_SIZE)
        return {"kind": v[0], "links": v[1], "size": v[3], "mtime": v[4],
                "direct": list(v[5:17]), "indirect": v[17], "double": v[18]}

    def write_inode(self, ino, node):
        per = BLOCK // INODE_SIZE
        b = self.sb["inode_table_start"] + ino // per
        block = bytearray(self.read_block(b))
        struct.pack_into(INODE_FORMAT, block, (ino % per) * INODE_SIZE, node["kind"], node["links"], 0,
                         node["size"], node["mtime"], *node["direct"], node["indirect"], node["double"])
        self.write_block(b, bytes(block))

    def pointers(self, block):
        return list(struct.unpack("<1024I", self.read_block(block)))

    def block_of(self, node, index, create=False):
        """Data block number for block `index` of a file (0 = hole)."""
        def slot(table_block, i):
            ptrs = self.pointers(table_block)
            if ptrs[i] == 0 and create:
                ptrs[i] = self.alloc_block()
                self.write_block(table_block, struct.pack("<1024I", *ptrs))
            return ptrs[i]
        if index < DIRECT:
            if node["direct"][index] == 0 and create:
                node["direct"][index] = self.alloc_block()
            return node["direct"][index]
        index -= DIRECT
        if index < PTRS:
            if node["indirect"] == 0:
                if not create:
                    return 0
                node["indirect"] = self.alloc_block()
            return slot(node["indirect"], index)
        index -= PTRS
        if node["double"] == 0:
            if not create:
                return 0
            node["double"] = self.alloc_block()
        middle = slot(node["double"], index // PTRS)
        return slot(middle, index % PTRS) if middle else 0

    def read_file(self, node):
        out = bytearray()
        for i in range((node["size"] + BLOCK - 1) // BLOCK):
            b = self.block_of(node, i)
            out += self.read_block(b) if b else b"\0" * BLOCK
        return bytes(out[:node["size"]])

    def entries(self, node):
        data = self.read_file(node)
        for off in range(0, len(data), ENTRY_SIZE):
            ino, kind, name_len = struct.unpack_from("<IBB", data, off)
            if ino:
                yield off, ino, kind, data[off + 8:off + 8 + name_len]

    def lookup(self, path):
        ino = ROOT
        for part in [p for p in path.split("/") if p]:
            node = self.inode(ino)
            if node["kind"] != DIR:
                raise FsError(f"{path}: not a directory")
            found = [e for e in self.entries(node) if e[3] == part.encode()]
            if not found:
                raise FsError(f"{path}: no such file or directory")
            ino = found[0][1]
        return ino

    # ---------------------------------------------------------------- writing
    def put(self, data, path):
        parent_path, _, name = path.rstrip("/").rpartition("/")
        name_b = name.encode()
        if not name_b or len(name_b) > MAX_NAME or b"\0" in name_b:
            raise FsError(f"bad file name {name!r}")
        parent = self.lookup(parent_path or "/")
        pnode = self.inode(parent)
        if pnode["kind"] != DIR:
            raise FsError(f"{parent_path}: not a directory")
        if any(e[3] == name_b for e in self.entries(pnode)):
            raise FsError(f"{path}: already exists")
        ino = self.alloc(self.sb["inode_bitmap_start"], self.sb["inode_count"], 2)
        node = {"kind": FILE, "links": 1, "size": len(data), "mtime": int(time.time()),
                "direct": [0] * DIRECT, "indirect": 0, "double": 0}
        for i in range(0, len(data), BLOCK):
            b = self.block_of(node, i // BLOCK, create=True)
            self.write_block(b, data[i:i + BLOCK].ljust(BLOCK, b"\0"))
        self.write_inode(ino, node)
        # Directory entry: first free slot, or append.
        dir_data = self.read_file(pnode)
        slot = next((off for off in range(0, len(dir_data), ENTRY_SIZE)
                     if struct.unpack_from("<I", dir_data, off)[0] == 0), len(dir_data))
        entry = struct.pack("<IBBH", ino, FILE, len(name_b), 0) + name_b.ljust(MAX_NAME, b"\0")
        b = self.block_of(pnode, slot // BLOCK, create=True)
        block = bytearray(self.read_block(b))
        block[slot % BLOCK:slot % BLOCK + ENTRY_SIZE] = entry
        self.write_block(b, bytes(block))
        pnode["size"] = max(pnode["size"], slot + ENTRY_SIZE)
        pnode["mtime"] = int(time.time())
        self.write_inode(parent, pnode)

    # ---------------------------------------------------------------- checking
    def check(self):
        """Every invariant of the format; returns a list of problems."""
        sb, problems = self.sb, []
        expect = layout(sb["total_blocks"])
        for key in SB_FIELDS[:-1]:
            if key in ("total_blocks",):
                continue
            if sb[key] != expect[key]:
                problems.append(f"superblock {key} = {sb[key]}, expected {expect[key]}")
        if problems:
            return problems
        used_blocks = {}  # block -> owner description
        links = {}
        reachable = set()

        def claim(block, owner):
            if not sb["data_start"] <= block < sb["total_blocks"]:
                problems.append(f"{owner}: block {block} out of range")
            elif block in used_blocks:
                problems.append(f"{owner}: block {block} also used by {used_blocks[block]}")
            else:
                used_blocks[block] = owner

        def walk_inode(ino, where):
            node = self.inode(ino)
            if node["kind"] not in (FILE, DIR):
                problems.append(f"{where}: inode {ino} has kind {node['kind']}")
                return None
            nblocks = (node["size"] + BLOCK - 1) // BLOCK
            for i, b in enumerate(node["direct"]):
                if b:
                    claim(b, f"{where} block {i}")
                    if i >= nblocks:
                        problems.append(f"{where}: pointer past the end of the file (block {i})")
            if node["indirect"]:
                claim(node["indirect"], f"{where} indirect table")
                for i, b in enumerate(self.pointers(node["indirect"])):
                    if b:
                        claim(b, f"{where} block {DIRECT + i}")
                        if DIRECT + i >= nblocks:
                            problems.append(f"{where}: pointer past the end of the file (block {DIRECT + i})")
            if node["double"]:
                claim(node["double"], f"{where} double-indirect table")
                for j, mid in enumerate(self.pointers(node["double"])):
                    if mid:
                        claim(mid, f"{where} indirect table {j}")
                        for i, b in enumerate(self.pointers(mid)):
                            if b:
                                index = DIRECT + PTRS + j * PTRS + i
                                claim(b, f"{where} block {index}")
                                if index >= nblocks:
                                    problems.append(f"{where}: pointer past the end of the file (block {index})")
            return node

        stack = [(ROOT, "/")]
        links[ROOT] = 1
        while stack:
            ino, path = stack.pop()
            if ino in reachable:
                problems.append(f"{path}: directory inode {ino} reachable twice")
                continue
            reachable.add(ino)
            node = walk_inode(ino, path)
            if node is None or node["kind"] != DIR:
                if node is not None and ino == ROOT:
                    problems.append("root is not a directory")
                continue
            if node["size"] % ENTRY_SIZE:
                problems.append(f"{path}: directory size {node['size']} is not a multiple of {ENTRY_SIZE}")
            names = set()
            for _, child, kind, name in self.entries(node):
                child_path = path.rstrip("/") + "/" + name.decode(errors="replace")
                if not name or b"/" in name or b"\0" in name:
                    problems.append(f"{child_path}: bad name {name!r}")
                if name in names:
                    problems.append(f"{child_path}: duplicate name")
                names.add(name)
                if not 2 <= child < sb["inode_count"]:
                    problems.append(f"{child_path}: inode {child} out of range")
                    continue
                links[child] = links.get(child, 0) + 1
                child_node = self.inode(child)
                if child_node["kind"] != kind:
                    problems.append(f"{child_path}: entry says kind {kind}, inode says {child_node['kind']}")
                if kind == DIR:
                    stack.append((child, child_path))
                elif child not in reachable:
                    reachable.add(child)
                    walk_inode(child, child_path)

        for ino in range(sb["inode_count"]):
            in_use = self.bit(sb["inode_bitmap_start"], ino)
            should = ino == 0 or ino in reachable
            if in_use != should:
                problems.append(f"inode {ino}: bitmap says {'used' if in_use else 'free'}, but it is "
                                f"{'reachable' if ino in reachable else 'unreachable'}")
            if ino in reachable and self.inode(ino)["links"] != links.get(ino, 0):
                problems.append(f"inode {ino}: links = {self.inode(ino)['links']}, but {links.get(ino, 0)} entries name it")
        for block in range(sb["total_blocks"]):
            in_use = self.bit(sb["block_bitmap_start"], block)
            should = block < sb["data_start"] or block in used_blocks
            if in_use != should:
                what = used_blocks.get(block, "metadata" if block < sb["data_start"] else "nothing")
                problems.append(f"block {block}: bitmap says {'used' if in_use else 'free'}, used by {what}")
        return problems


def mkfs(path, size_mib, force=False):
    if os.path.exists(path) and not force:
        raise FsError(f"{path} exists (use --force to overwrite it)")
    total = size_mib * 1024 * 1024 // BLOCK
    sb = layout(total)
    with open(path, "wb") as f:
        f.truncate(total * BLOCK)
    img = Image.__new__(Image)
    img.f = open(path, "r+b")
    img.sb = sb
    raw = struct.pack(SB_FORMAT, MAGIC, BLOCK, INODE_SIZE, *(sb[k] for k in SB_FIELDS))
    img.write_block(0, raw.ljust(BLOCK, b"\0"))
    for b in range(sb["data_start"]):
        img.set_bit(sb["block_bitmap_start"], b, 1)
    img.set_bit(sb["inode_bitmap_start"], 0, 1)
    img.set_bit(sb["inode_bitmap_start"], ROOT, 1)
    img.write_inode(ROOT, {"kind": DIR, "links": 1, "size": 0, "mtime": int(time.time()),
                           "direct": [0] * DIRECT, "indirect": 0, "double": 0})
    img.f.close()


def main(argv):
    if len(argv) < 2:
        sys.exit(__doc__)
    cmd, args = argv[1], argv[2:]
    try:
        if cmd == "mkfs" and len(args) in (2, 3):
            mkfs(args[0], int(args[1]), force=args[2:] == ["--force"])
        elif cmd == "check" and len(args) == 1:
            problems = Image(args[0]).check()
            for p in problems:
                print("kfs check:", p)
            if problems:
                sys.exit(1)
            print(f"{args[0]}: clean")
        elif cmd == "ls" and len(args) in (1, 2):
            img = Image(args[0])
            node = img.inode(img.lookup(args[1] if len(args) == 2 else "/"))
            if node["kind"] != DIR:
                raise FsError("not a directory")
            for _, ino, kind, name in img.entries(node):
                child = img.inode(ino)
                print(f"{child['size']:>10}  {name.decode(errors='replace')}{'/' if kind == DIR else ''}")
        elif cmd == "cat" and len(args) == 2:
            img = Image(args[0])
            node = img.inode(img.lookup(args[1]))
            if node["kind"] != FILE:
                raise FsError("not a file")
            sys.stdout.buffer.write(img.read_file(node))
        elif cmd == "put" and len(args) == 3:
            with open(args[1], "rb") as src:
                data = src.read()
            Image(args[0], writable=True).put(data, args[2])
        else:
            sys.exit(__doc__)
    except FsError as e:
        sys.exit(f"kfs: {e}")


if __name__ == "__main__":
    main(sys.argv)
