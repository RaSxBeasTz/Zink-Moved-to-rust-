//! Windows audio endpoint discovery for recorder and streaming source pickers.

use windows::core::PCWSTR;
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::{
    eCapture, eRender, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ,
};

use crate::Result;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioEndpoint {
    pub id: String,
    pub name: String,
}

pub fn render_endpoints() -> Result<Vec<AudioEndpoint>> {
    enumerate(true)
}

pub fn capture_endpoints() -> Result<Vec<AudioEndpoint>> {
    enumerate(false)
}

fn enumerate(render: bool) -> Result<Vec<AudioEndpoint>> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let collection = enumerator
            .EnumAudioEndpoints(if render { eRender } else { eCapture }, DEVICE_STATE_ACTIVE)?;
        let mut endpoints = Vec::with_capacity(collection.GetCount()? as usize);
        for index in 0..collection.GetCount()? {
            let device = collection.Item(index)?;
            endpoints.push(endpoint_info(&device)?);
        }
        Ok(endpoints)
    }
}

unsafe fn endpoint_info(device: &IMMDevice) -> Result<AudioEndpoint> {
    let id_ptr = device.GetId()?;
    let id = PCWSTR(id_ptr.0)
        .to_string()
        .map_err(|error| crate::PipelineError::Audio(error.to_string()))?;
    windows::Win32::System::Com::CoTaskMemFree(Some(id_ptr.0 as *const _));

    let store = device.OpenPropertyStore(STGM_READ)?;
    let value = store.GetValue(&PKEY_Device_FriendlyName)?;
    let name = value.to_string();
    Ok(AudioEndpoint { id, name })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_audio_endpoints_have_stable_ids_and_names() {
        for endpoint in render_endpoints()
            .expect("enumerate render endpoints")
            .into_iter()
            .chain(capture_endpoints().expect("enumerate capture endpoints"))
        {
            assert!(!endpoint.id.trim().is_empty());
            assert!(!endpoint.name.trim().is_empty());
        }
    }
}
