// SPDX-License-Identifier: GPL-3.0-only

//! The panel applet: a strip of graphs, a tooltip on each, and nothing else.
//!
//! There is no popup and no window of its own. The numbers behind a graph are
//! in its own tooltip, a click opens the task manager, and the settings live
//! in the Settings page built from
//! `data/applet-settings/fun.wmde.AppletSysmon.ron`.
//!
//! The MATE applet this follows does nothing on a click. A strip of load
//! graphs is where a person looks first when the machine drags, though, and
//! the next thing they want is the list of what is dragging it - so the click
//! is wired, and `open_task_manager` turns it off for anyone who disagrees.
//!
//! Each graph is its own widget rather than one strip over all of them,
//! because a tooltip is attached to a widget and the three graphs answer
//! three different questions - the difference between "how busy is the
//! processor" and "how long is the run queue" is exactly what a tooltip is
//! for. The gap between graphs moves from the strip to the row's spacing;
//! the pixels are the same.
//!
//! Sampling happens inline in `update`: reading five `/proc` files takes
//! microseconds, and handing that to `spawn_blocking` would cost more in
//! wakeups than the work itself.

use crate::fl;
use wmde_sysmon::graph::{self, Colors, Kind, MAX_BANDS, Metrics, Ring, Strip};
use wmde_sysmon::probe::{Probe, Sample, Sources};
use cosmic::{
    Element, Task, app,
    applet::token::subscription::{TokenRequest, TokenUpdate, activation_token_subscription},
    cctk::sctk::reexports::calloop::channel,
    cosmic_theme::Spacing,
    iced::{
        Length, Subscription,
        futures::{SinkExt, channel::mpsc},
        stream,
        widget::canvas,
    },
    surface, theme,
    widget::{autosize, button, container},
};
use cosmic_applets_config::sysmon::SysmonAppletConfig;
use cosmic_config::CosmicConfigEntry;
use std::sync::LazyLock;
use std::time::Duration;

/// What a click opens. Not a hard dependency of the package: without it
/// installed the click does nothing at all, which is the same as turning
/// `open_task_manager` off.
const TASK_MANAGER: &str = "wmde-taskmgr";

/// Bounds the settings page also declares. A width below this draws nothing
/// readable; above it the applet would eat the panel.
const MIN_WIDTH: u32 = 10;
const MAX_WIDTH: u32 = 200;
const MIN_INTERVAL: u32 = 250;
const MAX_INTERVAL: u32 = 10_000;
const MIN_HEIGHT_PERCENT: u32 = 20;
const MAX_HEIGHT_PERCENT: u32 = 100;
/// Below this a graph has no room for a border and a band at once.
const MIN_THICKNESS: f32 = 4.0;

static AUTOSIZE_MAIN_ID: LazyLock<cosmic::widget::Id> =
    LazyLock::new(|| cosmic::widget::Id::new("autosize-main"));

pub fn run() -> cosmic::iced::Result {
    cosmic::applet::run::<SysmonApplet>(())
}

struct SysmonApplet {
    core: cosmic::app::Core,
    config: SysmonAppletConfig,
    probe: Probe,
    /// One history per graph, indexed by `Kind::index`, alive whether the
    /// graph is shown or not - switching one back on costs no allocation.
    rings: [Ring; Kind::COUNT],
    /// Enabled graphs in panel order. Rebuilt only when the config changes.
    kinds: Vec<Kind>,
    colors: Colors,
    /// One per graph, indexed by `Kind::index`. Cleared once per tick; that
    /// is the only thing that invalidates them.
    ///
    /// One cache for all of them would not do: a `canvas::Cache` is keyed by
    /// the bounds it was drawn at, and neighbouring graphs sit at different
    /// coordinates, so each would throw away what the last one just stored.
    caches: [canvas::Cache; Kind::COUNT],
    last: Sample,
    /// Handed over by the activation-token subscription at start-up. Without a
    /// token the task manager opens unfocused and only marks itself urgent in
    /// the panel, which is not what a click asked for.
    token_tx: Option<channel::Sender<TokenRequest>>,
}

