// SPDX-License-Identifier: GPL-3.0-only

use cosmic_config::{self, CosmicConfigEntry, cosmic_config_derive::CosmicConfigEntry};

/// Config of the system monitor applet.
///
/// Every color is a `#RRGGBB` string, and an empty one - the default of every
/// color key - means the band takes its color from the WMDE theme. That way
/// the applet is in step with the palette out of the box and still repaintable
/// band by band, the way the MATE applet is.
#[derive(Debug, Clone, CosmicConfigEntry, PartialEq, serde::Deserialize, serde::Serialize)]
#[version = 1]
pub struct SysmonAppletConfig {
    pub interval_ms: u32,
    /// Length of one graph along the panel, in pixels. It is also the depth
    /// of the history: one sample is drawn one pixel wide.
    pub graph_width: u32,
    /// Extent of the strip across the panel, in percent of the panel's whole
    /// thickness. At 100 the graphs run from edge to edge with no padding
    /// left over.
    pub height_percent: u32,
    /// A click on the applet opens the task manager.
    ///
    /// The MATE applet this follows does nothing on a click, but a strip of
    /// load graphs is where a person looks first when the machine drags, and
    /// the next thing they want is the list of what is dragging it.
    pub open_task_manager: bool,
    pub cpu: bool,
    pub memory: bool,
    pub network: bool,
    pub swap: bool,
    pub load: bool,
    pub disk: bool,
    pub color_cpu_user: String,
    pub color_cpu_nice: String,
    pub color_cpu_system: String,
    pub color_cpu_iowait: String,
    pub color_mem_used: String,
    pub color_mem_buffers: String,
    pub color_mem_cached: String,
    pub color_net_in: String,
    pub color_net_out: String,
    pub color_swap_used: String,
    pub color_load_avg: String,
    pub color_disk_read: String,
    pub color_disk_write: String,
    pub color_background: String,
    pub color_border: String,
}

impl Default for SysmonAppletConfig {
    fn default() -> Self {
        Self {
            interval_ms: 1000,
            graph_width: 40,
            height_percent: 80,
            open_task_manager: true,
            // The two graphs MATE also shows by default; the rest are one
            // toggle away in the settings page.
            cpu: true,
            memory: true,
            network: false,
            swap: false,
            load: false,
            disk: false,
            color_cpu_user: String::new(),
            color_cpu_nice: String::new(),
            color_cpu_system: String::new(),
            color_cpu_iowait: String::new(),
            color_mem_used: String::new(),
            color_mem_buffers: String::new(),
            color_mem_cached: String::new(),
            color_net_in: String::new(),
            color_net_out: String::new(),
            color_swap_used: String::new(),
            color_load_avg: String::new(),
            color_disk_read: String::new(),
            color_disk_write: String::new(),
            color_background: String::new(),
            color_border: String::new(),
        }
    }
}

impl SysmonAppletConfig {
    /// Band color by key name. The applet asks with the same string the
    /// settings schema and the config file use, so the list of key names
    /// exists once.
    #[must_use]
    pub fn color(&self, key: &str) -> &str {
        match key {
            "color_cpu_user" => &self.color_cpu_user,
            "color_cpu_nice" => &self.color_cpu_nice,
            "color_cpu_system" => &self.color_cpu_system,
            "color_cpu_iowait" => &self.color_cpu_iowait,
            "color_mem_used" => &self.color_mem_used,
            "color_mem_buffers" => &self.color_mem_buffers,
            "color_mem_cached" => &self.color_mem_cached,
            "color_net_in" => &self.color_net_in,
            "color_net_out" => &self.color_net_out,
            "color_swap_used" => &self.color_swap_used,
            "color_load_avg" => &self.color_load_avg,
            "color_disk_read" => &self.color_disk_read,
            "color_disk_write" => &self.color_disk_write,
            _ => "",
        }
    }
}
