// SPDX-License-Identifier: GPL-3.0-only

//! History rings and the widget that draws the whole strip.
//!
//! One widget draws every enabled graph, not one widget per graph: that keeps
//! the tick down to a single geometry, a single cache and a single layer.
//! The cache lives in the application, not in the widget tree, because the
//! application is what knows a new sample arrived - see `app::Message::Tick`.
//!
//! A band is one filled polygon spanning all the columns, not a rectangle per
//! column: 4 fills instead of 160 for a 40-pixel processor graph. Every
//! coordinate is a whole pixel, because fills in this stack are not snapped
//! for us and half-pixel edges come out as hairlines.

use cosmic::iced::advanced::graphics::geometry::Renderer as _;
use cosmic::iced::advanced::widget::Tree;
use cosmic::iced::advanced::{Layout, Widget, layout, mouse, renderer};
use cosmic::iced::widget::canvas;
use cosmic::iced::{Color, Element, Length, Point, Rectangle, Size, Vector};

/// Widest graph in bands; sizes the sample slice pushed into a [`Ring`].
pub const MAX_BANDS: usize = 4;

/// One series. The order inside a [`Kind`] is the stacking order, bottom
/// first. The config carries one color key per entry here, and the theme one
/// default per entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Band {
    CpuUser,
    CpuNice,
    CpuSystem,
    CpuIowait,
    MemUsed,
    MemBuffers,
    MemCached,
    NetIn,
    NetOut,
    SwapUsed,
    LoadAvg,
    DiskRead,
    DiskWrite,
}

/// Which theme color a band takes when the config leaves it alone.
#[derive(Debug, Clone, Copy)]
enum Role {
    Accent,
    Success,
    Destructive,
    Warning,
}

impl Band {
    pub const ALL: [Band; 13] = [
        Band::CpuUser,
        Band::CpuNice,
        Band::CpuSystem,
        Band::CpuIowait,
        Band::MemUsed,
        Band::MemBuffers,
        Band::MemCached,
        Band::NetIn,
        Band::NetOut,
        Band::SwapUsed,
        Band::LoadAvg,
        Band::DiskRead,
        Band::DiskWrite,
    ];
    pub const COUNT: usize = Band::ALL.len();

    /// Config key of this band's color. Also the order of [`Band::ALL`].
    pub const fn key(self) -> &'static str {
        match self {
            Band::CpuUser => "color_cpu_user",
            Band::CpuNice => "color_cpu_nice",
            Band::CpuSystem => "color_cpu_system",
            Band::CpuIowait => "color_cpu_iowait",
            Band::MemUsed => "color_mem_used",
            Band::MemBuffers => "color_mem_buffers",
            Band::MemCached => "color_mem_cached",
            Band::NetIn => "color_net_in",
            Band::NetOut => "color_net_out",
            Band::SwapUsed => "color_swap_used",
            Band::LoadAvg => "color_load_avg",
            Band::DiskRead => "color_disk_read",
            Band::DiskWrite => "color_disk_write",
        }
    }

    const fn role(self) -> Role {
        match self {
            Band::CpuUser
            | Band::MemUsed
            | Band::NetIn
            | Band::SwapUsed
            | Band::LoadAvg
            | Band::DiskRead => Role::Accent,
            Band::CpuNice | Band::MemBuffers => Role::Success,
            Band::CpuSystem | Band::NetOut | Band::DiskWrite => Role::Destructive,
            Band::CpuIowait | Band::MemCached => Role::Warning,
        }
    }
}

/// A graph. `ALL` is also the left-to-right order in the panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Cpu,
    Memory,
    Network,
    Swap,
    Load,
    Disk,
}

impl Kind {
    pub const ALL: [Kind; 6] = [
        Kind::Cpu,
        Kind::Memory,
        Kind::Network,
        Kind::Swap,
        Kind::Load,
        Kind::Disk,
    ];
    pub const COUNT: usize = Kind::ALL.len();

    pub const fn index(self) -> usize {
        self as usize
    }

