// SPDX-License-Identifier: GPL-3.0-only

//! Panel button and forecast popup.
//!
//! The popup layout follows the Nimbus "Modern" forecast sheet: a plain
//! five-column table (day, condition glyph, temperature range, wind,
//! precipitation) under a header row that differs from the data only by
//! weight and ink, a 1px rule below the header, hairlines between rows.
//! Colors, fonts, spacing and corner radius all come from the WMDE theme.

use crate::fl;
use crate::weather::{self, CurrentWeather, DayForecast};
use cosmic::{
    Element, Task, app,
    applet::{
        cosmic_panel_config::PanelAnchor,
        menu_button, padded_control,
        token::subscription::{TokenRequest, TokenUpdate, activation_token_subscription},
    },
    cctk::sctk::reexports::calloop,
    cosmic_theme::Spacing,
    iced::{
        Alignment, Background, Color, Length, Limits, Subscription,
        alignment::{Horizontal, Vertical},
        futures::{SinkExt, channel::mpsc},
        stream,
        widget::{Container, Space, column, row},
        window,
    },
    surface, theme,
    widget::{Column, autosize, button, divider, layer_container, text},
};
use cosmic_applets_config::weather::{WeatherAppletConfig, WindUnit};
use cosmic_config::CosmicConfigEntry;
use icu::{
    datetime::{
        DateTimeFormatter, DateTimeFormatterPreferences, fieldsets,
        input::{Date as IcuDate, DateTime, Time},
    },
    locale::Locale,
};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

pub const WEATHER_FONT_BYTES: &[u8] = include_bytes!("../data/fonts/weathericons.ttf");
const WEATHER_FONT: cosmic::iced::Font = cosmic::iced::Font::with_name("Weather Icons");

/// Fixed popup width; the Nimbus sheet is 620px, compacted here because the
/// day column carries a weekday instead of a full ISO date.
const POPUP_WIDTH: f32 = 460.0;
/// Column widths of the forecast table; gaps and padding are theme spacing
/// tokens, and columns + 4 gaps + side padding must stay under the width.
const DAY_W: f32 = 60.0;
const COND_W: f32 = 40.0;
const TEMP_W: f32 = 108.0;
const WIND_W: f32 = 84.0;
const PRECIP_W: f32 = 68.0;
/// Condition glyph size in the table; the theme has no token for this.
const GLYPH_SIZE: f32 = 20.0;
/// The daily forecast is re-fetched when the popup opens and the cache is
/// older than this.
const DAILY_CACHE: Duration = Duration::from_secs(600);

static AUTOSIZE_MAIN_ID: LazyLock<cosmic::widget::Id> =
    LazyLock::new(|| cosmic::widget::Id::new("autosize-main"));
static POPUP_AUTOSIZE_ID: LazyLock<cosmic::widget::Id> =
    LazyLock::new(|| cosmic::widget::Id::new("weather-popup-autosize"));

pub fn run() -> cosmic::iced::Result {
    cosmic::applet::run::<WeatherApplet>(())
}

fn get_system_locale() -> Locale {
    for var in ["LC_TIME", "LC_ALL", "LANG"] {
        if let Ok(locale_str) = std::env::var(var) {
            let cleaned_locale = locale_str
                .split('.')
                .next()
                .unwrap_or(&locale_str)
                .replace('_', "-");

            if let Ok(locale) = Locale::try_from_str(&cleaned_locale) {
                return locale;
            }

            if let Some(lang) = cleaned_locale.split('-').next() {
                if let Ok(locale) = Locale::try_from_str(lang) {
                    return locale;
                }
            }
        }
    }
    tracing::warn!("No valid locale found in environment, using fallback");
    Locale::try_from_str("en-US").expect("Failed to parse fallback locale 'en-US'")
}

