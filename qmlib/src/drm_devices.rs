use std::collections::HashMap;
use std::cell::RefCell;
use std::fs;
use std::path::Path;
use std::rc::{Rc, Weak};

use anyhow::{bail, Result};
use libc;
use log::{debug, info, warn};
use serde::{Deserialize, Serialize};
#[cfg(feature = "udev_backend")]
use udev;

use crate::hwmon::Hwmon;
use crate::drm_clients::{DrmClients, DrmClientInfoMap, DrmClientInfoMapRef};
use crate::drm_drivers::{self, DrmDriver};


#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum VirtFn
{
    NoVirt,
    SriovPF,
    SriovVF,
    VFIO,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DrmDeviceType
{
    Unknown,
    Integrated(VirtFn),
    Discrete(VirtFn),
}

impl DrmDeviceType
{
    pub fn is_discrete(&self) -> bool
    {
        match self {
            DrmDeviceType::Discrete(_) => true,
            _ => false
        }
    }

    pub fn is_integrated(&self) -> bool
    {
        match self {
            DrmDeviceType::Integrated(_) => true,
            _ => false
        }
    }

    pub fn to_string(&self) -> String
    {
        let mut ret = String::new();

        let sriovfn = match *self {
            DrmDeviceType::Discrete(sfn) => {
                ret.push_str("Discrete");
                sfn
            },
            DrmDeviceType::Integrated(sfn) => {
                ret.push_str("Integrated");
                sfn
            },
            DrmDeviceType::Unknown => {
                ret.push_str("Unknown");
                VirtFn::NoVirt
            }
        };

        match sriovfn {
            VirtFn::SriovPF => ret.push_str(" (PF)"),
            VirtFn::SriovVF => ret.push_str(" (VF)"),
            VirtFn::VFIO => ret.push_str(" (VFIO)"),
            _ => {}
        }

        ret
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DrmDeviceThrottleReasons
{
    pub pl1: bool,
    pub pl2: bool,
    pub pl4: bool,
    pub prochot: bool,
    pub ratl: bool,
    pub thermal: bool,
    pub vr_tdc: bool,
    pub vr_thermalert: bool,
    pub status: bool
}

impl DrmDeviceThrottleReasons
{
    pub fn new() -> DrmDeviceThrottleReasons
    {
        DrmDeviceThrottleReasons {
            pl1: false,
            pl2: false,
            pl4: false,
            prochot: false,
            ratl: false,
            thermal: false,
            vr_tdc: false,
            vr_thermalert: false,
            status: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DrmDeviceFreqLimits
{
    pub name: String,
    pub minimum: u64,
    pub efficient: u64,
    pub maximum: u64,
}

impl DrmDeviceFreqLimits
{
    pub fn new() -> DrmDeviceFreqLimits
    {
        DrmDeviceFreqLimits {
            name: String::new(),
            minimum: 0,
            efficient: 0,
            maximum: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DrmDeviceFreqs
{
    pub min_freq: u64,
    pub cur_freq: u64,
    pub act_freq: u64,
    pub max_freq: u64,
    pub throttle_reasons: DrmDeviceThrottleReasons,
}

impl DrmDeviceFreqs
{
    pub fn new() -> DrmDeviceFreqs
    {
        DrmDeviceFreqs {
            min_freq: 0,
            cur_freq: 0,
            act_freq: 0,
            max_freq: 0,
            throttle_reasons: DrmDeviceThrottleReasons::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DrmDevicePower
{
    pub gpu_cur_power: f64,
    pub pkg_cur_power: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DrmDeviceMemInfo
{
    pub smem_total: u64,
    pub smem_used: u64,
    pub vram_total: u64,
    pub vram_used: u64,
}

impl DrmDeviceMemInfo
{
    pub fn new() -> DrmDeviceMemInfo
    {
        DrmDeviceMemInfo {
            smem_total: 0,
            smem_used: 0,
            vram_total: 0,
            vram_used: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DrmDeviceTemperature
{
    pub name: String,
    pub temp: f64,
}

impl DrmDeviceTemperature
{
    pub fn from_hwmon(hwmon: &Hwmon) -> Result<Vec<DrmDeviceTemperature>>
    {
        let mut temps = Vec::new();

        let slist = hwmon.sensors("temp");
        for sensor in slist.iter() {
            if !sensor.has_item("input") {
                continue;
            }

            let name = if sensor.label.is_empty() {
                format!("{}", &sensor.stype["temp".len()..])
            } else {
                sensor.label.clone()
            };
            let temp_u64 = hwmon.read_sensor(&sensor.stype, "input")?;

            temps.push(DrmDeviceTemperature {
                name,
                temp: temp_u64 as f64 / 1000.0,
            });
        }
        temps.sort_by(|a, b| a.name.cmp(&b.name));

        Ok(temps)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DrmDeviceFan
{
    pub name: String,
    pub speed: u64,
}

impl DrmDeviceFan
{
    pub fn from_hwmon(hwmon: &Hwmon) -> Result<Vec<DrmDeviceFan>>
    {
        let mut fans = Vec::new();

        let slist = hwmon.sensors("fan");
        for sensor in slist.iter() {
            if !sensor.has_item("input") {
                continue;
            }

            let name = if sensor.label.is_empty() {
                format!("{}", &sensor.stype["fan".len()..])
            } else {
                sensor.label.clone()
            };
            let speed = hwmon.read_sensor(&sensor.stype, "input")?;

            fans.push(DrmDeviceFan { name, speed, });
        }
        fans.sort_by(|a, b| a.name.cmp(&b.name));

        Ok(fans)
    }
}

pub const DRM_DEVNODE_MAJOR: u32 = 226;

#[derive(Debug)]
#[allow(dead_code)]
pub struct DeviceNodeInfo
{
    pub devnode: String,
    pub major: u32,
    pub minor: u32,
}

impl DeviceNodeInfo
{
    fn from_major(devnode: String, devnum: u64,
        major: u32, major_str: &str) -> Result<DeviceNodeInfo>
    {
        let mj = libc::major(devnum) as u32;
        let mn = libc::minor(devnum) as u32;

        if major > 0 && mj != major {
            bail!("Expected {} major {:?} but found {:?} for {:?}",
                major_str, major, mj, devnode);
        }

        Ok(DeviceNodeInfo {
            devnode: devnode,
            major: mj,
            minor: mn,
        })
    }

    fn from_drm(devnode: String, devnum: u64) -> Result<DeviceNodeInfo>
    {
        DeviceNodeInfo::from_major(
            devnode, devnum, DRM_DEVNODE_MAJOR, "DRM")
    }

    fn from(devnode: String, devnum: u64) -> Result<DeviceNodeInfo>
    {
        DeviceNodeInfo::from_major(devnode, devnum, 0, "")
    }
}

#[derive(Debug)]
pub struct DrmDeviceInfo
{
    pub pci_dev: String,                // sysname or PCI_SLOT_NAME in udev
    pub vendor_id: String,
    pub vendor: String,
    pub device_id: String,
    pub device: String,
    pub revision: String,
    pub drv_name: String,
    pub dev_nodes: Vec<DeviceNodeInfo>,
    pub dev_type: DrmDeviceType,
    pub freq_limits: Vec<DrmDeviceFreqLimits>,
    pub freqs: Vec<DrmDeviceFreqs>,
    pub power: Option<DrmDevicePower>,
    pub mem_info: Option<DrmDeviceMemInfo>,
    engs_utilization: HashMap<String, f64>,
    pub temps: Vec<DrmDeviceTemperature>,
    pub fans: Vec<DrmDeviceFan>,
    driver: Option<Rc<RefCell<dyn DrmDriver>>>,
    drm_clis: Option<DrmClientInfoMapRef>,
}

impl Default for DrmDeviceInfo
{
    fn default() -> DrmDeviceInfo
    {
        DrmDeviceInfo {
            pci_dev: String::new(),
            vendor_id: String::new(),
            vendor: String::new(),
            device_id: String::new(),
            device: String::new(),
            revision: String::new(),
            drv_name: String::new(),
            dev_nodes: Vec::new(),
            dev_type: DrmDeviceType::Unknown,
            freq_limits: Vec::new(),
            freqs: Vec::new(),
            power: None,
            mem_info: None,
            engs_utilization: HashMap::new(),
            temps: Vec::new(),
            fans: Vec::new(),
            driver: None,
            drm_clis: None,
        }
    }
}

impl DrmDeviceInfo
{
    pub fn eng_utilization(&self, eng: &String) -> f64
    {
        if !self.engs_utilization.is_empty() {
            if self.engs_utilization.contains_key(eng) {
                return self.engs_utilization[eng];
            }
        }

        0.0
    }

    pub fn engines(&self) -> Vec<String>
    {
        let mut engs = Vec::new();

        if !self.engs_utilization.is_empty() {
            engs = self.engs_utilization.keys()
                .map(|nm| nm.clone())
                .collect();
            engs.sort();
        }

        engs
    }

    pub fn clients(&self) -> Option<Weak<RefCell<DrmClientInfoMap>>>
    {
        if let Some(vref) = &self.drm_clis {
            return Some(Rc::downgrade(&vref));
        }

        None
    }

    pub fn refresh(&mut self) -> Result<()>
    {
        // reset engines usage data
        self.engs_utilization.drain();

        // update usage data from specific driver
        if let Some(drv_ref) = &self.driver {
            let mut drv_b = drv_ref.borrow_mut();

            // note: dev_type and freq_limits don't change

            self.freqs = drv_b.freqs()?;
            self.power = drv_b.power()?;
            self.mem_info = drv_b.mem_info()?;
            self.engs_utilization = drv_b.engs_utilization()?;
            self.temps = drv_b.temps()?;
            self.fans = drv_b.fans()?;
        }

        Ok(())
    }

    fn update_engs_utilization(&mut self)
    {
        // if no driver was found or it doesn't provide engines usage data,
        // fall back to adding up utilization from DRM clients list
        if self.engs_utilization.is_empty() {
            if let Some(vref) = &self.drm_clis {
                let clis_b = vref.borrow();

                for cli in clis_b.values() {
                    for en in cli.engines() {
                        let ut = cli.eng_utilization(en);
                        self.engs_utilization
                            .entry(en.clone())
                            .and_modify(|tot| *tot += ut)
                            .or_insert(ut);
                    }
                }

                for (en, tot) in self.engs_utilization.iter_mut() {
                    if *tot > 100.0 {
                        warn!("{}: engine {:?} utilization at {:?}, clamped to 100%.",
                            &self.pci_dev, en, tot);
                        *tot = 100.0;
                    }
                }
            }
        }
    }

    pub fn has_driver(&self) -> bool
    {
        self.driver.is_some()
    }

    fn is_drm_vfio(&self) -> bool
    {
        if self.dev_nodes.is_empty() {
            return false;
        }

        let dn = Path::new(&self.dev_nodes[0].devnode)
            .file_name().unwrap()
            .to_str().unwrap();
        let dev_path = Path::new("/sys/class/vfio-dev")
            .join(dn)
            .join("device");
        let physfn_path = dev_path
            .join("physfn");
        let drm_path = physfn_path
            .join("drm");

        dev_path.is_symlink() && physfn_path.is_symlink() && drm_path.is_dir()
    }
}

#[derive(Debug)]
pub struct DrmDevices
{
    infos: HashMap<String, DrmDeviceInfo>,
    qmclis: Option<DrmClients>,
}

impl DrmDevices
{
    pub fn device_info(&self, dev: &String) -> Option<&DrmDeviceInfo>
    {
        self.infos.get(dev)
    }

    pub fn devices(&self) -> Vec<&String>
    {
        let mut res: Vec<&String> = self.infos.keys().collect::<Vec<&_>>();
        res.sort();

        res
    }

    pub fn is_empty(&self) -> bool
    {
        self.infos.is_empty()
    }

    pub fn refresh(&mut self) -> Result<()>
    {
        // assumes devices don't vanish, so just update their driver-specific
        // dynamic information (e.g. mem info, engines, freqs, power, ...)
        for di in self.infos.values_mut() {
            di.refresh()?;
        }

        // update DRM clients information (if possible)
        if let Some(clis) = &mut self.qmclis {
            clis.refresh()?;

            for di in self.infos.values_mut() {
                di.drm_clis = clis.device_clients(&di.pci_dev);
                if let Some(drv_ref) = &di.driver {
                    let drv_wref = Rc::downgrade(drv_ref);
                    clis.set_dev_clients_driver(&di.pci_dev, drv_wref);
                }
            }
        }

        // check & update engs utilization from DRM clients if needed
        for di in self.infos.values_mut() {
            di.update_engs_utilization();
        }

        debug!("DRM Devices: {:#?}", self.infos);

        Ok(())
    }

    pub fn set_clients_pid_tree(&mut self, at_pid: &str) -> Result<()>
    {
        self.qmclis = Some(DrmClients::from_pid_tree(at_pid)?);

        Ok(())
    }

    fn new() -> DrmDevices
    {
        DrmDevices {
            infos: HashMap::new(),
            qmclis: None,
        }
    }

    fn vendor_name(vendor_id: &String) -> String
    {
        // Intel is the primary vendor we care about
        if vendor_id == "8086" {
            return "Intel Corporation".to_string();
        }

        #[cfg(feature = "udev_backend")]
        if let Ok(hwdb) = udev::Hwdb::new() {
            let id = u32::from_str_radix(vendor_id, 16).unwrap();
            let modalias = format!("pci:v{:08X}*", id);

            if let Some(res) = hwdb.query_one(modalias,
                "ID_VENDOR_FROM_DATABASE".to_string()) {
                return res.to_str().unwrap().to_string();
            }
        }

        vendor_id.clone()
    }

    fn device_name(vendor_id: &String, device_id: &String) -> String
    {
        // For Intel devices, try our built-in PCI ID table first
        if vendor_id == "8086" {
            if let Ok(did) = u16::from_str_radix(device_id, 16) {
                if let Some(name) = crate::intel_pci_ids::intel_device_name(did) {
                    return name.to_string();
                }
            }
        }

        #[cfg(feature = "udev_backend")]
        if let Ok(hwdb) = udev::Hwdb::new() {
            let vid = u32::from_str_radix(vendor_id, 16).unwrap();
            let did = u32::from_str_radix(device_id, 16).unwrap();
            let modalias = format!("pci:v{:08X}d{:08X}*", vid, did);

            if let Some(res) = hwdb.query_one(modalias,
                "ID_MODEL_FROM_DATABASE".to_string()) {
                return res.to_str().unwrap().to_string();
            }
        }

        device_id.clone()
    }

    #[cfg(feature = "udev_backend")]
    fn devices_from_udev(
        dev_slots: &Vec<&str>,
        mut udev_enum: udev::Enumerator,
        devnodeinfo_from: fn(String, u64) -> Result<DeviceNodeInfo>
    ) -> Result<HashMap<String, DrmDeviceInfo>>
    {
        let mut devs = HashMap::new();

        for d in udev_enum.scan_devices()? {
            let pdev = d.parent().unwrap();
            let sysname = pdev.sysname().to_str().unwrap().to_string();

            if !dev_slots.is_empty() &&
                !dev_slots.iter().any(|&ds| ds == sysname) {
                continue;
            }

            let vendor_id: String;
            let vendor: String;
            let device_id: String;
            let device: String;
            let revision: String;

            if !devs.contains_key(&sysname) {
                if let Some(pciid) = pdev.property_value("PCI_ID") {
                    let pciid = pciid.to_str().unwrap();

                    vendor_id = String::from(&pciid[0..4]);
                    vendor = DrmDevices::vendor_name(&vendor_id);

                    device_id = String::from(&pciid[5..9]);
                    device = DrmDevices::device_name(&vendor_id, &device_id);

                    let rev_str = pdev.attribute_value("revision")
                        .unwrap().to_str().unwrap();
                    revision = if rev_str.starts_with("0x") {
                        String::from(&rev_str[2..])
                    } else {
                        String::from(rev_str)
                    };
                } else {
                    // not a PCI device
                    vendor_id = String::new();
                    vendor = String::new();
                    device_id = String::new();
                    device = String::new();
                    revision = String::new();
                };

                let drv_name = pdev.driver().unwrap()
                    .to_str().unwrap().to_string();

                let ndinf = DrmDeviceInfo {
                    pci_dev: sysname.clone(),
                    vendor_id,
                    vendor,
                    device_id,
                    device,
                    revision,
                    drv_name,
                    ..Default::default()
                };
                devs.insert(sysname.clone(), ndinf);
            }

            let devnode = d.devnode().unwrap().to_str().unwrap().to_string();
            let devnum = d.devnum().unwrap();
            let minf = devnodeinfo_from(devnode, devnum)?;

            let dinf = devs.get_mut(&sysname).unwrap();
            dinf.dev_nodes.push(minf);
        }

        Ok(devs)
    }

    /// Scan DRM devices directly from sysfs without udev.
    /// This works on Android and other systems where udev is not available.
    #[cfg(not(feature = "udev_backend"))]
    fn devices_from_sysfs(
        dev_slots: &Vec<&str>,
    ) -> Result<HashMap<String, DrmDeviceInfo>>
    {
        let mut devs: HashMap<String, DrmDeviceInfo> = HashMap::new();
        let drm_class = Path::new("/sys/class/drm");

        if !drm_class.is_dir() {
            bail!("No /sys/class/drm directory found");
        }

        let entries = fs::read_dir(drm_class)?;
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name().to_str().unwrap().to_string();

            // Only process card* and renderD* entries that are symlinks
            if !name.starts_with("card") && !name.starts_with("renderD") {
                continue;
            }
            // Skip card0-boot etc, only want card0, card1, renderD128 etc
            if name.starts_with("card") && name.contains('-') {
                continue;
            }

            let dev_link = drm_class.join(&name).join("device");
            if !dev_link.is_symlink() {
                continue;
            }

            // Resolve the PCI device sysname
            let sysname = fs::read_link(&dev_link)?
                .file_name().unwrap()
                .to_str().unwrap().to_string();

            if !dev_slots.is_empty() &&
                !dev_slots.iter().any(|&ds| ds == sysname) {
                continue;
            }

            // Find the devnode path
            let devnode = format!("/dev/dri/{}", name);
            if !Path::new(&devnode).exists() {
                debug!("Device node {} does not exist, skipping", &devnode);
                continue;
            }

            if !devs.contains_key(&sysname) {
                let pci_dev_path = dev_link.canonicalize()?;

                // Read vendor and device IDs from sysfs
                let vendor_id = fs::read_to_string(pci_dev_path.join("vendor"))?
                    .trim().trim_start_matches("0x").to_lowercase();
                let device_id = fs::read_to_string(pci_dev_path.join("device"))?
                    .trim().trim_start_matches("0x").to_lowercase();
                let revision = fs::read_to_string(pci_dev_path.join("revision"))
                    .unwrap_or_default()
                    .trim().trim_start_matches("0x").to_string();

                let vendor = DrmDevices::vendor_name(&vendor_id);
                let device = DrmDevices::device_name(&vendor_id, &device_id);

                // Read driver name from the driver symlink
                let drv_link = pci_dev_path.join("driver");
                let drv_name = if drv_link.is_symlink() {
                    fs::read_link(&drv_link)?
                        .file_name().unwrap()
                        .to_str().unwrap().to_string()
                } else {
                    String::new()
                };

                let ndinf = DrmDeviceInfo {
                    pci_dev: sysname.clone(),
                    vendor_id,
                    vendor,
                    device_id,
                    device,
                    revision,
                    drv_name,
                    ..Default::default()
                };
                devs.insert(sysname.clone(), ndinf);
            }

            // Get device number from the dev file in sysfs
            let dev_file = drm_class.join(&name).join("dev");
            if let Ok(dev_str) = fs::read_to_string(&dev_file) {
                let parts: Vec<&str> = dev_str.trim().split(':').collect();
                if parts.len() == 2 {
                    let major: u32 = parts[0].parse().unwrap_or(0);
                    let minor: u32 = parts[1].parse().unwrap_or(0);
                    let devnum = libc::makedev(major, minor);
                    if let Ok(minf) = DeviceNodeInfo::from_drm(devnode, devnum) {
                        let dinf = devs.get_mut(&sysname).unwrap();
                        dinf.dev_nodes.push(minf);
                    }
                }
            }
        }

        Ok(devs)
    }

    #[cfg(not(feature = "udev_backend"))]
    fn vfio_devices_from_sysfs(
        dev_slots: &Vec<&str>,
    ) -> Result<HashMap<String, DrmDeviceInfo>>
    {
        let mut devs: HashMap<String, DrmDeviceInfo> = HashMap::new();
        let vfio_class = Path::new("/sys/class/vfio-dev");

        if !vfio_class.is_dir() {
            return Ok(devs);
        }

        let entries = fs::read_dir(vfio_class)?;
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name().to_str().unwrap().to_string();

            let dev_link = vfio_class.join(&name).join("device");
            if !dev_link.is_symlink() {
                continue;
            }

            let sysname = fs::read_link(&dev_link)?
                .file_name().unwrap()
                .to_str().unwrap().to_string();

            if !dev_slots.is_empty() &&
                !dev_slots.iter().any(|&ds| ds == sysname) {
                continue;
            }

            let devnode = format!("/dev/vfio/devices/{}", name);
            if !Path::new(&devnode).exists() {
                continue;
            }

            if !devs.contains_key(&sysname) {
                let pci_dev_path = dev_link.canonicalize()?;

                let vendor_id = fs::read_to_string(pci_dev_path.join("vendor"))?
                    .trim().trim_start_matches("0x").to_lowercase();
                let device_id = fs::read_to_string(pci_dev_path.join("device"))?
                    .trim().trim_start_matches("0x").to_lowercase();
                let revision = fs::read_to_string(pci_dev_path.join("revision"))
                    .unwrap_or_default()
                    .trim().trim_start_matches("0x").to_string();

                let vendor = DrmDevices::vendor_name(&vendor_id);
                let device = DrmDevices::device_name(&vendor_id, &device_id);

                let drv_link = pci_dev_path.join("driver");
                let drv_name = if drv_link.is_symlink() {
                    fs::read_link(&drv_link)?
                        .file_name().unwrap()
                        .to_str().unwrap().to_string()
                } else {
                    String::new()
                };

                let ndinf = DrmDeviceInfo {
                    pci_dev: sysname.clone(),
                    vendor_id,
                    vendor,
                    device_id,
                    device,
                    revision,
                    drv_name,
                    ..Default::default()
                };
                devs.insert(sysname.clone(), ndinf);
            }

            let dev_file = vfio_class.join(&name).join("dev");
            if let Ok(dev_str) = fs::read_to_string(&dev_file) {
                let parts: Vec<&str> = dev_str.trim().split(':').collect();
                if parts.len() == 2 {
                    let major: u32 = parts[0].parse().unwrap_or(0);
                    let minor: u32 = parts[1].parse().unwrap_or(0);
                    let devnum = libc::makedev(major, minor);
                    if let Ok(minf) = DeviceNodeInfo::from(devnode, devnum) {
                        let dinf = devs.get_mut(&sysname).unwrap();
                        dinf.dev_nodes.push(minf);
                    }
                }
            }
        }

        Ok(devs)
    }

    #[cfg(feature = "udev_backend")]
    pub fn find_devices(dev_slots: &Vec<&str>,
        drv_opts: &HashMap<&str, Vec<&str>>) -> Result<DrmDevices>
    {
        let mut qmds = DrmDevices::new();

        // find DRM devices
        let mut drm_enum = udev::Enumerator::new()?;
        drm_enum.match_subsystem("drm")?;
        drm_enum.match_property("DEVNAME", "/dev/dri/*")?;

        qmds.infos = DrmDevices::devices_from_udev(
            dev_slots, drm_enum, DeviceNodeInfo::from_drm)?;

        // find VFIO devices
        let mut vfio_enum = udev::Enumerator::new()?;
        vfio_enum.match_subsystem("vfio-dev")?;
        vfio_enum.match_property("DEVNAME", "/dev/vfio/devices/*")?;

        let vfio_devs = DrmDevices::devices_from_udev(
            dev_slots, vfio_enum, DeviceNodeInfo::from)?;

        for (dname, dinfo) in vfio_devs.into_iter() {
            if qmds.infos.contains_key(&dname) {
                warn!("Found {:?} on both DRM and VFIO, ignoring VFIO.",
                    &dname);
                continue;
            }
            if !dinfo.is_drm_vfio() {
                debug!("INF: VFIO device {:?} not for DRM physfn, ignoring.",
                    &dname);
                continue;
            }
            qmds.infos.insert(dname, dinfo);
        }

        // initialize drivers and log devices found
        for dinf in qmds.infos.values_mut() {
            let dopts = drv_opts.get(dinf.drv_name.as_str());
            if let Some(drv_ref) = drm_drivers::driver_from(dinf, dopts)? {
                let dref = drv_ref.clone();
                let mut drv_b = dref.borrow_mut();

                dinf.dev_type = drv_b.dev_type()?;
                dinf.freq_limits = drv_b.freq_limits()?;
                dinf.driver = Some(drv_ref);
            }
            info!(
                "New device: pci_dev={}, vendor_id={}, vendor={:?}, \
                device_id={}, device={:?}, revision={}, drv_name={}, \
                dev_type={:?}, dev_nodes={:?}",
                &dinf.pci_dev, &dinf.vendor_id, &dinf.vendor,
                &dinf.device_id, &dinf.device, &dinf.revision,
                &dinf.drv_name, &dinf.dev_type, &dinf.dev_nodes
            );
        }

        Ok(qmds)
    }

    #[cfg(not(feature = "udev_backend"))]
    pub fn find_devices(dev_slots: &Vec<&str>,
        drv_opts: &HashMap<&str, Vec<&str>>) -> Result<DrmDevices>
    {
        let mut qmds = DrmDevices::new();

        // find DRM devices via sysfs
        qmds.infos = DrmDevices::devices_from_sysfs(dev_slots)?;

        // find VFIO devices via sysfs
        let vfio_devs = DrmDevices::vfio_devices_from_sysfs(dev_slots)?;

        for (dname, dinfo) in vfio_devs.into_iter() {
            if qmds.infos.contains_key(&dname) {
                warn!("Found {:?} on both DRM and VFIO, ignoring VFIO.",
                    &dname);
                continue;
            }
            if !dinfo.is_drm_vfio() {
                debug!("INF: VFIO device {:?} not for DRM physfn, ignoring.",
                    &dname);
                continue;
            }
            qmds.infos.insert(dname, dinfo);
        }

        // initialize drivers and log devices found
        for dinf in qmds.infos.values_mut() {
            let dopts = drv_opts.get(dinf.drv_name.as_str());
            if let Some(drv_ref) = drm_drivers::driver_from(dinf, dopts)? {
                let dref = drv_ref.clone();
                let mut drv_b = dref.borrow_mut();

                dinf.dev_type = drv_b.dev_type()?;
                dinf.freq_limits = drv_b.freq_limits()?;
                dinf.driver = Some(drv_ref);
            }
            info!(
                "New device: pci_dev={}, vendor_id={}, vendor={:?}, \
                device_id={}, device={:?}, revision={}, drv_name={}, \
                dev_type={:?}, dev_nodes={:?}",
                &dinf.pci_dev, &dinf.vendor_id, &dinf.vendor,
                &dinf.device_id, &dinf.device, &dinf.revision,
                &dinf.drv_name, &dinf.dev_type, &dinf.dev_nodes
            );
        }

        Ok(qmds)
    }
}

pub fn sysname_from_drm_minor(mn: u32) -> Result<String>
{
    let card = format!("/sys/class/drm/card{}", mn);
    let render = format!("/sys/class/drm/renderD{}", mn);

    let lnk = if Path::new(&card).is_symlink() {
        &card
    } else if Path::new(&render).is_symlink() {
        &render
    } else {
        bail!("No device for DRM minor {:?} in /sys/class/drm", mn);
    };

    let sysname = fs::read_link(Path::new(lnk).join("device"))?
        .file_name().unwrap()
        .to_str().unwrap().to_string();

    Ok(sysname)
}
