// SPDX-License-Identifier: GPL-3.0-only

//! Standalone settings window (installed as wmde-weather-settings): city search via the
//! Open-Meteo geocoding API, and nothing else.
//!
//! Everything that is a plain stored value - units, update interval, the coordinates
//! themselves - is edited in Settings, built from
//! `/usr/share/wmde/applet-settings/fun.wmde.AppletWeather.ron`. What is left here is the
//! one thing a declarative schema cannot describe: a network query whose result writes
//! three keys at once.
//!
//! Those three keys are written one at a time rather than with `write_entry`. Settings
//! writes the same config, and writing the whole struct would push this window's stale
//! copy of the units back over whatever was just chosen there.

use crate::fl;
use crate::localize::LANGUAGE_LOADER;
use crate::weather::{self, CityMatch};
use cosmic::{
    ApplicationExt, Element, Task, app,
    iced::{Alignment, Length, Limits},
    theme,
    widget::{Column, button, container, scrollable, text, text_input},
};
use cosmic_applets_config::weather::WeatherAppletConfig;
use cosmic_config::{ConfigSet, CosmicConfigEntry};
use i18n_embed::LanguageLoader;

pub fn run() -> cosmic::iced::Result {
    let settings = cosmic::app::Settings::default()
        .size_limits(Limits::NONE.width(480.0).height(420.0))
        .resizable(Some(0.0));
    cosmic::app::run::<SettingsApp>(settings, ())
}

struct SettingsApp {
    core: cosmic::app::Core,
    handler: Option<cosmic_config::Config>,
    config: WeatherAppletConfig,
    city_query: String,
    results: Vec<CityMatch>,
    searching: bool,
    searched: bool,
    search_failed: bool,
}

#[derive(Debug, Clone)]
enum Message {
    CityQueryChanged(String),
    Search,
    SearchResults(Option<Vec<CityMatch>>),
    SelectCity(usize),
}

fn city_label(city: &CityMatch) -> String {
    let mut label = city.name.clone();
    if !city.admin1.is_empty() {
        label.push_str(", ");
        label.push_str(&city.admin1);
    }
    if !city.country.is_empty() {
        label.push_str(", ");
        label.push_str(&city.country);
    }
    label
}

impl SettingsApp {
    /// Write the three keys a city choice sets, one at a time.
    fn save_location(&self) {
        let Some(handler) = self.handler.as_ref() else {
            tracing::error!("config handler unavailable; weather location not persisted");
            return;
        };

        let writes: [(&str, Result<(), cosmic_config::Error>); 3] = [
            ("latitude", handler.set("latitude", self.config.latitude)),
            ("longitude", handler.set("longitude", self.config.longitude)),
            (
                "city_name",
                handler.set("city_name", self.config.city_name.clone()),
            ),
        ];

        for (key, result) in writes {
            if let Err(err) = result {
                tracing::error!(key, ?err, "failed to write weather applet config");
            }
        }
    }
}

impl cosmic::Application for SettingsApp {
    type Message = Message;
    type Executor = cosmic::executor::Default;
    type Flags = ();
    const APP_ID: &'static str = "fun.wmde.AppletWeather";

    fn init(core: cosmic::app::Core, _flags: Self::Flags) -> (Self, app::Task<Self::Message>) {
        let handler = cosmic_config::Config::new(Self::APP_ID, WeatherAppletConfig::VERSION).ok();
        let config = handler
            .as_ref()
            .and_then(|c| WeatherAppletConfig::get_entry(c).ok())
            .unwrap_or_default();

        let mut app = Self {
            core,
            handler,
            config,
            city_query: String::new(),
            results: Vec::new(),
            searching: false,
            searched: false,
            search_failed: false,
        };

        // WMDE: the window title is also what the normal title bar shows.
        let task = match app.core.main_window_id() {
            Some(id) => app.set_window_title(fl!("settings-title"), id),
            None => Task::none(),
        };

        (app, task)
    }

    fn core(&self) -> &cosmic::app::Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut cosmic::app::Core {
        &mut self.core
    }

    fn update(&mut self, message: Self::Message) -> app::Task<Self::Message> {
        match message {
            Message::CityQueryChanged(query) => {
                self.city_query = query;
            }
            Message::Search => {
                let query = self.city_query.trim().to_owned();
                if query.is_empty() || self.searching {
                    return Task::none();
                }
                self.searching = true;
                self.searched = true;
                self.search_failed = false;
                let lang = LANGUAGE_LOADER.current_language().language.as_str().to_owned();
                return cosmic::task::future(async move {
                    Message::SearchResults(weather::search_city(query, lang).await)
                });
            }
            Message::SearchResults(results) => {
                self.searching = false;
                match results {
                    Some(results) => self.results = results,
                    None => {
                        self.results.clear();
                        self.search_failed = true;
                    }
                }
            }
            Message::SelectCity(index) => {
                if let Some(city) = self.results.get(index) {
                    self.config.latitude = city.latitude;
                    self.config.longitude = city.longitude;
                    self.config.city_name = city.name.clone();
                    self.city_query = city.name.clone();
                    self.results.clear();
                    self.searched = false;
                    self.save_location();
                }
            }
        }
        Task::none()
    }

    fn view(&self) -> Element<'_, Message> {
        let spacing = theme::active().cosmic().spacing;

        let search_row = cosmic::iced::widget::row![
            text_input(fl!("city-placeholder"), &self.city_query)
                .on_input(Message::CityQueryChanged)
                .on_submit(|_| Message::Search)
                .width(Length::Fill),
            button::standard(fl!("search")).on_press_maybe(
                (!self.searching && !self.city_query.trim().is_empty())
                    .then_some(Message::Search)
            ),
        ]
        .spacing(spacing.space_xs)
        .align_y(Alignment::Center);

        let mut location = Column::new()
            .push(text::heading(fl!("city")))
            .push(search_row)
            .push(text::body(format!(
                "{}: {} ({:.4}, {:.4})",
                fl!("selected-city"),
                self.config.city_name,
                self.config.latitude,
                self.config.longitude
            )))
            .spacing(spacing.space_xs);

        if self.searching {
            location = location.push(text::body(fl!("searching")));
        } else if self.search_failed {
            location = location.push(text::body(fl!("search-failed")));
        } else if !self.results.is_empty() {
            let mut list = Column::new().spacing(2);
            for (i, city) in self.results.iter().enumerate() {
                list = list.push(
                    button::text(format!(
                        "{} ({:.2}, {:.2})",
                        city_label(city),
                        city.latitude,
                        city.longitude
                    ))
                    .on_press(Message::SelectCity(i)),
                );
            }
            location = location.push(
                scrollable(list).height(Length::Fixed(180.0)).width(Length::Fill),
            );
        } else if self.searched {
            location = location.push(text::body(fl!("no-results")));
        }

        let content = Column::new()
            .push(text::title3(fl!("settings-title")))
            .push(location)
            .spacing(spacing.space_m)
            .padding(spacing.space_l)
            .max_width(480);

        container(scrollable(content))
            .width(Length::Fill)
            .center_x(Length::Fill)
            .into()
    }
}