#[derive(Debug, Clone)]
enum Message {
    Tick,
    /// Boxed: fifteen color strings would otherwise pad every `Tick` that
    /// goes through the queue to the size of the whole config.
    ConfigChanged(Box<SysmonAppletConfig>),
    Surface(surface::Action),
    /// The applet was clicked. Opens the task manager, unless turned off.
    OpenTaskManager,
    /// The compositor answered with an activation token, or the subscription
    /// came up or went away.
    Token(TokenUpdate),
}

impl SysmonApplet {
    fn sources(&self) -> Sources {
        Sources {
            cpu: self.config.cpu,
            // A strip 20 pixels tall has no room for one graph per core, and the whole file
            // would have to be read to fill them.
            cpu_cores: false,
            memory: self.config.memory,
            // Available, committed and the slab are figures for a window to print. The
            // strip draws bands, and reading them would walk the tail of /proc/meminfo
            // every tick for nothing.
            memory_detail: false,
            network: self.config.network,
            swap: self.config.swap,
            load: self.config.load,
            disk: self.config.disk,
        }
    }

    fn width(&self) -> u32 {
        self.config.graph_width.clamp(MIN_WIDTH, MAX_WIDTH)
    }

    fn interval(&self) -> u32 {
        self.config.interval_ms.clamp(MIN_INTERVAL, MAX_INTERVAL)
    }

    /// Reads the config into the shapes the tick and the draw need, so that
    /// neither has to look at strings or re-decide what is enabled.
    fn apply_config(&mut self) {
        let sources = self.sources();
        self.kinds.clear();
        for kind in Kind::ALL {
            let on = match kind {
                Kind::Cpu => sources.cpu,
                Kind::Memory => sources.memory,
                Kind::Network => sources.network,
                Kind::Swap => sources.swap,
                Kind::Load => sources.load,
                Kind::Disk => sources.disk,
            };
            if on {
                self.kinds.push(kind);
            }
        }

        let width = self.width() as usize;
        for kind in Kind::ALL {
            self.rings[kind.index()].set_capacity(width);
        }

        self.colors = Colors::default();
        for band in graph::Band::ALL {
            self.colors.bands[band as usize] = graph::parse_color(self.config.color(band.key()));
        }
        self.colors.background = graph::parse_color(&self.config.color_background);
        self.colors.border = graph::parse_color(&self.config.color_border);

        self.clear_caches();
    }

    fn clear_caches(&self) {
        for cache in &self.caches {
            cache.clear();
        }
    }

    fn push(&mut self, sample: Sample) {
        let mut values = [0.0f32; MAX_BANDS];
        for kind in &self.kinds {
            let count = match kind {
                Kind::Cpu => {
                    values[..4].copy_from_slice(&sample.cpu);
                    4
                }
                Kind::Memory => {
                    let total = sample.mem_total.max(1) as f32;
                    for (slot, bytes) in values.iter_mut().zip(sample.mem) {
                        *slot = bytes as f32 / total;
                    }
                    3
                }
                Kind::Network => {
                    values[0] = sample.net[0] as f32;
                    values[1] = sample.net[1] as f32;
                    2
                }
                Kind::Swap => {
                    values[0] = sample.swap_used as f32 / sample.swap_total.max(1) as f32;
                    1
                }
                Kind::Load => {
                    values[0] = sample.load;
                    1
                }
                Kind::Disk => {
                    values[0] = sample.disk[0] as f32;
                    values[1] = sample.disk[1] as f32;
                    2
                }
            };
            self.rings[kind.index()].push(&values[..count]);
        }
        self.last = sample;
    }

