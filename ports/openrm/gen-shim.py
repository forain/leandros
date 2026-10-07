#!/usr/bin/env python3
"""Generate drivers/nvidia/src/stubs.rs: one extern "C" stub per OS callback
OpenRM's nv-kernel.o imports that neither the C glue (ports/openrm/glue) nor a
hand-written Rust module in drivers/nvidia/src already provides.

Signatures come from clang's JSON AST of NVIDIA's own headers, compiled with
the exact flags NVIDIA's Makefile uses (recovered with `make -n`), so a stub
can never silently disagree with the prototype RM was built against.

Each stub logs its name once and returns a failure value:
    NV_STATUS -> NV_ERR_NOT_SUPPORTED, NvlStatus -> -NVL_ERR_NOT_SUPPORTED,
    pointers -> NULL, NvBool/bool -> false, other ints -> 0, void -> nothing.

Usage: ports/openrm/gen-shim.py            (after ports/openrm/build.sh)
       ports/openrm/gen-shim.py --report   (also print the category table)
"""
import json
import os
import re
import subprocess
import sys
from collections import OrderedDict

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", ".."))
SHIM_DIR = os.path.join(ROOT, "drivers", "nvidia", "src")
OUT_RS = os.path.join(SHIM_DIR, "stubs.rs")


def sh_env():
    out = subprocess.check_output(
        ["bash", "-c", f'source "{HERE}/version.sh"; echo "$OPENRM_CACHE"; echo "$OPENRM_SRC"'],
        text=True).split("\n")
    return out[0], out[1]


CACHE, SRC = sh_env()
BUILD = os.environ.get("OPENRM_BUILD_DIR") or os.path.join(CACHE, "build-x86_64")
NVDIR = os.path.join(SRC, "src", "nvidia")
CC = os.path.join(CACHE, "tools", "cc")


def ref_command(ref_src, obj):
    cmd = subprocess.check_output(
        ["make", "-s", "-n", "-C", NVDIR, f"CC={CC}", "LD=true", "TARGET_ARCH=x86_64", "TARGET_OS=Linux",
         f"OUTPUTDIR={BUILD}", "NV_VERBOSE=1", "-W", ref_src, os.path.join(BUILD, obj)],
        text=True)
    for line in cmd.splitlines():
        if f" -c {ref_src} " in line:
            return line.split(f" -c {ref_src} ")[0]
    sys.exit(f"gen-shim: no compile command for {ref_src}")


def ast_decls(tu, ref_src, obj, wanted):
    base = ref_command(ref_src, obj)
    cmd = (f"{base} -I {HERE}/glue -Wno-everything -fsyntax-only "
           f"-Xclang -ast-dump=json {tu}")
    raw = subprocess.check_output(["bash", "-c", cmd], cwd=NVDIR)
    ast = json.loads(raw)
    found = {}
    for node in ast.get("inner", []):
        name = node.get("name")
        if node.get("kind") == "TypedefDecl" and name:
            TYPEDEFS[name] = node["type"].get("desugaredQualType") or node["type"]["qualType"]
        if name not in wanted or node.get("kind") not in ("FunctionDecl", "VarDecl"):
            continue
        if node.get("isImplicit"):
            continue
        prev = found.get(name)
        # prefer the declaration that carries parameter names
        if prev is None or (node["kind"] == "FunctionDecl" and len(node.get("inner", [])) >
                            len(prev.get("inner", []))):
            found[name] = node
    return found


# ---------------------------------------------------------------- types ----

TYPEDEFS = {}


def resolve(t):
    t = t.strip()
    for _ in range(16):
        bare = t.replace("const ", "").strip()
        if bare in TYPEDEFS:
            t = TYPEDEFS[bare].strip()
        else:
            break
    return t