pub fn condition_text(code: i64) -> String {
    match code {
        0 => fl!("cond-clear"),
        1..=3 => fl!("cond-partly-cloudy"),
        45 | 48 => fl!("cond-fog"),
        51..=55 => fl!("cond-drizzle"),
        61..=65 => fl!("cond-rain"),
        71..=77 => fl!("cond-snow"),
        80..=82 => fl!("cond-rain-showers"),
        85..=86 => fl!("cond-snow-showers"),
        c if c >= 95 => fl!("cond-thunderstorm"),
        _ => fl!("cond-unknown"),
    }
}

struct WeatherApplet {
    core: cosmic::app::Core,
    popup: Option<window::Id>,
    config: WeatherAppletConfig,
    current: Option<CurrentWeather>,
    daily: Option<Vec<DayForecast>>,
    daily_stamp: Option<Instant>,
    daily_coords: (f64, f64),
    daily_failed: bool,
    token_tx: Option<calloop::channel::Sender<TokenRequest>>,
    locale: Locale,
}

#[derive(Debug, Clone)]
enum Message {
    TogglePopup,
    CloseRequested(window::Id),
    Tick,
    CurrentFetched(Option<CurrentWeather>),
    DailyFetched(Option<Vec<DayForecast>>),
    ConfigChanged(WeatherAppletConfig),
    OpenSettings,
    Token(TokenUpdate),
    Surface(surface::Action<Message>),
}

impl WeatherApplet {
    fn fetch_current_task(&self) -> app::Task<Message> {
        let lat = self.config.latitude;
        let lon = self.config.longitude;
        cosmic::task::future(async move {
            Message::CurrentFetched(weather::fetch_current(lat, lon).await)
        })
    }

    fn fetch_daily_task(&self) -> app::Task<Message> {
        let lat = self.config.latitude;
        let lon = self.config.longitude;
        cosmic::task::future(async move {
            Message::DailyFetched(weather::fetch_daily(lat, lon).await)
        })
    }

    fn daily_is_stale(&self) -> bool {
        self.daily.is_none()
            || self.daily_coords != (self.config.latitude, self.config.longitude)
            || self.daily_stamp.is_none_or(|t| t.elapsed() > DAILY_CACHE)
    }

    fn wind_label(&self) -> String {
        match self.config.wind_unit {
            WindUnit::MetersPerSecond => fl!("unit-ms"),
            WindUnit::KilometersPerHour => fl!("unit-kmh"),
        }
    }

    fn pressure_label(&self) -> String {
        match self.config.pressure_unit {
            cosmic_applets_config::weather::PressureUnit::Hectopascal => fl!("unit-hpa"),
            cosmic_applets_config::weather::PressureUnit::MillimetersOfMercury => {
                fl!("unit-mmhg")
            }
            cosmic_applets_config::weather::PressureUnit::InchesOfMercury => fl!("unit-inhg"),
        }
    }

    /// Tray-tooltip line ported from Nimbus: temperature, condition,
    /// feels-like, humidity, wind, pressure.
    fn tooltip_text(&self) -> String {
        let Some(c) = self.current.as_ref() else {
            return fl!("loading");
        };
        let unit = self.config.unit;
        let unit_label = weather::temp_unit_label(unit);
        format!(
            "{:.0}{unit_label} | {} | {} {:.0}{unit_label} | \u{1f4a7}{:.0}% | \u{1f4a8}{} | {}",
            weather::temp_to_unit(c.temperature_c, unit),
            condition_text(c.weather_code),
            fl!("feels-like"),
            weather::temp_to_unit(c.feels_like_c, unit),
            c.humidity,
            weather::format_wind(c.wind_kmh, self.config.wind_unit, &self.wind_label()),
            weather::format_pressure(
                c.pressure_hpa,
                self.config.pressure_unit,
                &self.pressure_label()
            ),
        )
    }

