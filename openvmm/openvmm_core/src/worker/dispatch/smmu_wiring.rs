// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(guest_arch = "aarch64")]

//! SMMU resource resolution and wiring helpers for aarch64 VMs.
//!
//! This module handles combining SMMU MMIO ranges (from the memory layout
//! allocator) with SPI assignments (from the SPI allocator) into resolved
//! resources and instantiating SMMU chipset devices.

use chipset_device_resources::IRQ_LINE_SET;
use guestmem::GuestMemory;
use std::sync::Arc;
use vm_topology::pcie::PcieHostBridge;
use vmotherboard::ChipsetBuilder;

/// Size of the physical MSI doorbell window shadowed by Hyper-V.
const MSI_DOORBELL_WINDOW_SIZE: u64 = 0x1_0000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SmmuBackend {
    Local,
    HyperV,
}

fn smmu_backend(accel: bool) -> SmmuBackend {
    if accel {
        SmmuBackend::HyperV
    } else {
        SmmuBackend::Local
    }
}

/// Default advertised OAS (in bits) for an `oas=auto` SMMU.
///
/// This is a fixed sizing policy, not a computed maximum: rather than sizing
/// the advertised OAS to the guest memory layout (or to the host's supported
/// IPA width, which on aarch64/KVM can be up to 52 bits), an `auto` SMMU
/// advertises a constant 48 bits. This matches the fixed-OAS approach taken by
/// Hyper-V's emulated SMMU.
///
/// The memory-layout allocator packs high MMIO compactly bottom-up just above
/// guest RAM, so for typical configurations every translatable address sits
/// far below 48 bits (256 TiB). This is not a hard guarantee, though: a large
/// enough RAM size, or an explicitly pinned high MMIO/ECAM base, can place
/// addresses above 256 TiB (up to the host IPA width). Such a configuration
/// must pass an explicit `oas=` (e.g. `oas=52`) rather than relying on `auto`.
const DEFAULT_AUTO_OAS_BITS: u8 = 48;

/// Valid SMMUv3 output address sizes (IDR5.OAS encodings), in bits.
const VALID_OAS_BITS: [u8; 7] = [32, 36, 40, 42, 44, 48, 52];

/// Resolved resources for a single SMMUv3 instance, combining MMIO and SPI
/// allocations.
pub(super) struct ResolvedSmmuResources {
    /// MMIO base address (from the memory layout allocator).
    pub base: u64,
    /// GIC INTID for the event queue interrupt (from the SPI allocator).
    pub evtq_intid: u32,
    /// GIC INTID for the global error interrupt (from the SPI allocator).
    pub gerr_intid: u32,
}

/// Combines SMMU MMIO ranges from the memory layout with SPI assignments from
/// the SPI layout into resolved resources.
pub(super) fn resolve_smmu_resources(
    smmu_ranges: &[memory_range::MemoryRange],
    spi_layout: &crate::worker::spi_layout::ResolvedSpiLayout,
) -> Vec<ResolvedSmmuResources> {
    smmu_ranges
        .iter()
        .zip(&spi_layout.smmu)
        .map(|(range, spis)| ResolvedSmmuResources {
            base: range.start(),
            evtq_intid: spis.evtq_intid,
            gerr_intid: spis.gerr_intid,
        })
        .collect()
}

/// A hypervisor-managed virtual IOMMU to be created for a root complex.
///
/// On ARM64 the hypervisor emulates the guest-visible SMMUv3: it intercepts the
/// MMIO window at `base`, processes the guest command queue and shadows the
/// guest stream table onto the physical SMMU. OpenVMM therefore allocates the
/// window and interrupts and asks the hypervisor to create the vSMMU, but does
/// not emulate it itself.
#[derive(Debug, Clone)]
pub struct VirtIommuSetup {
    /// Caller-assigned virtual IOMMU ID, unique within the partition.
    pub virt_iommu_id: u32,
    /// Guest page number of the vSMMU register window.
    pub base_gpa_page: u64,
    /// GIC INTID for the event queue interrupt.
    pub evtq_intid: u32,
    /// GIC INTID for the global error interrupt.
    pub gerr_intid: u32,
    /// Index of the root complex this virtual IOMMU covers.
    pub rc_index: u32,
    /// Advertise endpoint ATS. PASID-tagged traffic is controlled separately.
    pub ats: bool,
    /// Guest substream/PASID width; nonzero requests PASID/SVA.
    pub ssid_bits: u8,
    /// Guest output address width.
    pub oas_bits: u8,
}