INT_MAP = {
    "unsigned char": "u8", "signed char": "i8", "char": "c_char",
    "unsigned short": "u16", "short": "i16",
    "unsigned int": "u32", "int": "i32",
    "unsigned long": "u64", "long": "i64",
    "unsigned long long": "u64", "long long": "i64",
    "_Bool": "bool", "bool": "bool",
}


def rust_type(qual, desugared):
    t = (desugared or qual).strip()
    if "(*)" in t or "(**)" in t:
        return "*mut c_void"                         # function pointer
    if t.endswith("*") or "[" in t:
        if "[" in t:
            return "*mut c_void"
        stars = len(t) - len(t.rstrip("* "))
        inner = t.rstrip("* ").strip()
        depth = t.count("*")
        is_const = inner.startswith("const ")
        base = resolve(inner.replace("const ", "").strip())
        if depth == 1:
            if base == "char":
                return "*const c_char" if is_const else "*mut c_char"
            if base in INT_MAP and INT_MAP[base] not in ("bool",):
                return ("*const " if is_const else "*mut ") + INT_MAP[base]
            return "*const c_void" if is_const else "*mut c_void"
        if depth == 2 and base == "void":
            return "*mut *mut c_void"
        if depth == 2 and base == "char":
            return "*mut *mut c_char"
        return "*mut c_void"
    if t.startswith("enum ") or re.fullmatch(r"(const\s+)?enum\b.*", t):
        return "u32"
    if "va_list" in qual or "__builtin_va_list" in t or "__va_list_tag" in t:
        return "*mut c_void"
    t = t.replace("const ", "").strip()
    if t in INT_MAP:
        return INT_MAP[t]
    if t == "void":
        return None
    if t.startswith("struct ") or t.startswith("union "):
        raise ValueError(f"by-value aggregate: {t}")
    raise ValueError(f"unmapped type: {qual} / {desugared}")


def fail_value(ret_qual, rtype):
    if rtype is None:
        return None
    if "NV_STATUS" in ret_qual:
        return "NV_ERR_NOT_SUPPORTED"
    if "NvlStatus" in ret_qual:
        return "-NVL_ERR_NOT_SUPPORTED"
    if rtype.startswith("*mut"):
        return "core::ptr::null_mut()"
    if rtype.startswith("*const"):
        return "core::ptr::null()"
    if rtype == "bool":
        return "false"
    return "0"


# ----------------------------------------------------------- categories ----

