//! Shared DXGI adapter numbering for discovery and D3D12 device creation.

use std::sync::OnceLock;

use anyhow::{Context, Result};
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
use windows::Win32::Graphics::Direct3D12::{D3D12CreateDevice, ID3D12Device};
use windows::Win32::Graphics::Dxgi::*;

pub struct AdapterInfo {
    pub index: u32,
    pub name: String,
    pub dedicated_mib: usize,
    pub software: bool,
    pub supports_d3d12: bool,
}

pub fn list() -> Result<Vec<AdapterInfo>> {
    let factory = factory()?;
    let mut adapters = Vec::new();
    // SAFETY: the factory owns its DXGI interfaces and these are read-only queries.
    unsafe {
        for index in 0.. {
            let adapter = match factory.EnumAdapters1(index) {
                Ok(adapter) => adapter,
                Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(error) => return Err(error).context("enumerating DXGI adapters"),
            };
            let desc = adapter
                .GetDesc1()
                .context("reading DXGI adapter description")?;
            adapters.push(AdapterInfo {
                index,
                name: String::from_utf16_lossy(&desc.Description)
                    .trim_end_matches('\0')
                    .to_owned(),
                dedicated_mib: desc.DedicatedVideoMemory / (1024 * 1024),
                software: desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0,
                supports_d3d12: probe(&adapter).is_ok(),
            });
        }
    }
    Ok(adapters)
}

/// Resolves and checks the explicit selection once per process.
pub fn selected() -> Result<Option<&'static IDXGIAdapter1>> {
    static ADAPTER: OnceLock<Result<Option<IDXGIAdapter1>>> = OnceLock::new();
    ADAPTER
        .get_or_init(resolve)
        .as_ref()
        .map(Option::as_ref)
        .map_err(|error| anyhow::anyhow!("{error:#}"))
}

fn resolve() -> Result<Option<IDXGIAdapter1>> {
    let Some(index) = crate::Config::load_adapter()? else {
        return Ok(None);
    };
    // SAFETY: the factory is live and DXGI validates the index.
    let adapter = unsafe { factory()?.EnumAdapters1(index) }
        .with_context(|| format!("cannot select adapter {index}; see `victory adapters`"))?;
    probe(&adapter).with_context(|| {
        format!("adapter {index} cannot create a D3D12 feature level 12_0 device")
    })?;
    Ok(Some(adapter))
}

fn factory() -> Result<IDXGIFactory4> {
    // SAFETY: DXGI writes the requested factory interface.
    unsafe { CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)) }.context("creating DXGI factory")
}

fn probe(adapter: &IDXGIAdapter1) -> windows::core::Result<()> {
    // SAFETY: a null output pointer tells D3D12 to check support without creating a device.
    unsafe {
        D3D12CreateDevice::<_, ID3D12Device>(adapter, D3D_FEATURE_LEVEL_12_0, std::ptr::null_mut())
    }
}