    /// What one graph is showing: the reading, and under it what the reading
    /// means.
    ///
    /// The second line matters more than it looks. Three graphs side by side
    /// invite the reading that they are three views of one thing, and they
    /// are not: memory counts the page cache the processor graph knows
    /// nothing about, and load average counts a queue rather than any kind of
    /// busyness at all. The wording is the settings page's own, so the toggle
    /// and the graph it turns on say the same thing.
    fn tooltip_for(&self, kind: Kind) -> String {
        let s = &self.last;
        let (reading, what) = match kind {
            Kind::Cpu => {
                let busy: f32 = s.cpu.iter().sum();
                (
                    fl!("tip-cpu", value = format!("{:.0}%", busy * 100.0)),
                    fl!("what-cpu"),
                )
            }
            Kind::Memory => (
                fl!(
                    "tip-memory",
                    used = fmt_size(s.mem[0]),
                    total = fmt_size(s.mem_total)
                ),
                fl!("what-memory"),
            ),
            Kind::Network => (
                fl!(
                    "tip-network",
                    rx = fmt_rate(s.net[0]),
                    tx = fmt_rate(s.net[1])
                ),
                fl!("what-network"),
            ),
            Kind::Swap => (
                fl!(
                    "tip-swap",
                    used = fmt_size(s.swap_used),
                    total = fmt_size(s.swap_total)
                ),
                fl!("what-swap"),
            ),
            Kind::Load => (
                fl!("tip-load", value = format!("{:.2}", s.load)),
                fl!("what-load"),
            ),
            Kind::Disk => (
                fl!(
                    "tip-disk",
                    read = fmt_rate(s.disk[0]),
                    write = fmt_rate(s.disk[1])
                ),
                fl!("what-disk"),
            ),
        };
        format!("{reading}\n{what}")
    }
}

/// Extent of the strip across the panel. 100 percent is the panel's whole
/// thickness, padding included: at that setting a graph runs edge to edge.
fn graph_thickness(panel_thickness: f32, percent: u32) -> f32 {
    let percent = percent.clamp(MIN_HEIGHT_PERCENT, MAX_HEIGHT_PERCENT) as f32;
    // Not `clamp`: on a panel thinner than the floor the bounds would cross,
    // and `f32::clamp` panics when they do.
    (panel_thickness * percent / 100.0)
        .round()
        .max(MIN_THICKNESS)
        .min(panel_thickness)
}

const KIB: f64 = 1024.0;
const MIB: f64 = 1024.0 * KIB;
const GIB: f64 = 1024.0 * MIB;

/// Binary units - the ones the kernel counts memory in.
fn fmt_size(bytes: u64) -> String {
    let value = bytes as f64;
    if value >= GIB {
        fl!("size-gib", value = format!("{:.1}", value / GIB))
    } else if value >= MIB {
        fl!("size-mib", value = format!("{:.1}", value / MIB))
    } else if value >= KIB {
        fl!("size-kib", value = format!("{:.0}", value / KIB))
    } else {
        fl!("size-b", value = bytes.to_string())
    }
}

fn fmt_rate(bytes_per_second: f64) -> String {
    if bytes_per_second >= MIB {
        fl!("rate-mib", value = format!("{:.1}", bytes_per_second / MIB))
    } else if bytes_per_second >= KIB {
        fl!("rate-kib", value = format!("{:.0}", bytes_per_second / KIB))
    } else {
        fl!("rate-b", value = format!("{:.0}", bytes_per_second))
    }
}

impl cosmic::Application for SysmonApplet {
    type Message = Message;
    type Executor = cosmic::SingleThreadExecutor;
    type Flags = ();
    const APP_ID: &'static str = "fun.wmde.AppletSysmon";

    fn init(core: cosmic::app::Core, _flags: Self::Flags) -> (Self, app::Task<Self::Message>) {
        // `get_entry` reports an error for every key that is not on disk yet,
        // but the config it hands back with those errors already holds
        // everything that IS on disk. Taking `.ok()` and falling back to the
        // default throws all of it away, so the first release to add a key
        // would silently reset every setting the person had chosen.
        let config = cosmic_config::Config::new(Self::APP_ID, SysmonAppletConfig::VERSION)
            .ok()
            .map(|c| {
                SysmonAppletConfig::get_entry(&c).unwrap_or_else(|(errors, config)| {
                    for err in errors {
                        tracing::info!("sysmon applet config: {err}");
                    }
                    config
                })
            })
            .unwrap_or_default();

        let width = config.graph_width.clamp(MIN_WIDTH, MAX_WIDTH) as usize;
        let rings = Kind::ALL.map(|kind| Ring::new(kind.bands().len(), width));

        let mut applet = Self {
            core,
            config,
            probe: Probe::new(),
            rings,
            kinds: Vec::with_capacity(Kind::COUNT),
            colors: Colors::default(),
            caches: std::array::from_fn(|_| canvas::Cache::new()),
            last: Sample::default(),
            token_tx: None,
        };
        applet.apply_config();

        (applet, Task::none())
    }