/// Preserve firmware-assigned requester IDs on root complexes with a DIRECT
/// MSI RMR; Linux ignores such RMR mappings without preserve_config.
pub(super) fn mark_rmr_bridges_preserve_config(
    bridges: &mut [PcieHostBridge],
    configs: &[vmm_core::acpi_builder::AcpiSmmuConfig],
) {
    for config in configs.iter().filter(|config| config.msi_window.is_some()) {
        if let Some(bridge) = bridges
            .iter_mut()
            .find(|bridge| bridge.index == config.rc_index)
        {
            bridge.preserve_boot_config = true;
        }
    }
}

/// Result of [`setup_smmu`].
pub(super) struct SmmuDevicesResult {
    /// Per-RC SMMU shared state, indexed parallel to `pcie_host_bridges`.
    /// `None` for root complexes without an SMMU, and for accelerated SMMUs
    /// (which the hypervisor emulates, so there is no local shared state).
    pub shared_states: Vec<Option<Arc<smmu::SmmuSharedState>>>,
    /// ACPI IORT configuration for each SMMU instance.
    pub configs: Vec<vmm_core::acpi_builder::AcpiSmmuConfig>,
    /// Virtual IOMMUs the hypervisor must create, for accelerated SMMUs.
    pub virt_iommus: Vec<VirtIommuSetup>,
}