CATEGORIES = [
    ("log", "Logging / debug / crash", r"^(out_string|os_dbg_|os_bug_check|os_dump_stack|os_(add|delete)_record_for_crashLog|nv_printf|nv_log_error)"),
    ("mem", "Kernel heap, string, libc-like", r"^(os_(alloc|free)_mem$|os_mem_|os_string_|os_strtoul|os_snprintf|os_get_random_bytes)"),
    ("dma", "Page / DMA allocation, cache maintenance", r"^(nv_(alloc|free)_pages|os_alloc_pages_node|os_(get|put)_page|os_get_page_refcount|os_count_tail_pages|nv_dma_|nv_set_dma_address_size|nv_requires_dma_remap|nv_(un)?register_(phys_pages|user_pages|sgt|peer_io_mem)|nv_alias_pages|nv_get_(num_)?phys_pages|os_flush_|nv_flush_coherent|os_numa_|os_offline_page|os_get_numa_node_memory_usage|nv_get_device_memory_config|nv_get_egm_info)"),
    ("map", "MMIO / kernel + user mappings, user copies", r"^(os_(un)?map_kernel_space|nv_(alloc|free)_(kernel|user)_mapping|nv_add_mapping_context_to_file|nv_get_usermap_access_params|os_match_mmap_offset|nv_(acquire|release)_mmap_lock|nv_revoke_gpu_mappings|nv_get_all_mappings_revoked_locked|nv_set_safe_to_mmap_locked|os_(un)?lock_user_pages|os_lookup_user_io_memory|os_memcpy_(to|from)_user|os_get_max_user_va|os_pat_supported)"),
    ("sync", "Locks, semaphores, wait queues, context queries", r"^(os_.*(spinlock|mutex|rwlock|semaphore|wait_queue)|os_semaphore_may_sleep|os_is_isr|os_wait_|os_wake_up)"),
    ("time", "Time, delays, timers", r"^(os_get_current_(time|tick|tick_hr)$|os_get_tick_resolution|os_delay|os_schedule|nv_.*_nano_timer|nv_(start|stop)_rc_timer|os_get_cpu_frequency)"),
    ("work", "Work queues / deferred work", r"^(os_queue_work_item|os_flush_work_queue|os_is_queue_flush_ongoing|nv_schedule_uvm_)"),
    ("pci", "PCI config, port I/O, interrupts", r"^(os_pci_|os_io_|os_enable_pci_req_atomics|nv_control_soc_irqs)"),
    ("fw", "Firmware + file access", r"^(nv_(get|put)_firmware|os_(open_temporary|read|write|close)_file|os_close_file)"),
    ("proc", "Process identity, access control, file-private, events, caps", r"^(os_get_current_(process|process_name|thread)$|os_get_current_process|os_get_euid|os_is_administrator|os_check_access|os_(get|put)_pid_info|os_find_ns_pid|os_is_init_ns|nv_(get|put)_file_private|nv_get_ctl_state|nv_get_event|nv_post_event|os_nv_cap_|nv_acquire_fabric_mgmt_cap|os_imex_|nvidia_caps_root)"),
    ("pm", "Power management", r"^(nv_(allow|disallow)_runtime_suspend|nv_dynamic_power_available|nv_idle_holdoff|nv_indicate_(not_)?idle|nv_platform_supports_s0ix|nv_s2idle_pm_configured|nv_audio_dynamic_power|nv_get_adapter_state)"),
    ("i2c", "I2C adapters", r"^nv_i2c_"),
    ("plat", "Platform / ACPI / system info / virtualization", r"^(nv_acpi_|os_get_acpi_rsdp_from_uefi|os_is_efi_enabled|nv_is_chassis_notebook|os_get_cpu_(count|number)|os_get_version_info|nv_get_screen_info|os_(dis|en)able_console_access|nv_set_primary_vga_status|os_is_vgx_hyper|os_is_grid_supported|os_get_grid_csp_support|os_device_vm_present|os_call_vgpu_vfio|os_inject_vgx_msi|os_is_nvswitch_present|nv_get_dev_minor|nv_is_gpu_accessible|nv_match_gpu_os_info|nv_get_updated_emu_seg|nv_get_syncpoint_aperture|nv_get_disp_smmu_stream_ids|os_pci_remove_supported)"),
    ("nvlink", "NVLink / NVSwitch OS layer", r"^(nvlink_|nvswitch_os_)"),
    ("spdm", "libspdm crypto backend (Confidential Computing only)", r"^libspdm_"),
    ("data", "Data symbols / registry (C glue)", r"^(os_page_|os_cc_|os_dma_buf_enabled|os_imex_channel_is_supported|nv_parms|os_registry_init|NVreg_)"),
]


def category(name):
    for key, _, rx in CATEGORIES:
        if re.match(rx, name):
            return key
    return "misc"


# ---------------------------------------------------------- haiku status ----

def haiku_status():
    hdir = os.path.join(CACHE, "nvidia-haiku", "nvidia_gsp", "nvidia")
    status = {}
    if not os.path.isdir(hdir):
        return status
    stubs = open(os.path.join(hdir, "stubs.c")).read()
    for m in re.finditer(r"^int (\w+)\(void\) \{panic", stubs, re.M):
        status[m.group(1)] = "panic"
    for fn in os.listdir(hdir):
        if fn == "stubs.c" or not fn.endswith((".cpp", ".c")):
            continue
        text = open(os.path.join(hdir, fn)).read()
        for m in re.finditer(r"^[A-Za-z_][\w \*]*?\b(\w+)\s*\(", text, re.M):
            status.setdefault(m.group(1), "impl")
        for m in re.finditer(r"^(?:NvU32|NvU64|NvU8|NvBool)\s+(\w+)\s*=", text, re.M):
            status.setdefault(m.group(1), "impl")
    return status