    fn core(&self) -> &cosmic::app::Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut cosmic::app::Core {
        &mut self.core
    }

    fn update(&mut self, message: Self::Message) -> app::Task<Self::Message> {
        match message {
            Message::Tick => {
                let sample = self.probe.sample(self.sources());
                self.push(sample);
                self.clear_caches();
            }
            Message::ConfigChanged(config) => {
                if *config != self.config {
                    self.config = *config;
                    self.apply_config();
                }
            }
            Message::Surface(action) => {
                return cosmic::task::message(cosmic::Action::Cosmic(
                    cosmic::app::Action::Surface(action),
                ));
            }
            // Ask the compositor for an activation token first. Spawning
            // straight away opens the window behind whatever has focus.
            Message::OpenTaskManager => {
                if let Some(tx) = self.token_tx.as_ref() {
                    let _ = tx.send(TokenRequest {
                        app_id: Self::APP_ID.to_string(),
                        exec: TASK_MANAGER.to_string(),
                    });
                } else {
                    tracing::error!("no activation token channel");
                }
            }
            Message::Token(update) => match update {
                TokenUpdate::Init(tx) => self.token_tx = Some(tx),
                TokenUpdate::Finished => self.token_tx = None,
                TokenUpdate::ActivationToken { token, .. } => {
                    let mut cmd = std::process::Command::new(TASK_MANAGER);
                    if let Some(token) = token {
                        cmd.env("XDG_ACTIVATION_TOKEN", &token);
                        cmd.env("DESKTOP_STARTUP_ID", &token);
                    }
                    // A second copy does not start: it calls Present on the
                    // one already running and exits, so the click raises the
                    // window that is open rather than opening another.
                    tokio::spawn(cosmic::process::spawn(cmd));
                }
            },
        }
        Task::none()
    }

    fn view(&self) -> Element<'_, Message> {
        let horizontal = self.core.applet.is_horizontal();
        let suggested = self.core.applet.suggested_size(true);
        let (pad_major, pad_minor) = self.core.applet.suggested_padding(true);
        // Same axis mapping the applet Context uses for its own buttons.
        let (horizontal_padding, vertical_padding) = if horizontal {
            (pad_major, pad_minor)
        } else {
            (pad_minor, pad_major)
        };
        let Spacing { space_xxxs, .. } = theme::active().cosmic().spacing;
        // The panel makes itself as thick as an applet icon plus its padding
        // on both sides, so that sum is the panel's own thickness.
        let panel_thickness = if horizontal {
            f32::from(suggested.1 + 2 * vertical_padding)
        } else {
            f32::from(suggested.0 + 2 * horizontal_padding)
        };