    pub const fn bands(self) -> &'static [Band] {
        match self {
            Kind::Cpu => &[
                Band::CpuUser,
                Band::CpuNice,
                Band::CpuSystem,
                Band::CpuIowait,
            ],
            Kind::Memory => &[Band::MemUsed, Band::MemBuffers, Band::MemCached],
            Kind::Network => &[Band::NetIn, Band::NetOut],
            Kind::Swap => &[Band::SwapUsed],
            Kind::Load => &[Band::LoadAvg],
            Kind::Disk => &[Band::DiskRead, Band::DiskWrite],
        }
    }

    /// `None` - the pushed values are already fractions of a known total and
    /// the graph is drawn against 1.0. `Some(floor)` - the graph auto-scales
    /// to the tallest column in view, but never to less than `floor`, so that
    /// an idle machine shows a flat line instead of amplified noise.
    const fn floor(self) -> Option<f32> {
        match self {
            Kind::Cpu | Kind::Memory | Kind::Swap => None,
            // 32 KiB/s and 1 MiB/s: below that the traffic is background
            // chatter nobody is watching for.
            Kind::Network => Some(32.0 * 1024.0),
            Kind::Disk => Some(1024.0 * 1024.0),
            Kind::Load => Some(1.0),
        }
    }
}

/// Fixed-capacity history, one column of `bands` values per sample.
///
/// Allocated once at startup and on a width change; steady state allocates
/// nothing. Capacity equals the graph's length in pixels, so one column is
/// exactly one pixel and no resampling happens at draw time.
#[derive(Debug, Default)]
pub struct Ring {
    data: Vec<f32>,
    cap: usize,
    bands: usize,
    len: usize,
    head: usize,
}

impl Ring {
    pub fn new(bands: usize, cap: usize) -> Self {
        let cap = cap.max(1);
        Self {
            data: vec![0.0; cap * bands],
            cap,
            bands,
            len: 0,
            head: 0,
        }
    }

    /// Re-cuts the history to a new width. The old samples are dropped: at a
    /// different width they would have to be resampled, and a width change is
    /// a deliberate one-off act in the settings page.
    pub fn set_capacity(&mut self, cap: usize) {
        let cap = cap.max(1);
        if cap == self.cap {
            return;
        }
        self.data.clear();
        self.data.resize(cap * self.bands, 0.0);
        self.cap = cap;
        self.len = 0;
        self.head = 0;
    }

    pub fn push(&mut self, values: &[f32]) {
        let at = self.head * self.bands;
        let column = &mut self.data[at..at + self.bands];
        for (slot, value) in column.iter_mut().zip(values) {
            *slot = *value;
        }
        // A shorter slice than the ring is built for would leave stale
        // numbers behind; there is no caller that does it, but zeroing is one
        // instruction and a silent ghost band is not worth the risk.
        for slot in column.iter_mut().skip(values.len()) {
            *slot = 0.0;
        }
        self.head = (self.head + 1) % self.cap;
        self.len = (self.len + 1).min(self.cap);
    }

    pub fn len(&self) -> usize {
        self.len
    }

    /// Column `i` counted from the oldest sample in view.
    fn column(&self, i: usize) -> &[f32] {
        let at = ((self.head + self.cap - self.len + i) % self.cap) * self.bands;
        &self.data[at..at + self.bands]
    }
}

/// Per-band color overrides read from the config. `None` means "the theme
/// decides", which is what every key ships as.
#[derive(Debug, Clone, Copy)]
pub struct Colors {
    pub bands: [Option<Color>; Band::COUNT],
    pub background: Option<Color>,
    pub border: Option<Color>,
}

impl Default for Colors {
    fn default() -> Self {
        Self {
            bands: [None; Band::COUNT],
            background: None,
            border: None,
        }
    }
}