# ----------------------------------------------------------------- main ----

def read_list(path):
    return [l.strip() for l in open(path) if l.strip()]


def hand_written():
    done = set()
    for fn in os.listdir(SHIM_DIR):
        if not fn.endswith(".rs") or fn == "stubs.rs":
            continue
        text = open(os.path.join(SHIM_DIR, fn)).read()
        for m in re.finditer(r'#\[no_mangle\]\s*(?:pub\s+)?(?:unsafe\s+)?extern\s+"C"\s+fn\s+(\w+)', text):
            done.add(m.group(1))
    return done


def main():
    undefined = read_list(os.path.join(BUILD, "undefined.txt"))
    glue = set(read_list(os.path.join(BUILD, "glue-defined.txt")))
    manual = hand_written()
    todo = [s for s in undefined if s not in glue and s not in manual]

    decls = ast_decls(os.path.join(HERE, "glue", "protos_rm.c"),
                      "arch/nvalloc/unix/src/osapi.c", "osapi.o", set(todo))
    spdm_wanted = {s for s in todo if s.startswith("libspdm_")}
    decls.update(ast_decls(os.path.join(HERE, "glue", "protos_spdm.c"),
                           "src/kernel/gpu/spdm/spdm.c", "spdm.o", spdm_wanted))

    missing = [s for s in todo if s not in decls]
    if missing:
        sys.exit(f"gen-shim: no prototype found for: {' '.join(missing)}")

    by_cat = OrderedDict((k, []) for k, _, _ in CATEGORIES)
    by_cat["misc"] = []
    for s in todo:
        by_cat.setdefault(category(s), []).append(s)

    out = []
    out.append("// @generated by ports/openrm/gen-shim.py -- do not edit by hand.\n")
    out.append("//\n// One stub per OS callback libnvrm.a (OpenRM %s) imports that is not yet\n" % os.path.basename(SRC).rsplit("-", 1)[-1])
    out.append("// provided by the C glue or a hand-written module in this crate. To implement\n")
    out.append("// one, write it (same signature, #[no_mangle]) in a sibling module and re-run\n")
    out.append("// the generator: it drops every symbol a hand-written module defines.\n\n")
    out.append("#![allow(non_snake_case, unused_variables, clippy::missing_safety_doc)]\n\n")
    out.append("use core::ffi::{c_char, c_void};\n\n")
    out.append("use crate::{stub_called, NVL_ERR_NOT_SUPPORTED, NV_ERR_NOT_SUPPORTED};\n\n")
    fn_names = []
    for key, title, _ in CATEGORIES + [("misc", "Uncategorised", "")]:
        names = by_cat.get(key) or []
        if not names:
            continue
        out.append(f"// ---- {title} ({len(names)}) ----\n\n")
        for name in names:
            node = decls[name]
            if node["kind"] == "VarDecl":
                sys.exit(f"gen-shim: data symbol {name} must live in the C glue")
            ftype = node["type"]["qualType"]
            ret_q = ftype.split("(")[0].strip()
            ret_d = None
            params = [p for p in node.get("inner", []) if p.get("kind") == "ParmVarDecl"]
            if node.get("variadic"):
                sys.exit(f"gen-shim: variadic {name} must live in the C glue")
            # desugared return type: re-derive via a dummy pass on qualType only
            try:
                rtype = rust_type(ret_q, resolve(ret_q))
            except ValueError as e:
                sys.exit(f"gen-shim: {name}: return {e}")
            args = []
            for i, p in enumerate(params):
                pn = p.get("name") or f"a{i}"
                if pn in ("type", "ref", "fn", "in", "match", "loop", "move", "self", "impl", "use", "mod"):
                    pn += "_"
                try:
                    rt = rust_type(p["type"]["qualType"], p["type"].get("desugaredQualType"))
                except ValueError as e:
                    sys.exit(f"gen-shim: {name}: param {pn}: {e}")
                args.append(f"{pn}: {rt}")
            proto = f"{ftype}"
            out.append(f"/// C: `{name}`: `{proto}`\n")
            ret = f" -> {rtype}" if rtype else ""
            out.append("#[no_mangle]\n")
            out.append(f"pub unsafe extern \"C\" fn {name}({', '.join(args)}){ret} {{\n")
            out.append(f"    stub_called(\"{name}\");\n")
            fv = fail_value(ret_q, rtype)
            if fv:
                out.append(f"    {fv}\n")
            out.append("}\n\n")
            fn_names.append(name)

    out.append("/// Address of every stub, kept alive so fat LTO cannot drop a symbol only C\n")
    out.append("/// code references.\n")
    out.append("#[used]\n")
    out.append(f"pub static STUB_TABLE: [unsafe extern \"C\" fn(); {len(fn_names)}] = unsafe {{ [\n")
    for n in fn_names:
        out.append(f"    core::mem::transmute::<*const (), unsafe extern \"C\" fn()>({n} as *const ()),\n")
    out.append("] };\n")

    open(OUT_RS, "w").write("".join(out))

    # Every entry point NVIDIA's link step keeps (exports_link_command.txt):
    # referencing them all forces the whole RM graph live in the kernel link,
    # so an unresolved OS symbol anywhere fails the build instead of hiding
    # behind --gc-sections.
    exports = [l.strip().split("=", 1)[1] for l in open(os.path.join(NVDIR, "exports_link_command.txt"))
               if l.startswith("--undefined=")]
    exports = [e for e in exports if e != "pNVRM_ID"]          # data, not code
    ex = ["// @generated by ports/openrm/gen-shim.py -- do not edit by hand.\n",
          "//\n// Address-only declarations of every RM entry point exported by nv-kernel.o\n",
          "// (src/nvidia/exports_link_command.txt). Typed declarations for the ones the\n",
          "// shim calls live in rm.rs.\n\n",
          "#![allow(clashing_extern_declarations, dead_code)]\n\n",
          "extern \"C\" {\n"]
    ex += [f"    fn {e}();\n" for e in exports]
    ex += ["}\n\n#[used]\n",
           f"pub static RM_EXPORTS: [unsafe extern \"C\" fn(); {len(exports)}] = [\n"]
    ex += [f"    {e},\n" for e in exports]
    ex += ["];\n"]
    open(os.path.join(SHIM_DIR, "rm_exports.rs"), "w").write("".join(ex))
    print(f"gen-shim: {len(fn_names)} stubs -> {os.path.relpath(OUT_RS, ROOT)}"
          f" ({len(glue & set(undefined))} in C glue, {len(manual & set(undefined))} hand-written)")

    if "--report" in sys.argv:
        hs = haiku_status()
        allcat = OrderedDict()
        for s in undefined:
            allcat.setdefault(category(s), []).append(s)
        titles = {k: t for k, t, _ in CATEGORIES}
        titles["misc"] = "Uncategorised"
        for key, names in allcat.items():
            impl = sum(1 for n in names if hs.get(n) == "impl")
            pan = sum(1 for n in names if hs.get(n) == "panic")
            print(f"\n## {titles[key]} -- {len(names)} (haiku: {impl} impl, {pan} panic-stub)")
            for n in names:
                where = "glue" if n in glue else ("rust" if n in manual else "stub")
                print(f"  {n:48s} {where:5s} haiku={hs.get(n, '-')}")


if __name__ == "__main__":
    main()
