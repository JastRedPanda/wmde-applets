// SPDX-License-Identifier: GPL-3.0-only
//! Receiver for the Ubuntu/Unity `com.canonical.Unity.LauncherEntry.Update` broadcast.
//!
//! The signal is `Update(s app_uri, a{sv} properties)` on the session bus, sent with no
//! destination, so every client with a match rule sees it. Properties arrive as a partial
//! dictionary, so state has to be accumulated per sender. Only `progress` and
//! `progress-visible` are read: `count`, `count-visible` and `urgent` have no agreed look in
//! this stack.

use std::{collections::HashMap, time::Duration};

use cosmic::iced::{Subscription, stream};
use futures::{SinkExt, StreamExt, channel::mpsc::Sender};
use rustc_hash::FxHashMap;
use tokio::time::{Instant, sleep, sleep_until};
use zbus::{
    Connection, MatchRule, MessageStream,
    fdo::{DBusProxy, RequestNameFlags},
    message::Type as MessageType,
    zvariant::{OwnedValue, Value},
};

const INTERFACE: &str = "com.canonical.Unity.LauncherEntry";
const UNITY_NAME: &str = "com.canonical.Unity";

/// Coalescing window for a moving percentage. One message makes iced rebuild `view()` of every
/// window of this process, and a sender that reports per copied file would otherwise drive the
/// panel's redraw rate.
const THROTTLE: Duration = Duration::from_millis(100);

/// Delay before reconnecting after the session bus is lost.
const RECONNECT: Duration = Duration::from_secs(5);

/// Everything that should be painted right now, keyed by normalized desktop id
/// (`fun.wmde.files`). A snapshot, not a delta: the applet then only ever assigns, an emptied
/// map is the one and only way a bar disappears, and a sender that left the bus needs no
/// special message.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LauncherEntryUpdate(pub FxHashMap<String, f32>);

pub fn launcher_entry_subscription() -> Subscription<LauncherEntryUpdate> {
    // The key is the identity of the subscription: `TypeId::of::<WaylandUpdate>()` is already
    // taken by `wayland_subscription`, and two subscriptions sharing a key collapse into one.
    Subscription::run_with(std::any::TypeId::of::<LauncherEntryUpdate>(), |_| {
        stream::channel(8, move |mut output| async move {
            loop {
                if let Err(err) = listen(&mut output).await {
                    tracing::warn!(?err, "Unity LauncherEntry listener stopped");
                }
                // The bus is gone: nothing is tracked any more, so every bar goes out.
                let _ = output.send(LauncherEntryUpdate::default()).await;
                sleep(RECONNECT).await;
            }
        })
    })
}

#[derive(Debug, Clone, Copy, Default)]
struct Entry {
    progress: f32,
    visible: bool,
}

#[derive(Default)]
struct State {
    /// (unique bus name of the sender, normalized desktop id) -> accumulated properties.
    /// One process may report about several ids (Nautilus does), and several processes may
    /// report about one id (each wmde-files window is its own process), so neither half of
    /// the key is enough on its own.
    entries: HashMap<(String, String), Entry>,
}

impl State {
    /// Returns true when the message carried something worth a new snapshot.
    ///
    /// The record outlives `progress-visible: false` on purpose: the dictionary is partial, so
    /// a sender is allowed to hide the bar and later show it again without repeating the
    /// percentage. What is kept here is only ever dropped when the sender leaves the bus.
    fn apply(&mut self, sender: &str, uri: &str, props: &HashMap<String, OwnedValue>) -> bool {
        let Some(id) = normalize_app_uri(uri) else {
            return false;
        };
        if !props.contains_key("progress") && !props.contains_key("progress-visible") {
            return false;
        }
        let entry = self.entries.entry((sender.to_owned(), id)).or_default();
        if let Some(value) = props.get("progress") {
            entry.progress = read_progress(value);
        }
        if let Some(value) = props.get("progress-visible") {
            entry.visible = bool::try_from(value).unwrap_or(false);
        }
        true
    }

    /// Drops everything a departed connection was reporting.
    fn forget_sender(&mut self, sender: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|(name, _), _| name != sender);
        self.entries.len() != before
    }

    /// One bar per application. Several senders can report about one id, and the maximum is
    /// the only aggregate that does not jump backwards when a second, freshly started
    /// operation reports 0 - and it keeps the bar alive while the other window is still busy.
    fn snapshot(&self) -> FxHashMap<String, f32> {
        let mut out: FxHashMap<String, f32> = FxHashMap::default();
        for ((_, id), entry) in &self.entries {
            if !entry.visible {
                continue;
            }
            let value = quantize(entry.progress);
            let slot = out.entry(id.clone()).or_insert(0.0);
            if value > *slot {
                *slot = value;
            }
        }
        out
    }
}

