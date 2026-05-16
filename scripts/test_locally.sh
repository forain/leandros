./scripts/build-all.sh --arch x86_64
./scripts/run-qemu.sh x86_64 -display none --direct > │ │ │ │ qemu_x86_direct.log 2>&1
sleep 15
kill $! || true
cat qemu_x86_direct.log || true

