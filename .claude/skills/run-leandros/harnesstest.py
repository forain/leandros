#!/usr/bin/env python3
"""harnesstest - harness footguns are handled in code (host-only, no boot).

 * piped output is line-buffered without `python3 -u`
 * host tracing (-d / -trace) gets `-D <log>` automatically
 * a quoted command with arguments survives runtests parsing and the
   box/regress.sh -> ssh quoting (printf %q round trip)
Usage: harnesstest.py [arch]   (arch ignored)
"""
import os, subprocess, sys
HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import driver

fails = 0
def check(name, ok):
    global fails
    print(("PASS " if ok else "FAIL ") + name)
    fails += not ok

check("stdout_line_buffered", sys.stdout.line_buffering)
t = driver.ensure_trace_log(["qemu", "-d", "int"], "/x.log")
check("trace_d_adds_D", t[-2:] == ["-D", "/x.log"])
check("no_trace_untouched", driver.ensure_trace_log(["qemu"], "/x") == ["qemu"])
check("explicit_D_kept", driver.ensure_trace_log(["-d", "int", "-D", "/y"], "/x") == ["-d", "int", "-D", "/y"])
check("trace_file_kept", driver.ensure_trace_log(["-trace", "enable=a,file=/t"], "/x") == ["-trace", "enable=a,file=/t"])
check("trace_nofile_adds_D", driver.ensure_trace_log(["-trace", "enable=a"], "/x")[-2:] == ["-D", "/x"])

# regress.sh quotes runtests args with printf %q before the ssh hop.
args = ["--suite", "/bin/sh -c 'echo a b'"]
q = subprocess.run(["bash", "-c", 'printf "%q " "$@"', "_"] + args, capture_output=True, text=True).stdout
out = subprocess.run(["bash", "-c", 'printf "[%s]" ' + q], capture_output=True, text=True).stdout
check("quoted_cmd_survives_shell_hop", out == "[--suite][/bin/sh -c 'echo a b']")
bad = subprocess.run(["bash", "-c", 'printf "[%s]" ' + " ".join(args)], capture_output=True, text=True).stdout
check("unquoted_join_splits (documents old bug)", bad != out)
sys.exit(1 if fails else 0)