    /// "Mon 28" from an ISO date, weekday localized via icu and capitalized.
    fn day_label(&self, iso_date: &str, formatter: Option<&DateTimeFormatter<fieldsets::E>>) -> String {
        let mut parts = iso_date.split('-');
        let (Some(y), Some(m), Some(d)) = (parts.next(), parts.next(), parts.next()) else {
            return iso_date.to_owned();
        };
        let (Ok(y), Ok(m), Ok(d)) = (y.parse::<i32>(), m.parse::<u8>(), d.parse::<u8>()) else {
            return iso_date.to_owned();
        };
        let Some(formatter) = formatter else {
            return iso_date.to_owned();
        };
        let Ok(date) = IcuDate::try_new_gregorian(y, m, d) else {
            return iso_date.to_owned();
        };
        let Ok(time) = Time::try_new(0, 0, 0, 0) else {
            return iso_date.to_owned();
        };
        let datetime = DateTime { date, time };
        let weekday = formatter.format(&datetime).to_string();
        let mut chars = weekday.chars();
        let weekday = match chars.next() {
            Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
            None => weekday,
        };
        format!("{weekday} {d}")
    }

    fn table_row<'a>(
        &self,
        gap: u16,
        day: Element<'a, Message>,
        cond: Element<'a, Message>,
        temp: Element<'a, Message>,
        wind: Element<'a, Message>,
        precip: Element<'a, Message>,
    ) -> Element<'a, Message> {
        row![
            Container::new(day)
                .width(Length::Fixed(DAY_W))
                .align_x(Horizontal::Left),
            Container::new(cond)
                .width(Length::Fixed(COND_W))
                .align_x(Horizontal::Center),
            Container::new(temp)
                .width(Length::Fixed(TEMP_W))
                .align_x(Horizontal::Right),
            Container::new(wind)
                .width(Length::Fixed(WIND_W))
                .align_x(Horizontal::Right),
            Container::new(precip)
                .width(Length::Fixed(PRECIP_W))
                .align_x(Horizontal::Right),
        ]
        .spacing(f32::from(gap))
        .align_y(Alignment::Center)
        .into()
    }

    /// 1px horizontal line: the header rule and the row hairlines.
    fn hline<'a>(color: Color) -> Element<'a, Message> {
        Container::new(Space::new())
            .width(Length::Fill)
            .height(Length::Fixed(1.0))
            .style(move |_theme| cosmic::iced::widget::container::Style {
                background: Some(Background::Color(color)),
                ..Default::default()
            })
            .into()
    }

    /// The stock `popup_container` look with a wider fixed width (the stock
    /// helper is hard-limited to 360px).
    fn modern_popup<'a>(&self, content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
        let (vertical_align, horizontal_align) = match self.core.applet.anchor {
            PanelAnchor::Left => (Vertical::Center, Horizontal::Left),
            PanelAnchor::Right => (Vertical::Center, Horizontal::Right),
            PanelAnchor::Top => (Vertical::Top, Horizontal::Center),
            PanelAnchor::Bottom => (Vertical::Bottom, Horizontal::Center),
        };

        autosize::autosize(
            Container::new(Container::new(content.into()).style(|theme| {
                let cosmic = theme.cosmic();
                let corners = cosmic.corner_radii;
                cosmic::iced::widget::container::Style {
                    text_color: Some(cosmic.background(theme.transparent).on.into()),
                    background: Some(
                        Color::from(cosmic.background(theme.transparent).base).into(),
                    ),
                    border: cosmic::iced::Border {
                        radius: corners.radius_m.into(),
                        width: 1.0,
                        color: cosmic.background(theme.transparent).divider.into(),
                    },
                    icon_color: Some(cosmic.background(theme.transparent).on.into()),
                    snap: true,
                    ..Default::default()
                }
            }))
            .height(Length::Shrink)
            .align_x(horizontal_align)
            .align_y(vertical_align),
            POPUP_AUTOSIZE_ID.clone(),
        )
        .limits(
            Limits::NONE
                .min_height(1.)
                .min_width(POPUP_WIDTH)
                .max_width(POPUP_WIDTH)
                .max_height(1000.0),
        )
        .into()
    }
}

