# lane/intelimg (openbugs #91) - DELIVERED (static verification)

mkfs-f2fs-populated.py: INTEL=1 now fails if the stage lacks iris (iris_screen_create in libgallium), libvulkan_intel.so or intel_icd json; always stamps `<img>.gpu` = intel|generic.
write-usb.sh: x86_64 refuses an image not stamped intel (LEANDROS_USB_ALLOW_GENERIC_GPU=1 overrides). README documents the knob.
Evidence: predicate on real stages: gpu-stage-x86_64 -> False, gpu-stage-x86_64-intel -> True; gate snippet dies on `generic`, passes on `intel`. Full build-all not run. QEMU builds unchanged (stamp only).
