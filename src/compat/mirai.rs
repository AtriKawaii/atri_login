use ricq::device::{Device, OSVersion};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug)]
pub struct MiraiDeviceInfo {
    #[serde(rename = "deviceInfoVersion")]
    pub device_info_version: u8,
    pub data: MiraiDevice,
}

impl From<Device> for MiraiDeviceInfo {
    fn from(d: Device) -> Self {
        Self {
            device_info_version: 2,
            data: d.into()
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct MiraiDevice {
    pub display: String,
    pub product: String,
    pub device: String,
    pub board: String,
    pub brand: String,
    pub model: String,
    pub bootloader: String,
    pub fingerprint: String,
    #[serde(rename = "bootId")]
    pub boot_id: String,
    #[serde(rename = "procVersion")]
    pub proc_version: String,
    #[serde(rename = "baseBand")]
    pub base_band: String,
    pub version: OSVersion,
    #[serde(rename = "simInfo")]
    pub sim_info: String,
    #[serde(rename = "osType")]
    pub os_type: String,
    #[serde(rename = "macAddress")]
    pub mac_address: String,
    #[serde(rename = "wifiBSSID")]
    pub wifi_bssid: String,
    #[serde(rename = "wifiSSID")]
    pub wifi_ssid: String,
    #[serde(rename = "imsiMd5")]
    pub imsi_md5: String,
    pub imei: String,
    pub apn: String,
}

impl From<Device> for MiraiDevice {
    fn from(Device {
                display,
                product,
                device,
                board,
                brand,
                model,
                bootloader,
                finger_print,
                boot_id,
                proc_version,
                base_band,
                version,
                sim_info,
                os_type,
                mac_address,
                wifi_bssid,
                wifi_ssid,
                imsi_md5,
                imei,
                apn, ..
            }: Device) -> Self {
        let md5u8 = imsi_md5;
        let mut imsi_md5 = String::new();

        for byte in md5u8 {
            let str = format!("{:02x}", byte);
            imsi_md5.push_str(&str);
        }

        Self {
            display,
            product,
            device,
            board,
            brand,
            model,
            bootloader,
            fingerprint: finger_print,
            boot_id,
            proc_version,
            base_band,
            version,
            sim_info,
            os_type,
            mac_address,
            wifi_bssid,
            wifi_ssid,
            imsi_md5,
            imei,
            apn,
        }
    }
}