impl cosmic::Application for WeatherApplet {
    type Message = Message;
    type Executor = cosmic::SingleThreadExecutor;
    type Flags = ();
    const APP_ID: &'static str = "fun.wmde.AppletWeather";

    fn init(core: cosmic::app::Core, _flags: Self::Flags) -> (Self, app::Task<Self::Message>) {
        let config = cosmic_config::Config::new(Self::APP_ID, WeatherAppletConfig::VERSION)
            .ok()
            .and_then(|c| WeatherAppletConfig::get_entry(&c).ok())
            .unwrap_or_default();

        (
            Self {
                core,
                popup: None,
                config,
                current: None,
                daily: None,
                daily_stamp: None,
                daily_coords: (0.0, 0.0),
                daily_failed: false,
                token_tx: None,
                locale: get_system_locale(),
            },
            Task::none(),
        )
    }

    fn core(&self) -> &cosmic::app::Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut cosmic::app::Core {
        &mut self.core
    }

    fn update(&mut self, message: Self::Message) -> app::Task<Self::Message> {
        match message {
            Message::TogglePopup => {
                if let Some(p) = self.popup.take() {
                    return cosmic::surface::surface_task(cosmic::surface::action::destroy_popup(
                        p,
                    ));
                } else {
                    let open = cosmic::surface::surface_task(cosmic::surface::action::app_popup(
                        |_| Default::default(),
                        |app: &mut WeatherApplet| {
                            let new_id = window::Id::unique();
                            app.popup.replace(new_id);

                            let mut popup_settings = app.core.applet.get_popup_settings(
                                app.core.main_window_id().unwrap(),
                                new_id,
                                None,
                                None,
                                None,
                            );
                            // The default positioner is hard-limited to
                            // 360px; the forecast table needs more.
                            popup_settings.positioner.size = None;
                            popup_settings.positioner.size_limits = Limits::NONE
                                .min_height(1.0)
                                .min_width(POPUP_WIDTH)
                                .max_width(POPUP_WIDTH)
                                .max_height(1080.0);
                            popup_settings
                        },
                        None,
                    ));
                    if self.daily_is_stale() {
                        self.daily_failed = false;
                        self.daily_coords = (self.config.latitude, self.config.longitude);
                        return Task::batch([open, self.fetch_daily_task()]);
                    }
                    return open;
                }
            }
            Message::CloseRequested(id) => {
                if Some(id) == self.popup {
                    self.popup = None;
                }
            }
            Message::Tick => {
                return self.fetch_current_task();
            }
            Message::CurrentFetched(current) => {
                if current.is_some() {
                    self.current = current;
                }
            }
            Message::DailyFetched(daily) => match daily {
                Some(days) => {
                    self.daily = Some(days);
                    self.daily_stamp = Some(Instant::now());
                    self.daily_failed = false;
                }
                None => {
                    if self.daily.is_none() {
                        self.daily_failed = true;
                    }
                }
            },
            Message::ConfigChanged(config) => {
                let coords_changed = (config.latitude, config.longitude)
                    != (self.config.latitude, self.config.longitude);
                self.config = config;
                if coords_changed {
                    self.current = None;
                    self.daily = None;
                    self.daily_stamp = None;
                    self.daily_failed = false;
                    let mut tasks = vec![self.fetch_current_task()];
                    if self.popup.is_some() {
                        self.daily_coords = (self.config.latitude, self.config.longitude);
                        tasks.push(self.fetch_daily_task());
                    }
                    return Task::batch(tasks);
                }
            }
            Message::OpenSettings => {
                // The settings page of this applet in Settings, built from
                // `fun.wmde.AppletWeather.ron`. The city search, which no schema can
                // describe, is reached from there by its `External` row.
                let exec = format!("wmde-settings applet {}", Self::APP_ID);
                if let Some(tx) = self.token_tx.as_ref() {
                    let _ = tx.send(TokenRequest {
                        app_id: Self::APP_ID.to_string(),
                        exec,
                    });
                } else {
                    tracing::error!("Wayland tx is None");
                }
            }
            Message::Token(u) => match u {
                TokenUpdate::Init(tx) => {
                    self.token_tx = Some(tx);
                }
                TokenUpdate::Finished => {
                    self.token_tx = None;
                }
                TokenUpdate::ActivationToken { token, .. } => {
                    let mut cmd = std::process::Command::new("wmde-settings");
                    cmd.args(["applet", Self::APP_ID]);
                    if let Some(token) = token {
                        cmd.env("XDG_ACTIVATION_TOKEN", &token);
                        cmd.env("DESKTOP_STARTUP_ID", &token);
                    }
                    tokio::spawn(cosmic::process::spawn(cmd));
                }
            },
            Message::Surface(a) => {
                return cosmic::task::message(cosmic::Action::Surface(a));
            }
        }
        Task::none()
    }

