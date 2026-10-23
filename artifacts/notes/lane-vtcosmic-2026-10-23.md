# lane/vtcosmic 2026-10-23 (L11.7 VT switch on a real COSMIC session) -- BLOCKED by greeter login

Goal: verify the VT-switch fix (0a14742f) on a live COSMIC session, 10x Ctrl+Alt+F2/F1.

Result: NOT VERIFIED on COSMIC. cosmic-greeter rejects the correct password ("leandro")
typed via QMP/HMP sendkey, so no user session ever started (ffsession `desktop` step = None
in 5 runs, virgl and non-virgl; greeter card shows "Incorrect password"). Serial login
leandro/leandro works, so the password and hashes are fine; the failure is in the
greeter input/auth path (lane greeterpw owns it). Typed characters never show as dots
in the password field, even 8 s after typing (manual sendkey, 1 s/key).

Greeter-only evidence (same as vtclock, still valid): 10 round trips of Ctrl+Alt+F2/F1 on the
greeter under virgl: after each return the screen is non-black, 0.00-0.11 % pixel diff vs the
pre-switch shot (clock digits only); F2 shot is a text console (0.1 % nonblack). /proc/vtstat
was not captured in the final run (hostpost shell lacks grep); no pre-fix baseline taken.

Tooling added:
- `.claude/skills/run-leandros/vtloop.sh [N] [DIR]`: N chord round trips, screenshot after each,
  pixel diff vs pre-switch shot, /proc/vtstat counters. Use as `ffsession.py ... --hostpost "sh .claude/skills/run-leandros/vtloop.sh 10"`.
- `ffsession.py`: env `FFS_GREETER_PAINT=<s>` waits longer for the greeter's first paint (virgl ~90 s; the
  process exists at 0.7 s so keys typed then are lost).
Next: once greeterpw lands, rerun vtloop in a logged-in session (cosmic-term with `top`), expect
vt_restores==10, stale_presents==0, then revert 0a14742f for a baseline.
