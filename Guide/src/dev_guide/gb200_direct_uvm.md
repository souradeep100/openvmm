# GB200 direct UVM on MSHV

This path implements Design A: Hyper-V is the only owner of the guest-visible
SMMUv3, while OpenVMM provides configuration, VFIO ownership, Hyper-V object
creation and ACPI firmware.

PASID/SVA and PCIe ATS are independent capabilities. A guest SSID width requests
physical PASID support without enabling endpoint ATS. ATS is an additional,
explicit opt-in and remains disabled by default.

## Command line

PASID/SVA-only is the pre-KDNET architecture and build-validation mode:

```text
--iommu id=iommu0 --direct-iommu iommu=iommu0 \
--smmu rc=rc0,accel,ssid-bits=14 \
--vfio host=0008:06:00.0,port=rp0,iommu=iommu0
```

This requests a PASID-capable DIRECT HWPT and advertises a 14-bit guest SSID
space. It does not set the SMMUv3 IDR0 ATS bit, the IORT PCI root ATS attribute,
or Hyper-V's physical ATS fields.

After ATS has passed the KDNET safety gate, it can be added explicitly:

```text
--iommu id=iommu0 --direct-iommu iommu=iommu0 \
--smmu rc=rc0,accel,ats,ssid-bits=14 \
--vfio host=0008:06:00.0,port=rp0,iommu=iommu0
```

Nonzero `ssid-bits` requires `accel` and VFIO devices on that root complex to
use a `--direct-iommu` context. `ats` additionally requires nonzero
`ssid-bits`; `ssid-bits` may be 0-20. An accelerated root complex does not
instantiate OpenVMM's local `SmmuDevice`.

## Safety and qualification

PASID-only keeps physical PCIe ATS and endpoint ATC behavior disabled. It is
architecture- and build-ready for pre-KDNET validation, but it is not evidence
that NVIDIA GB200 UVM will successfully select or run a PASID-only mode. NVIDIA
sources permit `uvm_ats_mode=0`, where `uvm_ats_bind_gpu()` returns success
without binding SVA; when ATS VA-space mode is enabled, its SVA path calls
`iommu_sva_bind_device()`. The Linux ARM SMMUv3 SVA path does not itself require
endpoint ATS, but GB200 runtime behavior remains unqualified until tested with
the matching stack.

Real ATS remains KDNET-gated because enabling it changes physical endpoint and
IOMMU behavior. Do not add `ats` merely to obtain a guest PASID.

## Identities

The DIRECT vDEVICE identity is the Hyper-V logical device ID encoded from the
host physical segment and BDF. The Hyper-V vSID is the final guest requester ID
after PCI resource assignment. IORT publishes the full identity mapping from
every 16-bit guest BDF to the same vSID; compact sequential vSIDs are not used.

One Hyper-V virtual IOMMU is created per accelerated root complex. A DIRECT
iommufd context may share its kernel DIRECT vIOMMU across devices, while each
device retains its own logical ID, vDEVICE, HWPT and guest vSID.

## Firmware

OpenVMM publishes a matching SMMUv3 IORT node. SSID width remains advertised
when `ssid-bits` is nonzero even if ATS is off. The SMMUv3 IDR0 ATS bit and PCI
root ATS attribute are set only when `ats` is explicit. Each accelerated root
complex also receives a 64-KiB-aligned, identity-only MSI doorbell RMR with one
mapping per DIRECT device. The root complex uses `preserve_config`, because
Linux ignores an RMR whose firmware requester IDs are not preserved. Existing
SRAT Generic Initiator and coherent-memory entries remain unchanged.

## Lifecycle

Startup order is:

1. create the DIRECT kernel vIOMMU, per-device vDEVICE and DIRECT HWPT;
2. attach the VFIO cdev to the HWPT;
3. assign guest PCI resources;
4. create the Hyper-V virtual IOMMU and bind logical devices to identity vSIDs;
5. build firmware.

Failures unwind completed Hyper-V bindings. Direct cleanup explicitly issues
`VFIO_DEVICE_DETACH_IOMMUFD_PT` before destroying the HWPT, then destroys the
vDEVICE and the shared DIRECT vIOMMU after the last device. Failed detach or
destroy operations retain their object IDs and error state for retry. The VM fd
is held until the DIRECT manager is released.

Hyper-V currently has no virtual-IOMMU destroy hypercall. OpenVMM therefore
unbinds every logical device and relies on partition teardown to reclaim the
empty Hyper-V vIOMMU.

## Runtime dependencies

The OpenVMM source mirrors the reviewed, not-yet-upstream DIRECT kernel UAPI:

- `IOMMU_VIOMMU_TYPE_DIRECT = 3`;
- `IOMMU_HWPT_DATA_DIRECT = 3`;
- `IOMMU_HWPT_DIRECT_FLAG_PASID = 1 << 0`;
- `IOMMU_HWPT_DIRECT_FLAG_ATS = 1 << 1`, valid only with PASID;
- `iommu_viommu_direct` carries the MSHV partition fd;
- VFIO attach/detach payloads include a zero PASID field for whole-device use.

A kernel without that exact UAPI will compile OpenVMM but cannot run this path.
This implementation is build/review only until the matching MSHV/iommufd stack
is deployed and PASID-only behavior is qualified. ATS remains disabled until
KDNET-backed validation is available.