    fn view(&self) -> Element<'_, Message> {
        let horizontal = self.core.applet.is_horizontal();
        let suggested = self.core.applet.suggested_size(true);
        let (pad_major, pad_minor) = self.core.applet.suggested_padding(true);
        // Same axis mapping as the text_button helper in the applet Context.
        let (horizontal_padding, vertical_padding) = if horizontal {
            (pad_major, pad_minor)
        } else {
            (pad_minor, pad_major)
        };

        let glyph_char = self
            .current
            .as_ref()
            .map(|c| weather::glyph_for_code(c.weather_code))
            .unwrap_or(weather::GLYPH_CLOUD);
        // The glyph line box is pinned to the suggested icon height: Weather
        // Icons glyphs overhang their em box, and an unconstrained line would
        // inflate the button - and its hover box - to the full panel height.
        let glyph = text(glyph_char.to_string())
            .font(WEATHER_FONT)
            .size(f32::from(suggested.1))
            .line_height(cosmic::iced::widget::text::LineHeight::Absolute(
                f32::from(suggested.1).into(),
            ));

        let temp_str = self
            .current
            .as_ref()
            .map(|c| weather::format_temp_short(c.temperature_c, self.config.unit))
            .unwrap_or_else(|| String::from("--\u{b0}"));
        let temp = self.core.applet.text(temp_str);

        // Built the way Context::text_button builds its button: the content
        // sits in a layer_container fixed to the suggested applet size, so
        // the hover box keeps its margins from the panel edges instead of
        // filling the panel.
        let button = if horizontal {
            button::custom(
                layer_container(row![glyph, temp].spacing(4).align_y(Alignment::Center))
                    .center_y(Length::Fixed(f32::from(suggested.1 + 2 * vertical_padding))),
            )
            .padding([0, horizontal_padding])
        } else {
            button::custom(
                layer_container(column![glyph, temp].spacing(2).align_x(Alignment::Center))
                    .center_x(Length::Fixed(f32::from(suggested.0 + 2 * horizontal_padding))),
            )
            .padding([vertical_padding, 0])
        }
        .on_press_down(Message::TogglePopup)
        .class(cosmic::theme::Button::AppletIcon);

