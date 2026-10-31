# Shared constants for the dual-boot fixture (sourced by POSIX sh / bash).
# Geometry mirrors the HP EliteBook 840 G5 EndeavourOS layout: ESP at LBA 4096,
# ext4 at LBA 2101248, swap directly after (odd start), small unusable tail.
FX_DISK_SECTORS=100663296            # 48 GiB virtual
FX_ESP_START=4096
FX_ESP_SECTORS=2097152               # 1 GiB
FX_P2_START=2101248
FX_TAIL_SECTORS=14336                # 7 MiB tail, like the laptop
FX_SWAP_SECTORS=4194304              # 2 GiB
FX_BACKUP_GPT_SECTORS=34
FX_P3_END=$((FX_DISK_SECTORS - FX_BACKUP_GPT_SECTORS - FX_TAIL_SECTORS - 1))
FX_P3_START=$((FX_P3_END - FX_SWAP_SECTORS + 1))
FX_GAP_GIB=${FX_GAP_GIB:-0}          # unallocated GiB left between p2 and p3 (0 = none)
FX_GAP_SECTORS=$((FX_GAP_GIB * 2097152))
FX_P2_END=$((FX_P3_START - 1 - FX_GAP_SECTORS))
FX_P2_SECTORS=$((FX_P2_END - FX_P2_START + 1))
FX_P2_BLOCKS=$((FX_P2_SECTORS * 512 / 4096))

FX_DISK_GUID=4c3f0a52-7d1e-4c8e-9b1a-0a6d5c2f1001
FX_P1_GUID=4c3f0a52-7d1e-4c8e-9b1a-0a6d5c2f1101
FX_P2_GUID=4c3f0a52-7d1e-4c8e-9b1a-0a6d5c2f1102
FX_P3_GUID=4c3f0a52-7d1e-4c8e-9b1a-0a6d5c2f1103
FX_P2_UUID=0b8f6a3e-5a47-4d3c-8e0b-2f7a9c1d4e02   # ext4 filesystem UUID
FX_SWAP_UUID=9d2c71a4-3b58-4f6e-a1c7-5e0f8b3a6d03
FX_ESP_VOLID=4E1F2A3B
FX_MACHINE_ID=3f9a1c52e7b64d08a5c1b7e0d4f26a93
FX_P2_LABEL=endeavouros
FX_FILLER_GIB=${FX_FILLER_GIB:-10}   # random filler that forces data past 9 GiB
FX_DATA_MIB=${FX_DATA_MIB:-2048}     # total test data written after the filler