/// Instantiate SMMU chipset devices for root complexes that have SMMU
/// configured.
///
/// This is the single entry point for all SMMU setup in dispatch. It
/// iterates root complex configs, creates one `SmmuDevice` per RC with
/// `iommu: Some(Smmu)`, and wires up interrupts.
pub(super) fn setup_smmu(
    root_complexes: &[openvmm_defs::config::PcieRootComplexConfig],
    resolved_smmu_resources: &[ResolvedSmmuResources],
    pcie_host_bridges: &[PcieHostBridge],
    chipset_builder: &ChipsetBuilder<'_>,
    gm: &GuestMemory,
) -> anyhow::Result<SmmuDevicesResult> {
    // Instantiate SMMU chipset devices.
    let mut shared_states: Vec<Option<Arc<smmu::SmmuSharedState>>> =
        vec![None; pcie_host_bridges.len()];
    let mut configs = Vec::new();

    // Iterate RCs with SMMU enabled, zipping with resolved MMIO+SPI resources.
    let smmu_rcs = root_complexes
        .iter()
        .enumerate()
        .filter_map(|(rc_pos, rc)| match &rc.iommu {
            Some(openvmm_defs::config::PcieIommuConfig::Smmu {
                accel,
                ats,
                ssid_bits,
                oas,
            }) => Some((rc_pos, rc, *accel, *ats, *ssid_bits, *oas)),
            _ => None,
        });

    let mut virt_iommus = Vec::new();

    for ((rc_pos, rc, accel, ats, ssid_bits, oas), smmu) in smmu_rcs.zip(resolved_smmu_resources) {
        // Resolve the requested OAS into a concrete advertised value.
        let oas_bits = match oas {
            openvmm_defs::config::SmmuOas::Auto => DEFAULT_AUTO_OAS_BITS,
            openvmm_defs::config::SmmuOas::Fixed(bits) => {
                anyhow::ensure!(
                    VALID_OAS_BITS.contains(&bits),
                    "SMMU on root complex {}: OAS {bits} is not a valid SMMUv3 output \
                     address size (expected one of {:?})",
                    rc.name,
                    VALID_OAS_BITS
                );
                bits
            }
        };

        // An accelerated SMMU is emulated by the hypervisor, which intercepts
        // the MMIO window at `smmu.base`. Do not instantiate a local SmmuDevice
        // for it: that would register a second front-end on the same GPA range
        // and would also wrap device DMA/MSI in software translation that the
        // hypervisor is already performing in hardware. Record it so the caller
        // can ask the hypervisor to create it, and still emit the IORT node so
        // the guest discovers it.
        if smmu_backend(accel) == SmmuBackend::HyperV {
            virt_iommus.push(VirtIommuSetup {
                virt_iommu_id: (virt_iommus.len() + 1) as u32,
                base_gpa_page: smmu.base >> 12,
                evtq_intid: smmu.evtq_intid,
                gerr_intid: smmu.gerr_intid,
                rc_index: pcie_host_bridges[rc_pos].index,
                ats,
                ssid_bits,
                oas_bits,
            });
            configs.push(vmm_core::acpi_builder::AcpiSmmuConfig {
                rc_index: pcie_host_bridges[rc_pos].index,
                segment: pcie_host_bridges[rc_pos].segment,
                base: smmu.base,
                event_gsiv: smmu.evtq_intid,
                gerr_gsiv: smmu.gerr_intid,
                ats_supported: ats,
                device_stream_ids: Vec::new(),
                msi_window: Some((
                    openvmm_defs::config::DEFAULT_GIC_V2M_DOORBELL_BASE,
                    MSI_DOORBELL_WINDOW_SIZE,
                )),
            });
            continue;
        }

        let evtq_irq_vector = smmu.evtq_intid - *vmm_core::emuplat::gic::SPI_RANGE.start();
        let gerror_irq_vector = smmu.gerr_intid - *vmm_core::emuplat::gic::SPI_RANGE.start();
        let device_name = format!("smmu:{}", rc.name);
        let smmu_config = smmu::SmmuConfig {
            sidsize: 16,
            oas: oas_bits,
        };
        let smmu_device =
            chipset_builder
                .arc_mutex_device(device_name.as_str())
                .add(|services| {
                    let evtq_irq = services.new_line(IRQ_LINE_SET, "evtq", evtq_irq_vector);
                    let gerror_irq = services.new_line(IRQ_LINE_SET, "gerror", gerror_irq_vector);
                    smmu::SmmuDevice::new(
                        smmu.base,
                        gm.clone(),
                        &smmu_config,
                        Some(evtq_irq),
                        Some(gerror_irq),
                    )
                })?;

        shared_states[rc_pos] = Some(smmu_device.lock().shared_state().clone());
        configs.push(vmm_core::acpi_builder::AcpiSmmuConfig {
            rc_index: pcie_host_bridges[rc_pos].index,
            segment: pcie_host_bridges[rc_pos].segment,
            base: smmu.base,
            event_gsiv: smmu.evtq_intid,
            gerr_gsiv: smmu.gerr_intid,
            ats_supported: false,
            device_stream_ids: Vec::new(),
            msi_window: None,
        });
    }

    Ok(SmmuDevicesResult {
        shared_states,
        configs,
        virt_iommus,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accelerated_smmu_uses_hyperv_not_local_device() {
        assert_eq!(smmu_backend(true), SmmuBackend::HyperV);
        assert_eq!(smmu_backend(false), SmmuBackend::Local);
    }

    #[test]
    fn direct_msi_rmr_forces_preserve_config() {
        let mut bridges = vec![PcieHostBridge {
            index: 7,
            segment: 8,
            start_bus: 0,
            end_bus: 255,
            ecam_range: memory_range::MemoryRange::new(0..0x1000),
            low_mmio: memory_range::MemoryRange::new(0x1000..0x2000),
            high_mmio: memory_range::MemoryRange::new(0x1_0000_0000..0x1_0000_1000),
            cxl: None,
            vnode: None,
            preserve_bars: false,
            preserve_boot_config: false,
        }];
        let configs = vec![vmm_core::acpi_builder::AcpiSmmuConfig {
            rc_index: 7,
            segment: 8,
            base: 0xeffa_0000,
            event_gsiv: 35,
            gerr_gsiv: 36,
            ats_supported: false,
            device_stream_ids: vec![0x100],
            msi_window: Some((0xeff6_8000, 0x1_0000)),
        }];
        mark_rmr_bridges_preserve_config(&mut bridges, &configs);
        assert!(bridges[0].preserve_boot_config);
    }
}