        autosize::autosize(
            Element::from(self.core.applet.applet_tooltip(
                Element::from(button),
                self.tooltip_text(),
                self.popup.is_some(),
                Message::Surface,
                None,
            )),
            AUTOSIZE_MAIN_ID.clone(),
        )
        .into()
    }

    fn view_window(&self, _id: window::Id) -> Element<'_, Message> {
        let Spacing {
            space_xxxs,
            space_xxs,
            space_s,
            ..
        } = theme::active().cosmic().spacing;

        let active = theme::active();
        let palette = active.cosmic();
        let ink = Color::from(palette.background(active.transparent).on);
        // The header keeps the Nimbus principle - same size as the data, set
        // apart by weight and a secondary tone of the theme ink.
        let secondary = Color {
            a: 0.75 * ink.a,
            ..ink
        };
        // Rule and hairlines both draw with the theme divider color.
        let line = Color::from(palette.background(active.transparent).divider);

        let header_cell = |label: String| -> Element<'_, Message> {
            text::body(label)
                .font(cosmic::font::semibold())
                .class(theme::Text::Color(secondary))
                .into()
        };
        let cell = |value: String| -> Element<'_, Message> { text::body(value).into() };

        let mut rows: Vec<Element<'_, Message>> = Vec::with_capacity(16);
        rows.push(self.table_row(
            space_s,
            header_cell(fl!("col-day")),
            header_cell(fl!("col-condition")),
            header_cell(fl!("col-temp")),
            header_cell(fl!("col-wind")),
            header_cell(fl!("col-precip")),
        ));
        rows.push(Self::hline(line));

        match self.daily.as_deref() {
            Some(days) if !days.is_empty() => {
                let prefs = DateTimeFormatterPreferences::from(self.locale.clone());
                let weekday_fmt = DateTimeFormatter::try_new(prefs, fieldsets::E::short()).ok();
                for (i, day) in days.iter().enumerate() {
                    if i > 0 {
                        rows.push(Self::hline(line));
                    }
                    let glyph = text(weather::glyph_for_code(day.code).to_string())
                        .font(WEATHER_FONT)
                        .size(GLYPH_SIZE);
                    rows.push(self.table_row(
                        space_s,
                        cell(self.day_label(&day.date, weekday_fmt.as_ref())),
                        glyph.into(),
                        cell(weather::format_temp_range(
                            day.temp_max_c,
                            day.temp_min_c,
                            self.config.unit,
                        )),
                        cell(weather::format_wind(
                            day.wind_kmh,
                            self.config.wind_unit,
                            &self.wind_label(),
                        )),
                        cell(weather::format_precip(day.precip_mm, &fl!("unit-mm"))),
                    ));
                }
            }
            _ => {
                rows.push(
                    text::body(if self.daily_failed {
                        fl!("forecast-failed")
                    } else {
                        fl!("loading")
                    })
                    .into(),
                );
            }
        }

        let table = Column::with_children(rows)
            .spacing(f32::from(space_xxxs))
            .padding(cosmic::iced::Padding {
                top: f32::from(space_xxxs),
                right: f32::from(space_s),
                bottom: f32::from(space_xxxs),
                left: f32::from(space_s),
            });

        let content = Column::with_capacity(3)
            .push(table)
            .push(padded_control(divider::horizontal::default()).padding([space_xxs, space_s]))
            .push(menu_button(text::body(fl!("weather-settings"))).on_press(Message::OpenSettings))
            .padding([8, 0]);

        self.modern_popup(content)
    }

    fn subscription(&self) -> Subscription<Message> {
        fn tick_subscription(minutes: u32) -> Subscription<Message> {
            Subscription::run_with(minutes, |minutes: &u32| {
                let period = Duration::from_secs(u64::from(*minutes).max(1) * 60);
                stream::channel(1, move |mut output: mpsc::Sender<Message>| async move {
                    loop {
                        let _ = output.send(Message::Tick).await;
                        tokio::time::sleep(period).await;
                    }
                })
            })
        }

        Subscription::batch([
            tick_subscription(self.config.update_interval_minutes),
            activation_token_subscription(0).map(Message::Token),
            self.core.watch_config(Self::APP_ID).map(|u| {
                for err in u.errors {
                    tracing::error!(?err, "Error watching config");
                }
                Message::ConfigChanged(u.config)
            }),
        ])
    }

    fn on_close_requested(&self, id: window::Id) -> Option<Message> {
        Some(Message::CloseRequested(id))
    }

    fn style(&self) -> Option<cosmic::iced::theme::Style> {
        Some(cosmic::applet::style())
    }
}