impl Colors {
    fn band(&self, band: Band, theme: &cosmic::Theme) -> Color {
        if let Some(color) = self.bands[band as usize] {
            return color;
        }
        let palette = theme.cosmic();
        Color::from(match band.role() {
            Role::Accent => palette.accent_color(),
            Role::Success => palette.success_color(),
            Role::Destructive => palette.destructive_color(),
            Role::Warning => palette.warning_color(),
        })
    }

    fn background(&self, theme: &cosmic::Theme) -> Color {
        self.background
            .unwrap_or_else(|| Color::from(theme.cosmic().bg_component_color()))
    }

    fn border(&self, theme: &cosmic::Theme) -> Color {
        self.border
            .unwrap_or_else(|| Color::from(theme.cosmic().background(theme.transparent).divider))
    }
}

/// `#RRGGBB` or `#RRGGBBAA`, with or without the hash. Anything else - and
/// that includes the empty string every key ships with - means the theme.
pub fn parse_color(text: &str) -> Option<Color> {
    let hex = text.trim().trim_start_matches('#');
    if hex.len() != 6 && hex.len() != 8 {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    Some(Color::from_rgba8(
        byte(0)?,
        byte(2)?,
        byte(4)?,
        if hex.len() == 8 {
            f32::from(byte(6)?) / 255.0
        } else {
            1.0
        },
    ))
}

/// How the strip is laid out in the panel. Time always runs left to right
/// inside a graph; what the panel anchor changes is whether the graphs sit
/// side by side or stack.
#[derive(Debug, Clone, Copy)]
pub struct Metrics {
    pub horizontal: bool,
    /// Extent of one graph along the panel.
    pub length: f32,
    /// Extent across the panel - the icon size the panel suggests.
    pub thickness: f32,
    pub gap: f32,
}

/// The strip: every enabled graph, drawn into one geometry.
pub struct Strip<'a> {
    kinds: &'a [Kind],
    rings: &'a [Ring; Kind::COUNT],
    colors: &'a Colors,
    cache: &'a canvas::Cache,
    metrics: Metrics,
}

impl<'a> Strip<'a> {
    pub fn new(
        kinds: &'a [Kind],
        rings: &'a [Ring; Kind::COUNT],
        colors: &'a Colors,
        cache: &'a canvas::Cache,
        metrics: Metrics,
    ) -> Self {
        Self {
            kinds,
            rings,
            colors,
            cache,
            metrics,
        }
    }

    fn size(&self) -> Size {
        let count = self.kinds.len() as f32;
        let along =
            (count * self.metrics.length + (count - 1.0).max(0.0) * self.metrics.gap).max(1.0);
        if self.metrics.horizontal {
            Size::new(along, self.metrics.thickness)
        } else {
            Size::new(self.metrics.thickness, along)
        }
    }

    /// Box of graph `i`, in strip coordinates and whole pixels.
    fn slot(&self, i: usize) -> Rectangle {
        let Metrics {
            horizontal,
            length,
            thickness,
            gap,
        } = self.metrics;
        let offset = (i as f32 * (length + gap)).round();
        if horizontal {
            Rectangle::new(
                Point::new(offset, 0.0),
                Size::new(length.round(), thickness.round()),
            )
        } else {
            Rectangle::new(
                Point::new(0.0, offset),
                Size::new(thickness.round(), length.round()),
            )
        }
    }