        let content: Element<'_, Message> = if self.kinds.is_empty() {
            // Nothing to draw, but the applet still has to be findable in the
            // panel - otherwise it cannot be selected and removed. One
            // tooltip over the icon, because there is no graph to point at.
            let icon = cosmic::widget::icon::from_name("fun.wmde.AppletSysmon-symbolic")
                .size(suggested.0);
            Element::from(self.core.applet.applet_tooltip(
                Element::from(icon),
                fl!("tip-idle"),
                false,
                Message::Surface,
                None,
            ))
        } else {
            let metrics = Metrics {
                horizontal,
                length: self.width() as f32,
                thickness: graph_thickness(panel_thickness, self.config.height_percent),
                // The strip draws one graph now, so it has no gap of its own
                // to draw. The spacing below puts back the same pixels.
                gap: 0.0,
            };
            // `kinds` is in panel order, and each graph carries its own
            // tooltip. `Strip` is handed a one-element slice - the same shape
            // the task manager uses for the thumbnails in its rail.
            let graphs: Vec<Element<'_, Message>> = self.kinds.iter().map(|kind| {
                let strip = Strip::new(
                    std::slice::from_ref(kind),
                    &self.rings,
                    &self.colors,
                    &self.caches[kind.index()],
                    metrics,
                );
                Element::from(self.core.applet.applet_tooltip(
                    Element::from(strip),
                    self.tooltip_for(*kind),
                    false,
                    Message::Surface,
                    None,
                ))
            }).collect();
            if horizontal {
                cosmic::widget::row::with_children(graphs)
                    .spacing(space_xxxs)
                    .into()
            } else {
                cosmic::widget::column::with_children(graphs)
                    .spacing(space_xxxs)
                    .into()
            }
        };

        // The strip is centered across the panel and padded only along it:
        // at 100 percent it runs edge to edge, and the cross-axis padding is
        // exactly what it eats.
        let framed = if horizontal {
            container(content)
                .center_y(Length::Fixed(panel_thickness))
                .padding([0, horizontal_padding])
        } else {
            container(content)
                .center_x(Length::Fixed(panel_thickness))
                .padding([vertical_padding, 0])
        };

        // One button around the whole row rather than one per graph: the
        // hover highlight then covers the applet, the way every other panel
        // item highlights, instead of lighting one graph out of three.
        //
        // A button hands the event to its child before it looks at it itself
        // (`button/widget.rs`), so the tooltips inside still see the pointer
        // move under them.
        let clickable: Element<'_, Message> = if self.config.open_task_manager {
            button::custom(framed)
                .padding(0)
                .class(cosmic::theme::Button::AppletIcon)
                .on_press(Message::OpenTaskManager)
                .into()
        } else {
            framed.into()
        };

        autosize::autosize(clickable, AUTOSIZE_MAIN_ID.clone()).into()
    }

    fn subscription(&self) -> Subscription<Message> {
        fn tick_subscription(interval_ms: u32) -> Subscription<Message> {
            Subscription::run_with(interval_ms, |interval_ms: &u32| {
                let period = Duration::from_millis(u64::from(*interval_ms));
                stream::channel(1, move |mut output: mpsc::Sender<Message>| async move {
                    loop {
                        let _ = output.send(Message::Tick).await;
                        tokio::time::sleep(period).await;
                    }
                })
            })
        }

        let config = self.core.watch_config(Self::APP_ID).map(|update| {
            for err in update.errors {
                tracing::error!(?err, "Error watching config");
            }
            Message::ConfigChanged(Box::new(update.config))
        });

        let tokens = activation_token_subscription(0).map(Message::Token);

        // With every graph switched off there is nothing to sample, so the
        // timer is not created at all. The token channel stays either way:
        // the icon that stands in for the graphs is clickable too.
        if self.kinds.is_empty() {
            Subscription::batch([config, tokens])
        } else {
            Subscription::batch([tick_subscription(self.interval()), config, tokens])
        }
    }

    fn style(&self) -> Option<cosmic::iced::theme::Style> {
        Some(cosmic::applet::style())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Panel size S: a 20 pixel symbolic icon with 10 pixels of padding on
    /// each side.
    const PANEL_S: f32 = 40.0;

    #[test]
    fn full_height_fills_the_panel() {
        assert_eq!(graph_thickness(PANEL_S, 100), PANEL_S);
        assert_eq!(graph_thickness(PANEL_S, 80), 32.0, "the default");
        assert_eq!(graph_thickness(PANEL_S, 50), 20.0, "the icon row height");
    }

    #[test]
    fn height_stays_inside_the_panel_and_above_a_hairline() {
        assert_eq!(graph_thickness(PANEL_S, 400), PANEL_S, "clamped to 100");
        assert_eq!(graph_thickness(PANEL_S, 0), 8.0, "clamped to 20");
        // A panel thinner than the floor still gets what there is, not more.
        assert_eq!(graph_thickness(3.0, 20), 3.0);
    }
}