/// `application://fun.wmde.files.desktop` -> `fun.wmde.files`.
///
/// Tolerates what senders actually emit: a missing scheme, a missing `.desktop` suffix, and an
/// absolute path - libunity rewrites the uri to `application:///snap/...` in snap
/// environments, and that form carries three slashes.
///
/// The result is lowercased. The applet compares ids case insensitively, so two spellings of
/// one id have to collapse into one key here as well - otherwise `snapshot` keeps them apart,
/// never takes the maximum, and the button shows whichever the hash map happened to yield.
fn normalize_app_uri(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("application://").unwrap_or(uri);
    let rest = rest.rsplit('/').next().unwrap_or(rest);
    let rest = rest.strip_suffix(".desktop").unwrap_or(rest);
    (!rest.is_empty()).then(|| rest.to_ascii_lowercase())
}

/// The specification says `d`, but senders are known to put integers in. Anything that is not
/// a finite number becomes 0: a NaN from a sender that divided by zero must never reach the
/// layout.
fn read_progress(value: &OwnedValue) -> f32 {
    // A variant inside a variant is what QDBus produces for `a{sv}`: `QDBusVariant` wrapped in
    // `QVariant` marshals as `<<0.5>>`. Without unwrapping it the value falls through to 0 and
    // the button looks idle for the whole operation.
    let mut value: &Value<'_> = value;
    while let Value::Value(inner) = value {
        value = inner;
    }
    let raw = match value {
        Value::F64(v) => *v,
        Value::I64(v) => *v as f64,
        Value::U64(v) => *v as f64,
        Value::I32(v) => f64::from(*v),
        Value::U32(v) => f64::from(*v),
        Value::I16(v) => f64::from(*v),
        Value::U16(v) => f64::from(*v),
        Value::U8(v) => f64::from(*v),
        _ => return 0.0,
    };
    if raw.is_finite() {
        raw.clamp(0.0, 1.0) as f32
    } else {
        0.0
    }
}

/// Whole percents. Finer than that cannot be drawn on a button a few dozen pixels wide, and
/// quantizing here is what makes "nothing changed" detectable at all.
fn quantize(progress: f32) -> f32 {
    if progress.is_finite() {
        (progress.clamp(0.0, 1.0) * 100.0).round() / 100.0
    } else {
        0.0
    }
}

/// What to do with a freshly built snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flush {
    /// Identical to what the applet already has.
    Skip,
    /// A bar appeared or disappeared - the user sees that, so it goes out at once.
    Now,
    /// Only a percentage moved - wait for the throttle window.
    Deferred,
}

fn flush_kind(next: &FxHashMap<String, f32>, sent: &FxHashMap<String, f32>) -> Flush {
    if next == sent {
        return Flush::Skip;
    }
    if next.len() != sent.len() || next.keys().any(|key| !sent.contains_key(key)) {
        return Flush::Now;
    }
    Flush::Deferred
}

/// Coalescing window, kept apart from `listen` so it can be exercised without a session bus.
///
/// `listen` owns the `select!` and the io; everything about when a snapshot leaves lives here.
#[derive(Default)]
struct Throttle {
    /// What the applet has already been told.
    sent: FxHashMap<String, f32>,
    /// A snapshot waiting for the window to close.
    pending: Option<FxHashMap<String, f32>>,
    /// When that snapshot goes out.
    deadline: Option<Instant>,
}

impl Throttle {
    fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    /// Takes a freshly built snapshot and returns it when it has to go out right now.
    fn on_snapshot(
        &mut self,
        next: FxHashMap<String, f32>,
        now: Instant,
    ) -> Option<FxHashMap<String, f32>> {
        match flush_kind(&next, &self.sent) {
            Flush::Skip => {
                self.pending = None;
                self.deadline = None;
                None
            }
            Flush::Now => {
                self.pending = None;
                self.deadline = None;
                self.sent.clone_from(&next);
                Some(next)
            }
            Flush::Deferred => {
                // The deadline is armed once and does not slide, so a sender emitting every
                // 20 ms cannot starve the flush.
                self.deadline.get_or_insert(now + THROTTLE);
                self.pending = Some(next);
                None
            }
        }
    }