    fn draw_graph(
        &self,
        frame: &mut canvas::Frame,
        theme: &cosmic::Theme,
        kind: Kind,
        at: Rectangle,
    ) {
        frame.fill_rectangle(at.position(), at.size(), self.colors.background(theme));

        let ring = &self.rings[kind.index()];
        let bands = kind.bands();
        let columns = ring.len().min(at.width as usize);
        if columns > 0 {
            let scale = normalizer(kind, ring, columns);
            // Newest sample at the right edge, like every strip chart.
            let left = at.x + at.width - columns as f32;
            let bottom = at.y + at.height;

            // The top edge of one band is the bottom edge of the next, so the
            // cumulative sums are recomputed per column instead of being
            // stored in a per-draw buffer.
            let column = |i: usize| ring.column(ring.len() - columns + i);
            for (b, band) in bands.iter().enumerate() {
                // A band that is flat zero across the whole window - `nice`
                // and `iowait` most of the time - is skipped outright.
                if !(0..columns).any(|i| column(i)[b] > 0.0) {
                    continue;
                }
                let height =
                    |values: &[f32], upto: usize| stack_height(values, upto, scale, at.height);
                let path = canvas::Path::new(|builder| {
                    // Top edge, left to right.
                    for i in 0..columns {
                        let top = bottom - height(column(i), b + 1);
                        let x = left + i as f32;
                        if i == 0 {
                            builder.move_to(Point::new(x, top));
                        } else {
                            builder.line_to(Point::new(x, top));
                        }
                        builder.line_to(Point::new(x + 1.0, top));
                    }
                    // Bottom edge - the top of the band below - right to left.
                    for i in (0..columns).rev() {
                        let base = bottom - height(column(i), b);
                        let x = left + i as f32;
                        builder.line_to(Point::new(x + 1.0, base));
                        builder.line_to(Point::new(x, base));
                    }
                    builder.close();
                });
                frame.fill(&path, self.colors.band(*band, theme));
            }
        }

        // Half-pixel inset so a 1px stroke lands on the pixel, not across it.
        let border = canvas::Path::rectangle(
            Point::new(at.x + 0.5, at.y + 0.5),
            Size::new(at.width - 1.0, at.height - 1.0),
        );
        frame.stroke(
            &border,
            canvas::Stroke::default()
                .with_color(self.colors.border(theme))
                .with_width(1.0),
        );
    }
}

/// Height in whole pixels of the first `upto` bands of one column, stacked
/// from the bottom. Rounding to a pixel here is what keeps band edges crisp;
/// the clamp keeps a graph whose values overshoot its scale inside its box.
fn stack_height(values: &[f32], upto: usize, scale: f32, height: f32) -> f32 {
    let sum: f32 = values[..upto].iter().sum();
    (sum * scale * height).round().clamp(0.0, height)
}

/// Multiplier that turns a stored value into a fraction of the graph height.
fn normalizer(kind: Kind, ring: &Ring, columns: usize) -> f32 {
    let Some(floor) = kind.floor() else {
        return 1.0;
    };
    let mut peak = floor;
    for i in 0..columns {
        let sum: f32 = ring.column(ring.len() - columns + i).iter().sum();
        peak = peak.max(sum);
    }
    1.0 / peak
}

impl<Message> Widget<Message, cosmic::Theme, cosmic::Renderer> for Strip<'_> {
    fn size(&self) -> Size<Length> {
        let size = Strip::size(self);
        Size::new(Length::Fixed(size.width), Length::Fixed(size.height))
    }

    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &cosmic::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let size = Strip::size(self);
        layout::Node::new(limits.resolve(
            Length::Fixed(size.width),
            Length::Fixed(size.height),
            size,
        ))
    }

    fn draw(
        &self,
        _tree: &Tree,
        renderer: &mut cosmic::Renderer,
        theme: &cosmic::Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        let geometry = self.cache.draw(renderer, bounds.size(), |frame| {
            for (i, kind) in self.kinds.iter().enumerate() {
                self.draw_graph(frame, theme, *kind, self.slot(i));
            }
        });

        use cosmic::iced::advanced::Renderer as _;
        renderer.with_translation(Vector::new(bounds.x, bounds.y), |renderer| {
            renderer.draw_geometry(geometry);
        });
    }
}

