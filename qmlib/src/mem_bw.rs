use std::mem;
use std::time::Instant;

use anyhow::Result;
use log::{debug, info};
use serde::{Deserialize, Serialize};

use crate::perf_event::{perf_event_attr, PerfEvent};


/// Memory bandwidth reading in GB/s
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemBandwidth
{
    pub read_gbs: f64,
    pub write_gbs: f64,
}

/// One IMC (Integrated Memory Controller) free-running counter instance.
#[derive(Debug)]
struct ImcCounter
{
    pf_evt_read: PerfEvent,
    pf_evt_write: PerfEvent,
    scale: f64,            // MiB per raw count
    last_read: u64,
    last_write: u64,
}

impl ImcCounter
{
    fn open(src: &str) -> Result<ImcCounter>
    {
        let mut pf_read = PerfEvent::from_pmu(src)?;
        let mut pf_write = PerfEvent::from_pmu(src)?;

        let type_: u32 = pf_read.source_type()?;
        let scale: f64 = pf_read.event_scale("data_read")?;

        // build config for data_read: event=0xff, umask=0x20
        let read_cfgs = pf_read.event_keys_config(
            "data_read", &vec!["event", "umask"])?;
        let write_cfgs = pf_write.event_keys_config(
            "data_write", &vec!["event", "umask"])?;

        let read_config = pf_read.format_config(
            &vec![("event", read_cfgs["event"]), ("umask", read_cfgs["umask"])],
            0)?;
        let write_config = pf_write.format_config(
            &vec![("event", write_cfgs["event"]), ("umask", write_cfgs["umask"])],
            0)?;

        // cpumask tells us which CPU to bind to
        let cpu: i32 = 0;  // uncore PMUs typically bind to cpu 0

        let mut pf_attr = perf_event_attr::new();
        pf_attr.type_ = type_;
        pf_attr.size = mem::size_of::<perf_event_attr>() as u32;
        pf_attr.config = read_config;
        pf_read.open(&pf_attr, -1, cpu, 0)?;

        let mut pf_attr_w = perf_event_attr::new();
        pf_attr_w.type_ = type_;
        pf_attr_w.size = mem::size_of::<perf_event_attr>() as u32;
        pf_attr_w.config = write_config;
        pf_write.open(&pf_attr_w, -1, cpu, 0)?;

        // initial read to get baseline
        let vals_r = pf_read.read(1)?;
        let vals_w = pf_write.read(1)?;

        Ok(ImcCounter {
            pf_evt_read: pf_read,
            pf_evt_write: pf_write,
            scale,
            last_read: vals_r[0],
            last_write: vals_w[0],
        })
    }

    fn read_deltas(&mut self) -> Result<(u64, u64)>
    {
        let vals_r = self.pf_evt_read.read(1)?;
        let vals_w = self.pf_evt_write.read(1)?;

        let cur_read = vals_r[0];
        let cur_write = vals_w[0];

        let delta_read = cur_read.wrapping_sub(self.last_read);
        let delta_write = cur_write.wrapping_sub(self.last_write);

        self.last_read = cur_read;
        self.last_write = cur_write;

        Ok((delta_read, delta_write))
    }
}

/// System-level memory bandwidth monitor using uncore_imc_free_running PMUs.
#[derive(Debug)]
pub struct MemBwMonitor
{
    counters: Vec<ImcCounter>,
    scale: f64,
    last_time: Instant,
    pub bandwidth: MemBandwidth,
}

impl MemBwMonitor
{
    pub fn refresh(&mut self)
    {
        let now = Instant::now();
        let elapsed_s = now.duration_since(self.last_time).as_secs_f64();

        if elapsed_s <= 0.0 {
            self.last_time = now;
            return;
        }

        let mut total_read: u64 = 0;
        let mut total_write: u64 = 0;

        for ctr in self.counters.iter_mut() {
            match ctr.read_deltas() {
                Ok((dr, dw)) => {
                    total_read += dr;
                    total_write += dw;
                }
                Err(e) => {
                    debug!("MEM BW: failed to read IMC counter: {:?}", e);
                }
            }
        }

        // scale: raw_count * scale = MiB, then MiB / elapsed_s / 1024 = GB/s
        let read_mib = total_read as f64 * self.scale;
        let write_mib = total_write as f64 * self.scale;

        self.bandwidth.read_gbs = read_mib / 1024.0 / elapsed_s;
        self.bandwidth.write_gbs = write_mib / 1024.0 / elapsed_s;

        self.last_time = now;
    }
}

const IMC_FREE_RUNNING_PREFIX: &str = "uncore_imc_free_running_";

/// Discover and open all uncore_imc_free_running PMUs for memory bandwidth monitoring.
/// Returns None if perf events are not available or no IMC PMUs found.
pub fn find_mem_bw_monitor() -> Option<MemBwMonitor>
{
    if !PerfEvent::is_capable() {
        debug!("MEM BW: perf event not capable, skipping memory bandwidth.");
        return None;
    }

    let mut counters = Vec::new();
    let mut idx = 0u32;

    loop {
        let src = format!("{}{}", IMC_FREE_RUNNING_PREFIX, idx);
        if !PerfEvent::has_source(&src) {
            break;
        }

        match ImcCounter::open(&src) {
            Ok(ctr) => {
                info!("MEM BW: opened {:?} for memory bandwidth monitoring.", src);
                counters.push(ctr);
            }
            Err(e) => {
                debug!("MEM BW: failed to open {:?}: {:?}", src, e);
                break;
            }
        }

        idx += 1;
    }

    if counters.is_empty() {
        debug!("MEM BW: no uncore_imc_free_running PMUs found.");
        return None;
    }

    let scale = counters[0].scale;
    info!("MEM BW: monitoring {} IMC controllers, scale={} MiB/count.",
        counters.len(), scale);

    Some(MemBwMonitor {
        counters,
        scale,
        last_time: Instant::now(),
        bandwidth: MemBandwidth::default(),
    })
}