    /// The window closed: hands over whatever was held back.
    fn on_deadline(&mut self) -> Option<FxHashMap<String, f32>> {
        self.deadline = None;
        let next = self.pending.take()?;
        self.sent.clone_from(&next);
        Some(next)
    }
}

/// Sleeps until `deadline`, or forever when there is none. The deadline is taken by value so
/// the `select!` branch does not borrow the variable its own handler reassigns.
async fn wait_until(deadline: Option<Instant>) {
    match deadline {
        Some(at) => sleep_until(at).await,
        None => std::future::pending().await,
    }
}

async fn listen(output: &mut Sender<LauncherEntryUpdate>) -> zbus::Result<()> {
    let conn = Connection::session().await?;
    let dbus = DBusProxy::new(&conn).await?;

    // Applications built on libunity stay completely silent unless somebody owns this name:
    // they check whether Unity is running before every emit. Losing the race is normal - a
    // second panel carries a second copy of this applet, and an external dock may hold the
    // name - so a refusal is not an error and we keep listening either way.
    //
    // `DoNotQueue` is deliberately absent: without the queue a loser never gets the name, and
    // the panel restarts this process on every change to its own config, so the new instance
    // asks while the old one is still on the bus. The name would then be left unowned when the
    // old one exits, and every libunity client would fall silent for the rest of the session.
    // `ReplaceExisting` stays absent for the opposite reason: with it every panel restart
    // would steal the name and make every libunity client resend its whole state.
    match dbus
        .request_name(
            UNITY_NAME.try_into()?,
            RequestNameFlags::AllowReplacement.into(),
        )
        .await
    {
        Ok(reply) => tracing::debug!(?reply, "requested {UNITY_NAME}"),
        Err(err) => tracing::debug!(?err, "could not take {UNITY_NAME}"),
    }

    // No sender and no path in the rule on purpose: the path is a hash of the uri picked by
    // the sender, and every other implementation of this protocol ignores it. No eavesdrop
    // either - the bus delivers broadcast signals to every matching client.
    let rule = MatchRule::builder()
        .msg_type(MessageType::Signal)
        .interface(INTERFACE)?
        .member("Update")?
        .build();
    let mut updates = MessageStream::for_match_rule(rule, &conn, Some(64)).await?;
    let mut owners = dbus.receive_name_owner_changed().await?;

    let mut state = State::default();
    let mut throttle = Throttle::default();

    loop {
        tokio::select! {
            () = wait_until(throttle.deadline()), if throttle.deadline().is_some() => {
                if let Some(next) = throttle.on_deadline()
                    && output.send(LauncherEntryUpdate(next)).await.is_err()
                {
                    return Ok(());
                }
                continue;
            }
            message = updates.next() => {
                let Some(Ok(message)) = message else {
                    return Ok(());
                };
                let Some(sender) = message.header().sender().map(ToString::to_string) else {
                    continue;
                };
                let body = message.body();
                let Ok((uri, props)) =
                    body.deserialize::<(String, HashMap<String, OwnedValue>)>()
                else {
                    continue;
                };
                if !state.apply(&sender, &uri, &props) {
                    continue;
                }
            }
            change = owners.next() => {
                let Some(change) = change else {
                    return Ok(());
                };
                let Ok(args) = change.args() else {
                    continue;
                };
                if args.new_owner().is_some() {
                    continue;
                }
                // Only unique names are ever stored, so a well-known name leaving matches
                // nothing and costs one retain.
                if !state.forget_sender(args.name().as_str()) {
                    continue;
                }
            }
        }

        if let Some(next) = throttle.on_snapshot(state.snapshot(), Instant::now())
            && output.send(LauncherEntryUpdate(next)).await.is_err()
        {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owned(value: Value<'static>) -> OwnedValue {
        OwnedValue::try_from(value).unwrap()
    }

    fn props(pairs: &[(&str, Value<'static>)]) -> HashMap<String, OwnedValue> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), owned(value.try_clone().unwrap())))
            .collect()
    }

    const URI: &str = "application://fun.wmde.files.desktop";
    const ID: &str = "fun.wmde.files";

    #[test]
    fn normalizes_what_senders_emit() {
        assert_eq!(normalize_app_uri(URI).as_deref(), Some(ID));
        assert_eq!(
            normalize_app_uri("fun.wmde.files.desktop").as_deref(),
            Some(ID)
        );
        assert_eq!(
            normalize_app_uri("application://fun.wmde.files").as_deref(),
            Some(ID)
        );
        assert_eq!(
            normalize_app_uri("application:///snap/x/picard.desktop").as_deref(),
            Some("picard")
        );
        assert_eq!(
            normalize_app_uri("application://picard_picard.desktop").as_deref(),
            Some("picard_picard")
        );
        assert_eq!(normalize_app_uri("application://"), None);
        assert_eq!(normalize_app_uri(""), None);
    }

    #[test]
    fn folds_the_case_of_the_id() {
        // The applet matches ids case insensitively, so two spellings from one application
        // have to end up under one key - otherwise `snapshot` never takes their maximum.
        assert_eq!(
            normalize_app_uri("application://Fun.WMDE.Files.desktop").as_deref(),
            Some(ID)
        );

        let mut state = State::default();
        let shown = |progress| {
            props(&[
                ("progress", Value::F64(progress)),
                ("progress-visible", Value::Bool(true)),
            ])
        };
        state.apply(":1.1", "application://Fun.WMDE.Files.desktop", &shown(0.9));
        state.apply(":1.2", URI, &shown(0.2));
        assert_eq!(state.snapshot().len(), 1);
        assert_eq!(state.snapshot().get(ID), Some(&0.9));
    }

    fn map(pairs: &[(&str, f32)]) -> FxHashMap<String, f32> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), *value))
            .collect()
    }

    #[test]
    fn reads_progress_of_any_numeric_type() {
        assert_eq!(read_progress(&owned(Value::F64(0.42))), 0.42);
        assert_eq!(read_progress(&owned(Value::F64(-1.0))), 0.0);
        assert_eq!(read_progress(&owned(Value::F64(2.0))), 1.0);
        assert_eq!(read_progress(&owned(Value::F64(f64::NAN))), 0.0);
        assert_eq!(read_progress(&owned(Value::F64(f64::INFINITY))), 0.0);
        assert_eq!(read_progress(&owned(Value::I32(1))), 1.0);
        assert_eq!(read_progress(&owned(Value::from("0.5"))), 0.0);
    }

    #[test]
    fn reads_progress_wrapped_in_a_variant() {
        // QDBus marshals `a{sv}` as a variant inside a variant: `<<0.5>>`.
        assert_eq!(
            read_progress(&owned(Value::Value(Box::new(Value::F64(0.42))))),
            0.42
        );
        assert_eq!(
            read_progress(&owned(Value::Value(Box::new(Value::Value(Box::new(
                Value::I32(1)
            )))))),
            1.0
        );
    }

    #[test]
    fn accumulates_a_partial_dictionary() {
        let mut state = State::default();

        // Progress on its own is not painted: the specification gates it on progress-visible.
        assert!(state.apply(":1.1", URI, &props(&[("progress", Value::F64(0.4))])));
        assert!(state.snapshot().is_empty());

        // The flag alone turns on what was already accumulated.
        assert!(state.apply(
            ":1.1",
            URI,
            &props(&[("progress-visible", Value::Bool(true))])
        ));
        assert_eq!(state.snapshot().get(ID), Some(&0.4));

        // A moving percentage does not have to repeat the flag.
        assert!(state.apply(":1.1", URI, &props(&[("progress", Value::F64(0.7))])));
        assert_eq!(state.snapshot().get(ID), Some(&0.7));

        assert!(state.apply(
            ":1.1",
            URI,
            &props(&[("progress-visible", Value::Bool(false))])
        ));
        assert!(state.snapshot().is_empty());

        // Still hidden: the sender never turned the flag back on.
        assert!(state.apply(":1.1", URI, &props(&[("progress", Value::F64(0.9))])));
        assert!(state.snapshot().is_empty());
    }

    #[test]
    fn ignores_properties_that_are_not_painted() {
        let mut state = State::default();
        assert!(!state.apply(
            ":1.1",
            URI,
            &props(&[
                ("count", Value::I64(5)),
                ("count-visible", Value::Bool(true)),
            ])
        ));
        assert!(state.snapshot().is_empty());
    }

    #[test]
    fn takes_the_maximum_over_senders_of_one_id() {
        let mut state = State::default();
        let shown = |progress| {
            props(&[
                ("progress", Value::F64(progress)),
                ("progress-visible", Value::Bool(true)),
            ])
        };
        state.apply(":1.1", URI, &shown(0.9));
        state.apply(":1.2", URI, &shown(0.2));
        assert_eq!(state.snapshot().get(ID), Some(&0.9));

        assert!(state.forget_sender(":1.1"));
        assert_eq!(state.snapshot().get(ID), Some(&0.2));

        assert!(state.forget_sender(":1.2"));
        assert!(state.snapshot().is_empty());
    }

    #[test]
    fn one_sender_reporting_two_ids_leaves_at_once() {
        let mut state = State::default();
        let shown = props(&[
            ("progress", Value::F64(0.5)),
            ("progress-visible", Value::Bool(true)),
        ]);
        state.apply(":1.1", URI, &shown);
        state.apply(":1.1", "application://fun.wmde.term.desktop", &shown);
        assert_eq!(state.snapshot().len(), 2);

        assert!(state.forget_sender(":1.1"));
        assert!(state.snapshot().is_empty());
        assert!(!state.forget_sender(":1.1"));
    }

    #[test]
    fn quantizes_to_whole_percents() {
        assert_eq!(quantize(0.4449), 0.44);
        assert_eq!(quantize(0.4451), 0.45);
        assert_eq!(quantize(1.0), 1.0);
        assert_eq!(quantize(-0.1), 0.0);
        assert_eq!(quantize(5.0), 1.0);
    }

    #[test]
    fn a_bar_appearing_beats_the_throttle() {
        assert_eq!(
            flush_kind(&map(&[(ID, 0.5)]), &map(&[(ID, 0.5)])),
            Flush::Skip
        );
        assert_eq!(flush_kind(&map(&[(ID, 0.5)]), &map(&[])), Flush::Now);
        assert_eq!(flush_kind(&map(&[]), &map(&[(ID, 0.5)])), Flush::Now);
        assert_eq!(
            flush_kind(&map(&[(ID, 0.5)]), &map(&[("fun.wmde.term", 0.5)])),
            Flush::Now
        );
        assert_eq!(
            flush_kind(&map(&[(ID, 0.6)]), &map(&[(ID, 0.5)])),
            Flush::Deferred
        );
    }

    #[test]
    fn the_deadline_is_armed_once_and_does_not_slide() {
        // A sender emitting faster than the window must not push the flush out for ever: the
        // bar would freeze on its first value for the whole operation.
        let base = Instant::now();
        let mut throttle = Throttle::default();

        assert_eq!(
            throttle.on_snapshot(map(&[(ID, 0.1)]), base),
            Some(map(&[(ID, 0.1)])),
            "a bar appearing goes out at once"
        );
        assert_eq!(throttle.deadline(), None);

        assert_eq!(throttle.on_snapshot(map(&[(ID, 0.2)]), base), None);
        assert_eq!(throttle.deadline(), Some(base + THROTTLE));

        for (step, value) in [0.21_f32, 0.22, 0.23, 0.24].into_iter().enumerate() {
            let now = base + Duration::from_millis(20 * (step as u64 + 1));
            assert_eq!(throttle.on_snapshot(map(&[(ID, value)]), now), None);
            assert_eq!(
                throttle.deadline(),
                Some(base + THROTTLE),
                "the window is measured from the first deferred snapshot"
            );
        }

        // One flush for the whole storm, carrying the newest value.
        assert_eq!(throttle.on_deadline(), Some(map(&[(ID, 0.24)])));
        assert_eq!(throttle.deadline(), None);
        assert_eq!(throttle.on_deadline(), None);
    }

    #[test]
    fn a_bar_appearing_cancels_an_armed_deadline() {
        let base = Instant::now();
        let mut throttle = Throttle::default();

        assert_eq!(
            throttle.on_snapshot(map(&[(ID, 0.5)]), base),
            Some(map(&[(ID, 0.5)]))
        );
        assert_eq!(throttle.on_snapshot(map(&[(ID, 0.6)]), base), None);
        assert_eq!(throttle.deadline(), Some(base + THROTTLE));

        // The bar going out is visible, so it beats the window and takes the held snapshot
        // with it: what was waiting is older than what just happened.
        assert_eq!(throttle.on_snapshot(map(&[]), base), Some(map(&[])));
        assert_eq!(throttle.deadline(), None);
        assert_eq!(throttle.on_deadline(), None);
    }

    #[test]
    fn a_snapshot_back_at_the_sent_value_drops_the_deferred_one() {
        let base = Instant::now();
        let mut throttle = Throttle::default();

        assert_eq!(
            throttle.on_snapshot(map(&[(ID, 0.5)]), base),
            Some(map(&[(ID, 0.5)]))
        );
        assert_eq!(throttle.on_snapshot(map(&[(ID, 0.6)]), base), None);
        assert_eq!(throttle.on_snapshot(map(&[(ID, 0.5)]), base), None);
        assert_eq!(throttle.deadline(), None);
        assert_eq!(throttle.on_deadline(), None);
    }
}