impl<'a, Message: 'a> From<Strip<'a>> for Element<'a, Message, cosmic::Theme, cosmic::Renderer> {
    fn from(strip: Strip<'a>) -> Self {
        Element::new(strip)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_keeps_the_newest_samples() {
        let mut ring = Ring::new(2, 3);
        for i in 0..5 {
            ring.push(&[i as f32, 0.0]);
        }
        assert_eq!(ring.len(), 3);
        let seen: Vec<f32> = (0..3).map(|i| ring.column(i)[0]).collect();
        assert_eq!(seen, vec![2.0, 3.0, 4.0], "oldest first, newest last");
    }

    #[test]
    fn resizing_starts_a_clean_history() {
        let mut ring = Ring::new(1, 4);
        ring.push(&[1.0]);
        ring.set_capacity(8);
        assert_eq!(ring.len(), 0);
        ring.set_capacity(8);
        assert_eq!(ring.len(), 0);
    }

    #[test]
    fn autoscale_never_drops_below_the_floor() {
        let mut ring = Ring::new(2, 4);
        ring.push(&[1024.0, 0.0]);
        // A kilobyte a second must stay a sliver, not fill the graph.
        let scale = normalizer(Kind::Network, &ring, 1);
        assert!(scale * 1024.0 < 0.04, "idle traffic amplified: {scale}");

        ring.push(&[1024.0 * 1024.0, 0.0]);
        let scale = normalizer(Kind::Network, &ring, 2);
        assert!(
            (scale * 1024.0 * 1024.0 - 1.0).abs() < 1e-6,
            "the peak must reach the top"
        );
    }

    #[test]
    fn bands_stack_from_the_bottom_in_whole_pixels() {
        // Processor column: 25% user, 0% nice, 25% system, 0% iowait on a
        // 20 pixel graph.
        let column = [0.25, 0.0, 0.25, 0.0];
        let px = |upto| stack_height(&column, upto, 1.0, 20.0);
        assert_eq!(px(0), 0.0, "the bottom band starts at the baseline");
        assert_eq!(px(1), 5.0);
        assert_eq!(px(2), 5.0, "an empty band adds no height");
        assert_eq!(px(3), 10.0);
        assert_eq!(px(4), 10.0);
    }

    #[test]
    fn a_band_never_leaves_its_box() {
        // Rounding of the fractions can push their sum a hair over 1.0.
        assert_eq!(stack_height(&[0.7, 0.7], 2, 1.0, 20.0), 20.0);
        assert_eq!(stack_height(&[-1.0], 1, 1.0, 20.0), 0.0);
    }

    #[test]
    fn the_strip_is_as_wide_as_its_graphs_and_gaps() {
        let rings = Kind::ALL.map(|kind| Ring::new(kind.bands().len(), 40));
        let colors = Colors::default();
        let cache = canvas::Cache::new();
        let kinds = [Kind::Cpu, Kind::Memory, Kind::Disk];
        let metrics = Metrics {
            horizontal: true,
            length: 40.0,
            thickness: 20.0,
            gap: 4.0,
        };

        let strip = Strip::new(&kinds, &rings, &colors, &cache, metrics);
        assert_eq!(strip.size(), Size::new(3.0 * 40.0 + 2.0 * 4.0, 20.0));
        assert_eq!(strip.slot(0).position(), Point::new(0.0, 0.0));
        assert_eq!(strip.slot(2).position(), Point::new(88.0, 0.0));

        let stacked = Strip::new(
            &kinds,
            &rings,
            &colors,
            &cache,
            Metrics {
                horizontal: false,
                ..metrics
            },
        );
        assert_eq!(stacked.size(), Size::new(20.0, 3.0 * 40.0 + 2.0 * 4.0));
        assert_eq!(stacked.slot(2).position(), Point::new(0.0, 88.0));
    }

    #[test]
    fn colors_come_from_hex_or_not_at_all() {
        assert_eq!(
            parse_color("#0078D7"),
            Some(Color::from_rgb8(0, 0x78, 0xD7))
        );
        assert_eq!(parse_color("0078d7"), Some(Color::from_rgb8(0, 0x78, 0xD7)));
        assert_eq!(parse_color(""), None);
        assert_eq!(parse_color("#12345"), None);
        assert_eq!(parse_color("#zzzzzz"), None);
        assert_eq!(
            parse_color("#0078D780").map(|c| (c.a * 255.0).round() as u8),
            Some(128)
        );
    }
}
