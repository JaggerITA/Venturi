//! Which GPU decodes video (Settings > Playback): the setting resolved to
//! the devices every decoder of the app tries, in order.

use std::sync::{LazyLock, OnceLock, RwLock};
use vv_media::HwDevice;
use vv_render::wgpu;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HwDecodeMode {
    #[default]
    Auto,
    Off,
    Nvdec,
    Vulkan,
}

impl HwDecodeMode {
    /// The choices that exist on this platform: macOS has only VideoToolbox,
    /// which is what `Auto` picks there.
    pub fn available() -> &'static [Self] {
        if cfg!(target_os = "macos") {
            &[Self::Auto, Self::Off]
        } else {
            &[Self::Auto, Self::Off, Self::Nvdec, Self::Vulkan]
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Off => "off",
            Self::Nvdec => "nvdec",
            Self::Vulkan => "vulkan",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        [Self::Auto, Self::Off, Self::Nvdec, Self::Vulkan]
            .into_iter()
            .find(|mode| mode.id() == id)
    }
}

const NVIDIA_VENDOR_ID: u32 = 0x10de;

/// The devices for `mode` on a machine with `gpus` (its Vulkan adapters).
pub fn resolve(mode: HwDecodeMode, gpus: &[wgpu::AdapterInfo], macos: bool) -> Vec<HwDevice> {
    if macos {
        return match mode {
            HwDecodeMode::Off => Vec::new(),
            _ => vec![HwDevice::VideoToolbox],
        };
    }
    let real: Vec<&wgpu::AdapterInfo> = gpus
        .iter()
        .filter(|gpu| {
            matches!(
                gpu.device_type,
                wgpu::DeviceType::IntegratedGpu | wgpu::DeviceType::DiscreteGpu
            )
        })
        .collect();
    let of_type = |device_type| real.iter().find(|gpu| gpu.device_type == device_type);
    let vulkan_on = |gpu: &&wgpu::AdapterInfo| HwDevice::Vulkan(Some(gpu.name.clone()));
    let has_nvidia = real.iter().any(|gpu| gpu.vendor == NVIDIA_VENDOR_ID);
    match mode {
        HwDecodeMode::Off => Vec::new(),
        HwDecodeMode::Nvdec => vec![HwDevice::Cuda],
        HwDecodeMode::Vulkan => of_type(wgpu::DeviceType::DiscreteGpu)
            .or(real.first())
            .map(vulkan_on)
            .into_iter()
            .collect(),
        HwDecodeMode::Auto => {
            let integrated = of_type(wgpu::DeviceType::IntegratedGpu);
            let discrete = of_type(wgpu::DeviceType::DiscreteGpu);
            if let (Some(integrated), Some(_)) = (integrated, discrete) {
                // Hybrid laptop: decoding on the discrete GPU would wake it.
                return vec![vulkan_on(integrated)];
            }
            has_nvidia
                .then_some(HwDevice::Cuda)
                .into_iter()
                .chain(real.first().map(vulkan_on))
                .collect()
        }
    }
}

static GPUS: OnceLock<Vec<wgpu::AdapterInfo>> = OnceLock::new();

fn gpus() -> &'static [wgpu::AdapterInfo] {
    GPUS.get_or_init(|| {
        if cfg!(target_os = "macos") {
            Vec::new()
        } else {
            vv_render::vulkan_adapters()
        }
    })
}

/// What `Auto` decodes on here, for the settings: `None` while `start` is
/// still enumerating the GPUs, `Some(None)` if there is no GPU decoder.
pub fn auto_device() -> Option<Option<HwDevice>> {
    let macos = cfg!(target_os = "macos");
    let gpus = if macos { &[][..] } else { GPUS.get()? };
    Some(
        resolve(HwDecodeMode::Auto, gpus, macos)
            .into_iter()
            .find(|device| !vv_media::hw::known_unavailable(device)),
    )
}

pub fn device_label(device: &HwDevice) -> String {
    match device {
        HwDevice::VideoToolbox => "VideoToolbox".into(),
        HwDevice::Cuda => "NVDEC".into(),
        HwDevice::Vulkan(Some(gpu)) => format!("Vulkan, {gpu}"),
        HwDevice::Vulkan(None) => "Vulkan".into(),
    }
}

/// The setting read at startup; unset (tests) means software.
static STARTUP_MODE: OnceLock<HwDecodeMode> = OnceLock::new();

/// Resolved on first use, blocking the callers meanwhile: decoders opened
/// while `start` is still at work wait for it instead of staying software.
static DEVICES: LazyLock<RwLock<Vec<HwDevice>>> = LazyLock::new(|| {
    let mode = STARTUP_MODE.get().copied().unwrap_or(HwDecodeMode::Off);
    RwLock::new(created(mode))
});

/// Resolves `mode` off the UI thread: enumerating the GPUs and initializing
/// CUDA cost up to a few hundred ms.
pub fn start(mode: HwDecodeMode) {
    let _ = STARTUP_MODE.set(mode);
    std::thread::spawn(|| LazyLock::force(&DEVICES));
}

/// The devices of the current setting, for `Decoder::open_with`.
pub fn devices() -> Vec<HwDevice> {
    DEVICES.read().unwrap().clone()
}

pub fn apply(mode: HwDecodeMode) {
    let devices = created(mode);
    *DEVICES.write().unwrap() = devices;
}

fn created(mode: HwDecodeMode) -> Vec<HwDevice> {
    let mut devices = resolve(mode, gpus(), cfg!(target_os = "macos"));
    devices.retain(vv_media::hw::available);
    devices
}

/// Default budget for the HW surfaces: an eighth of the physical memory,
/// which on Apple Silicon is also the GPU's.
pub fn default_budget_bytes() -> usize {
    // SAFETY: plain queries, -1 on failure.
    let (pages, page_size) = unsafe {
        (
            libc::sysconf(libc::_SC_PHYS_PAGES),
            libc::sysconf(libc::_SC_PAGESIZE),
        )
    };
    if pages <= 0 || page_size <= 0 {
        return 1_000_000_000;
    }
    pages as usize * page_size as usize / 8
}

#[cfg(test)]
#[path = "tests/hw_decode.rs"]
mod tests;
