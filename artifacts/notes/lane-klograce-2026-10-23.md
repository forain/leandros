# lane/klograce (L8.12) - PARTIAL/FIXED by analysis

Cause: klog::push/push_bytes did WRITERS+=1 (Acquire) then HEAD+=n (Relaxed). On aarch64 the HEAD
reservation can become visible before the WRITERS increment, so stable_head() saw WRITERS==0 twice
around an advanced HEAD and the flusher wrote reserved-but-unwritten slots (NUL). Also the 100k-spin
fallback returned raw HEAD, exposing unwritten runs whenever a writer lingered.
Fix: WRITERS/HEAD RMWs and stable_head loads are SeqCst; fallback returns LAST_STABLE (last proven
quiescent head) instead of HEAD. Still lock-free; IRQ/NMI safe. Panic path unchanged (raw HEAD).
Evidence: aarch64 release build + full regress PASS. No stress test: the ring has no userland reader
(only flushed to the stick journal), so NUL counting is not feasible without a new interface.
