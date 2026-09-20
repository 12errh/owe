//! Hyprland's `socket2` event stream: the parser, the listener, and the replay
//! harness.
//!
//! ## The wire format, pinned
//!
//! Every line is `NAME>>PAYLOAD`, `\n`-terminated, and the payload is
//! comma-separated — with the trap that **titles, layout names and monitor
//! descriptions contain commas**. Splitting a payload on every comma is the
//! obvious implementation and it corrupts a window title the moment a user opens
//! a file called `report, final.pdf`. Each typed event below therefore splits a
//! *bounded* number of fields and keeps the last one whole.
//!
//! ## What is typed, what is not
//!
//! The events the governor needs (`activewindow`, `activewindowv2`, `fullscreen`,
//! `workspace`, `focusedmon`) plus the ones Hyprland documents unambiguously are
//! typed. Everything else becomes [`ShellEvent::Other`] with its payload intact:
//! a parser that silently drops unknown events is how a governor later misses the
//! one event that mattered, and a new Hyprland release adding an event must not
//! need this file edited to keep working.
//!
//! Anything that does not fit the format — no `>>`, a `fullscreen` payload that is
//! not `0`/`1`, a `workspace` id that is not a number — is reported as
//! [`ParsedLine::Malformed`] with the reason. It is never guessed at: a governor
//! acting on a misparsed `fullscreen` would pause the wallpaper at the wrong
//! moment, and nobody would connect the two.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use owe_core::shell::{EnvLookup, ShellEvent};

/// One line of the event stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedLine {
    /// A typed event.
    Event(Box<ShellEvent>),
    /// A blank line or a fixture comment. Not an error, and not an event.
    Ignored,
    /// A line this build cannot type confidently.
    Malformed {
        /// The line, as received (trimmed of its newline).
        line: String,
        /// Why it was rejected.
        why: String,
    },
}

/// Parse one `socket2` line.
///
/// Pure: no socket, no environment. That is what makes a recorded stream a
/// regression test rather than an anecdote.
pub fn parse_line(line: &str) -> ParsedLine {
    let line = line.trim_end_matches(['\r', '\n']);
    if line.trim().is_empty() || line.starts_with('#') {
        return ParsedLine::Ignored;
    }

    let Some((kind, payload)) = line.split_once(">>") else {
        return ParsedLine::Malformed {
            line: line.to_string(),
            why: "no `>>` separator".to_string(),
        };
    };
    let kind = kind.trim();
    if kind.is_empty() {
        return ParsedLine::Malformed {
            line: line.to_string(),
            why: "empty event name".to_string(),
        };
    }

    let event = match kind {
        "activewindow" => {
            // `class,title` — the title is everything after the first comma.
            if !payload.contains(',') {
                return malformed(line, "expected `class,title`");
            }
            let (class, title) = payload.split_once(',').unwrap_or((payload, ""));
            ShellEvent::ActiveWindow {
                class: class.to_string(),
                title: title.to_string(),
            }
        }
        "activewindowv2" => ShellEvent::ActiveWindowAddress {
            address: payload.to_string(),
        },
        "fullscreen" => match payload.trim() {
            "0" => ShellEvent::Fullscreen { on: false },
            "1" => ShellEvent::Fullscreen { on: true },
            other => return malformed(line, &format!("`fullscreen` payload `{other}` is not 0/1")),
        },
        "workspace" | "workspacev2" => match parse_workspace(payload) {
            Ok((id, name)) => ShellEvent::Workspace { id, name },
            Err(why) => return malformed(line, &why),
        },
        "createworkspace" | "createworkspacev2" => match parse_workspace(payload) {
            Ok((id, name)) => ShellEvent::WorkspaceCreated { id, name },
            Err(why) => return malformed(line, &why),
        },
        "destroyworkspace" | "destroyworkspacev2" => match parse_workspace(payload) {
            Ok((id, name)) => ShellEvent::WorkspaceDestroyed { id, name },
            Err(why) => return malformed(line, &why),
        },
        "focusedmon" | "focusedmonv2" => {
            if !payload.contains(',') {
                return malformed(line, "expected `monitor,workspace`");
            }
            let (monitor, workspace) = payload.split_once(',').unwrap_or((payload, ""));
            ShellEvent::FocusedMonitor {
                monitor: monitor.to_string(),
                workspace: workspace.to_string(),
            }
        }
        "monitoradded" => ShellEvent::MonitorAdded {
            id: None,
            monitor: payload.to_string(),
            description: None,
        },
        // `id,name,description` — a monitor description is "Dell Inc. DELL U2720Q"
        // and nothing stops a model name containing a comma, so the third field is
        // the rest of the line.
        "monitoraddedv2" => {
            let mut fields = payload.splitn(3, ',');
            let id = fields.next().unwrap_or_default();
            let monitor = fields.next();
            let Some(monitor) = monitor else {
                return malformed(line, "expected `id,name[,description]`");
            };
            let id = match id.trim().parse::<i64>() {
                Ok(id) => id,
                Err(_) => return malformed(line, &format!("monitor id `{id}` is not a number")),
            };
            ShellEvent::MonitorAdded {
                id: Some(id),
                monitor: monitor.to_string(),
                description: fields.next().map(str::to_string),
            }
        }
        "monitorremoved" => ShellEvent::MonitorRemoved {
            monitor: payload.to_string(),
        },
        // `address,workspace,class,title` — again, the title is the rest.
        "openwindow" => {
            let fields: Vec<&str> = payload.splitn(4, ',').collect();
            if fields.len() < 4 {
                return malformed(line, "expected `address,workspace,class,title`");
            }
            ShellEvent::OpenWindow {
                address: fields[0].to_string(),
                workspace: fields[1].to_string(),
                class: fields[2].to_string(),
                title: fields[3].to_string(),
            }
        }
        "closewindow" => ShellEvent::CloseWindow {
            address: payload.to_string(),
        },
        "windowtitle" | "windowtitlev2" => {
            if !payload.contains(',') {
                return malformed(line, "expected `address,title`");
            }
            let (address, title) = payload.split_once(',').unwrap_or((payload, ""));
            ShellEvent::WindowTitle {
                address: address.to_string(),
                title: title.to_string(),
            }
        }
        "submap" => ShellEvent::Submap {
            name: payload.to_string(),
        },
        "activelayout" => {
            if !payload.contains(',') {
                return malformed(line, "expected `keyboard,layout`");
            }
            let (keyboard, layout) = payload.split_once(',').unwrap_or((payload, ""));
            ShellEvent::ActiveLayout {
                keyboard: keyboard.to_string(),
                layout: layout.to_string(),
            }
        }
        "openlayer" => ShellEvent::LayerOpened {
            namespace: payload.to_string(),
        },
        "closelayer" => ShellEvent::LayerClosed {
            namespace: payload.to_string(),
        },
        "urgent" => ShellEvent::Urgent {
            address: payload.to_string(),
        },
        "configreloaded" => {
            if !payload.is_empty() {
                return malformed(line, "`configreloaded` takes no payload");
            }
            ShellEvent::ConfigReloaded
        }
        // Known-but-untyped and genuinely unknown both land here, whole.
        other => ShellEvent::Other {
            kind: other.to_string(),
            payload: payload.to_string(),
        },
    };

    ParsedLine::Event(Box::new(event))
}

fn malformed(line: &str, why: &str) -> ParsedLine {
    ParsedLine::Malformed {
        line: line.to_string(),
        why: why.to_string(),
    }
}

/// `id` or `id,name`.
fn parse_workspace(payload: &str) -> Result<(i64, Option<String>), String> {
    let (id, name) = match payload.split_once(',') {
        Some((id, name)) => (id, Some(name.to_string())),
        None => (payload, None),
    };
    match id.trim().parse::<i64>() {
        Ok(id) => Ok((id, name)),
        Err(_) => Err(format!("workspace id `{id}` is not a number")),
    }
}

/// Render an event back to its wire form.
///
/// Used by the replay harness to assert `parse(render(event)) == event` over a
/// whole recorded stream: a parser that quietly shifts a field fails that without
/// anyone having to hand-write an expectation for every line. The canonical form
/// prefers the `v2` spelling whenever the event carries the extra field, because
/// that is the message Hyprland actually sent.
pub fn to_wire(event: &ShellEvent) -> String {
    match event {
        ShellEvent::ActiveWindow { class, title } => format!("activewindow>>{class},{title}"),
        ShellEvent::ActiveWindowAddress { address } => format!("activewindowv2>>{address}"),
        ShellEvent::Fullscreen { on } => format!("fullscreen>>{}", i32::from(*on)),
        ShellEvent::Workspace { id, name } => match name {
            Some(name) => format!("workspacev2>>{id},{name}"),
            None => format!("workspace>>{id}"),
        },
        ShellEvent::FocusedMonitor { monitor, workspace } => {
            format!("focusedmon>>{monitor},{workspace}")
        }
        ShellEvent::MonitorAdded {
            id,
            monitor,
            description,
        } => match (id, description) {
            (Some(id), Some(description)) => {
                format!("monitoraddedv2>>{id},{monitor},{description}")
            }
            (Some(id), None) => format!("monitoraddedv2>>{id},{monitor}"),
            (None, _) => format!("monitoradded>>{monitor}"),
        },
        ShellEvent::MonitorRemoved { monitor } => format!("monitorremoved>>{monitor}"),
        ShellEvent::OpenWindow {
            address,
            workspace,
            class,
            title,
        } => format!("openwindow>>{address},{workspace},{class},{title}"),
        ShellEvent::CloseWindow { address } => format!("closewindow>>{address}"),
        ShellEvent::WindowTitle { address, title } => {
            format!("windowtitle>>{address},{title}")
        }
        ShellEvent::Submap { name } => format!("submap>>{name}"),
        ShellEvent::ActiveLayout { keyboard, layout } => {
            format!("activelayout>>{keyboard},{layout}")
        }
        ShellEvent::LayerOpened { namespace } => format!("openlayer>>{namespace}"),
        ShellEvent::LayerClosed { namespace } => format!("closelayer>>{namespace}"),
        ShellEvent::WorkspaceCreated { id, name } => match name {
            Some(name) => format!("createworkspacev2>>{id},{name}"),
            None => format!("createworkspace>>{id}"),
        },
        ShellEvent::WorkspaceDestroyed { id, name } => match name {
            Some(name) => format!("destroyworkspacev2>>{id},{name}"),
            None => format!("destroyworkspace>>{id}"),
        },
        ShellEvent::Urgent { address } => format!("urgent>>{address}"),
        ShellEvent::ConfigReloaded => "configreloaded>>".to_string(),
        ShellEvent::Other { kind, payload } => format!("{kind}>>{payload}"),
    }
}

/// Where the event socket lives for this session.
///
/// Hyprland puts it in `$XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE/`. It
/// used to be `/tmp/hypr/...`; both are checked, newest first, because a session
/// upgraded in place can leave an older socket behind.
pub fn socket_path(env: EnvLookup<'_>) -> Option<PathBuf> {
    let signature = env("HYPRLAND_INSTANCE_SIGNATURE")?;
    if signature.trim().is_empty() {
        return None;
    }
    let runtime = env("XDG_RUNTIME_DIR").unwrap_or_else(|| format!("/run/user/{}", fallback_uid()));
    let candidates = [
        Path::new(&runtime).join("hypr").join(&signature),
        PathBuf::from("/tmp/hypr").join(&signature),
    ];
    candidates
        .into_iter()
        .map(|dir| dir.join(".socket2.sock"))
        .find(|path| path.exists())
}

fn fallback_uid() -> u32 {
    // `XDG_RUNTIME_DIR` is set by every login session that can run a compositor,
    // so this is a last resort. Reading the real uid would mean a `libc` call or
    // spawning `id`; `$UID` is set by bash but not by every launcher, so an
    // unavailable value produces a path that does not exist — which the candidate
    // search then misses, rather than mistaking some other user's socket for this
    // session's.
    std::env::var("UID")
        .ok()
        .and_then(|uid| uid.parse().ok())
        .unwrap_or(0)
}

/// A live connection to the event socket.
#[derive(Debug)]
pub struct Socket2Stream {
    reader: BufReader<UnixStream>,
    path: PathBuf,
    /// Raw lines seen so far, for logs and counters.
    lines: u64,
}

impl Socket2Stream {
    /// Connect to a socket path.
    pub fn connect(path: &Path) -> std::io::Result<Self> {
        let stream = UnixStream::connect(path)?;
        Ok(Self {
            reader: BufReader::new(stream),
            path: path.to_path_buf(),
            lines: 0,
        })
    }

    /// Connect using the environment's session socket.
    pub fn connect_env(env: EnvLookup<'_>) -> Result<Self, String> {
        let path = socket_path(env).ok_or_else(|| {
            "no Hyprland event socket: HYPRLAND_INSTANCE_SIGNATURE is unset or the session \
             has no socket2 (a nested or non-Hyprland session has no event stream)"
                .to_string()
        })?;
        Self::connect(&path).map_err(|error| format!("cannot open {}: {error}", path.display()))
    }

    /// The socket this stream is reading.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Raw lines read so far.
    pub fn lines_read(&self) -> u64 {
        self.lines
    }

    /// The next line, blocking. `Ok(None)` means the socket closed (the shell
    /// exited or restarted) — a normal end, not an error.
    pub fn next_line(&mut self) -> std::io::Result<Option<String>> {
        let mut line = String::new();
        let read = self.reader.read_line(&mut line)?;
        if read == 0 {
            return Ok(None);
        }
        self.lines += 1;
        Ok(Some(line))
    }

    /// The next *typed* result, skipping blank lines and comments.
    pub fn next_event(&mut self) -> std::io::Result<Option<Result<ShellEvent, String>>> {
        loop {
            let Some(line) = self.next_line()? else {
                return Ok(None);
            };
            match parse_line(&line) {
                ParsedLine::Event(event) => return Ok(Some(Ok(*event))),
                ParsedLine::Ignored => continue,
                ParsedLine::Malformed { line, why } => {
                    return Ok(Some(Err(format!("{why} (line: {line})"))));
                }
            }
        }
    }
}

/// What a replay of a recorded stream found.
///
/// Deliberately a report rather than a `Result`: the gate is "10 000 events, zero
/// mismatches", and a report states both numbers in one place instead of failing
/// on the first problem and hiding the shape of the rest.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReplayReport {
    /// Lines that were neither blank nor comments.
    pub lines: u64,
    /// Lines that became a typed event.
    pub events: u64,
    /// Lines skipped as blank/comment.
    pub ignored: u64,
    /// Lines this build could not type.
    pub malformed: Vec<(String, String)>,
    /// Events that did not survive `parse(render(event)) == event`.
    pub round_trip_failures: Vec<String>,
    /// Lines per *wire* name, counted from the text and not from the typed event.
    ///
    /// That is the point: this histogram is the fixture's independent view of what
    /// went past. `activewindow` and `windowtitlev2` are keys here because they are
    /// what the socket said, not because of how this build chose to type them — so
    /// comparing it against the manifest the generator wrote is a real check rather
    /// than the parser agreeing with itself.
    pub histogram: BTreeMap<String, u64>,
    /// Wire names that arrived typed only as [`ShellEvent::Other`].
    pub untyped_kinds: std::collections::BTreeSet<String>,
    /// Events that carry environment state (`ShellEvent::is_state`).
    pub state_events: u64,
}

impl ReplayReport {
    /// Whether the stream parsed cleanly.
    pub fn is_clean(&self) -> bool {
        self.malformed.is_empty() && self.round_trip_failures.is_empty()
    }

    /// A one-line summary for logs and test output.
    pub fn summary(&self) -> String {
        format!(
            "{} line(s): {} event(s) ({} state), {} ignored, {} malformed, {} round-trip failures",
            self.lines,
            self.events,
            self.state_events,
            self.ignored,
            self.malformed.len(),
            self.round_trip_failures.len()
        )
    }
}

/// Replay a recorded stream through the parser.
///
/// Each event is parsed, re-rendered and parsed again — see [`to_wire`]. Two of the
/// three checks are deliberately parser-independent: the histogram is counted from
/// the raw text, so it can be compared against the generator's manifest, and
/// [`ReplayReport::untyped_kinds`] names what fell through to [`ShellEvent::Other`].
/// A parser change that quietly stops typing `windowtitle` would keep `events`,
/// `malformed` and the histogram identical — and move `windowtitle` into
/// `untyped_kinds`, where the test notices.
pub fn replay(text: &str) -> ReplayReport {
    let mut report = ReplayReport::default();

    for raw in text.lines() {
        match parse_line(raw) {
            ParsedLine::Ignored => report.ignored += 1,
            ParsedLine::Malformed { line, why } => {
                report.lines += 1;
                // Cap the list: a fixture of 10 000 broken lines should not
                // produce a 10 000-entry failure message.
                if report.malformed.len() < 16 {
                    report.malformed.push((line, why));
                }
            }
            ParsedLine::Event(event) => {
                report.lines += 1;
                report.events += 1;
                if event.is_state() {
                    report.state_events += 1;
                }
                if let Some((wire_kind, _)) = raw.split_once(">>") {
                    *report
                        .histogram
                        .entry(wire_kind.trim().to_string())
                        .or_default() += 1;
                }
                if matches!(*event, ShellEvent::Other { .. }) {
                    report.untyped_kinds.insert(event.kind().to_string());
                }

                let wire = to_wire(&event);
                match parse_line(&wire) {
                    ParsedLine::Event(again) if *again == *event => {}
                    other => {
                        if report.round_trip_failures.len() < 16 {
                            report
                                .round_trip_failures
                                .push(format!("{wire} re-parsed as {other:?}"));
                        }
                    }
                }
            }
        }
    }
    report
}

/// The `kind: count` manifest a synthesized fixture carries in its header.
///
/// Kept as a parser because the fixture's own expectations are the gate's
/// reference: the test compares the manifest the generator wrote against the
/// histogram the parser produces.
pub fn manifest_histogram(text: &str) -> BTreeMap<String, u64> {
    let mut histogram = BTreeMap::new();
    for line in text.lines() {
        let Some(rest) = line.trim().strip_prefix("#   ") else {
            continue;
        };
        let Some((kind, count)) = rest.split_once(": ") else {
            continue;
        };
        if let Ok(count) = count.trim().parse::<u64>() {
            histogram.insert(kind.trim().to_string(), count);
        }
    }
    histogram
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_with<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name: &str| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_string())
        }
    }

    fn event(line: &str) -> ShellEvent {
        match parse_line(line) {
            ParsedLine::Event(event) => *event,
            other => panic!("expected an event from `{line}`, got {other:?}"),
        }
    }

    // --- the samples: hand-written meanings ---------------------------------
    //
    // These are the lines whose *interpretation* matters, with the expected value
    // spelled out by hand. The 10k gate below proves scale and invariance; this
    // proves meaning, and neither substitutes for the other.

    #[test]
    fn an_active_window_keeps_its_title_whole() {
        // The title contains a comma. Splitting on every comma would report the
        // class as "kitty" and the title as "report" — off by a suffix, on every
        // window whose title has a comma in it.
        let parsed = event("activewindow>>kitty,report, final.pdf — nvim");
        assert_eq!(
            parsed,
            ShellEvent::ActiveWindow {
                class: "kitty".to_string(),
                title: "report, final.pdf — nvim".to_string(),
            }
        );
    }

    #[test]
    fn an_active_window_with_no_window_is_an_empty_pair() {
        // This is what Hyprland sends when focus lands on the desktop.
        assert_eq!(
            event("activewindow>>,"),
            ShellEvent::ActiveWindow {
                class: String::new(),
                title: String::new(),
            }
        );
    }

    #[test]
    fn a_missing_comma_is_reported_rather_than_guessed() {
        let parsed = parse_line("activewindow>>kitty");
        match parsed {
            ParsedLine::Malformed { why, .. } => {
                assert!(why.contains("class,title"), "{why}");
            }
            other => panic!("a payload with no comma must not be typed: {other:?}"),
        }
    }

    #[test]
    fn fullscreen_typing_is_exact() {
        assert_eq!(event("fullscreen>>0"), ShellEvent::Fullscreen { on: false });
        assert_eq!(event("fullscreen>>1"), ShellEvent::Fullscreen { on: true });
        // The governor pauses wallpapers on this event (PRD-F-21). A permissive
        // `!= "0"` would read `fullscreen>>2` as "yes"; a typo would read as "no".
        assert!(matches!(
            parse_line("fullscreen>>yes"),
            ParsedLine::Malformed { .. }
        ));
        assert!(matches!(
            parse_line("fullscreen>>2"),
            ParsedLine::Malformed { .. }
        ));
    }

    #[test]
    fn a_workspace_carries_its_name_when_the_shell_sends_one() {
        assert_eq!(
            event("workspace>>2"),
            ShellEvent::Workspace { id: 2, name: None }
        );
        assert_eq!(
            event("workspacev2>>2,web"),
            ShellEvent::Workspace {
                id: 2,
                name: Some("web".to_string())
            }
        );
        assert_eq!(
            event("workspace>>2,web"),
            ShellEvent::Workspace {
                id: 2,
                name: Some("web".to_string())
            }
        );
        // A workspace name can be anything the user typed, including a number and
        // a comma — only the id is parsed.
        assert_eq!(
            event("workspacev2>>3,1,2"),
            ShellEvent::Workspace {
                id: 3,
                name: Some("1,2".to_string())
            }
        );
        assert!(matches!(
            parse_line("workspace>>two"),
            ParsedLine::Malformed { .. }
        ));
    }

    #[test]
    fn a_monitor_description_keeps_its_commas() {
        assert_eq!(
            event("monitoraddedv2>>1,HDMI-A-1,Dell Inc. DELL U2720Q, rev A00"),
            ShellEvent::MonitorAdded {
                id: Some(1),
                monitor: "HDMI-A-1".to_string(),
                description: Some("Dell Inc. DELL U2720Q, rev A00".to_string()),
            }
        );
        // The v1 form has no id and no description; that is not an error.
        assert_eq!(
            event("monitoradded>>HDMI-A-1"),
            ShellEvent::MonitorAdded {
                id: None,
                monitor: "HDMI-A-1".to_string(),
                description: None,
            }
        );
        assert!(matches!(
            parse_line("monitoraddedv2>>1"),
            ParsedLine::Malformed { .. }
        ));
    }

    #[test]
    fn an_opened_window_keeps_its_title_whole() {
        assert_eq!(
            event("openwindow>>55b1c0e0e2a0,2,kitty,make, then commit"),
            ShellEvent::OpenWindow {
                address: "55b1c0e0e2a0".to_string(),
                workspace: "2".to_string(),
                class: "kitty".to_string(),
                title: "make, then commit".to_string(),
            }
        );
        assert!(matches!(
            parse_line("openwindow>>addr,2,kitty"),
            ParsedLine::Malformed { .. }
        ));
    }

    #[test]
    fn the_empty_submap_is_the_default_one_not_an_error() {
        // Recorded from a live session: `submap>>` is what resetting to the
        // default submap looks like.
        assert_eq!(
            event("submap>>"),
            ShellEvent::Submap {
                name: String::new()
            }
        );
        assert_eq!(
            event("submap>>resize"),
            ShellEvent::Submap {
                name: "resize".to_string()
            }
        );
    }

    #[test]
    fn an_unknown_event_is_preserved_whole() {
        // A Hyprland release that adds an event must not need this file edited,
        // and must not have its event silently dropped either.
        assert_eq!(
            event("screencopy>>monitor,eDP-1"),
            ShellEvent::Other {
                kind: "screencopy".to_string(),
                payload: "monitor,eDP-1".to_string(),
            }
        );
        assert_eq!(
            to_wire(&event("screencopy>>monitor,eDP-1")),
            "screencopy>>monitor,eDP-1"
        );
    }

    #[test]
    fn structural_damage_is_malformed() {
        assert!(matches!(
            parse_line("this is not an event"),
            ParsedLine::Malformed { .. }
        ));
        assert!(matches!(
            parse_line(">>payload"),
            ParsedLine::Malformed { .. }
        ));
        assert!(matches!(
            parse_line("configreloaded>>surprise"),
            ParsedLine::Malformed { .. }
        ));
    }

    #[test]
    fn blank_lines_and_comments_are_ignored_not_malformed() {
        assert_eq!(parse_line(""), ParsedLine::Ignored);
        assert_eq!(parse_line("   \n"), ParsedLine::Ignored);
        assert_eq!(parse_line("# a manifest line"), ParsedLine::Ignored);
    }

    #[test]
    fn state_events_are_distinguished_from_noise() {
        assert!(event("fullscreen>>1").is_state());
        assert!(event("workspacev2>>2,web").is_state());
        assert!(event("monitoradded>>DP-1").is_state());
        // A window title changes on every keystroke in a terminal. P6's governor
        // must not wake for it.
        assert!(!event("windowtitle>>addr,~/x — nvim").is_state());
        assert!(!event("configreloaded>>").is_state());
    }

    #[test]
    fn round_trip_holds_for_every_typed_variant() {
        for line in [
            "activewindow>>kitty,title, with comma",
            "activewindowv2>>55b1c0e0e2a0",
            "fullscreen>>1",
            "workspace>>2",
            "workspacev2>>2,web",
            "focusedmon>>eDP-1,2",
            "monitoradded>>HDMI-A-1",
            "monitoraddedv2>>1,HDMI-A-1,Dell Inc. DELL U2720Q",
            "monitoraddedv2>>1,HDMI-A-1",
            "monitorremoved>>HDMI-A-1",
            "openwindow>>addr,2,kitty,title",
            "closewindow>>addr",
            "windowtitle>>addr,title, with comma",
            "submap>>",
            "activelayout>>kbd,English (US)",
            "openlayer>>notifications",
            "closelayer>>notifications",
            "createworkspace>>3",
            "createworkspacev2>>3,web",
            "destroyworkspace>>3",
            "destroyworkspacev2>>3,web",
            "urgent>>addr",
            "configreloaded>>",
        ] {
            let once = event(line);
            let again = event(&to_wire(&once));
            assert_eq!(once, again, "round trip changed `{line}`");
        }
    }

    // --- the socket path ----------------------------------------------------

    #[test]
    fn the_socket_path_needs_a_session_signature() {
        assert!(socket_path(&env_with(&[])).is_none());
        assert!(socket_path(&env_with(&[("HYPRLAND_INSTANCE_SIGNATURE", "  ")])).is_none());
        // With a signature but no such session, there is no path: the candidate
        // list is checked against the filesystem.
        assert!(
            socket_path(&env_with(&[
                ("HYPRLAND_INSTANCE_SIGNATURE", "deadbeef_1_2"),
                ("XDG_RUNTIME_DIR", "/run/user/1984"),
            ]))
            .is_none()
        );
    }

    #[test]
    fn connecting_without_a_session_explains_itself() {
        let error = Socket2Stream::connect_env(&env_with(&[])).unwrap_err();
        assert!(error.contains("HYPRLAND_INSTANCE_SIGNATURE"), "{error}");
        assert!(
            error.contains("nested") || error.contains("no event stream"),
            "the message must say when this is normal: {error}"
        );
    }

    // --- the recorded fixtures ----------------------------------------------

    const SESSION: &str = include_str!("../tests/fixtures/events/socket2-session.log");
    const TEN_THOUSAND: &str = include_str!("../tests/fixtures/events/socket2-10k.log");

    #[test]
    fn the_recorded_session_parses_without_a_single_mismatch() {
        let report = replay(SESSION);
        assert!(
            report.is_clean(),
            "recorded session did not parse cleanly: {} — {:?}",
            report.summary(),
            report.malformed
        );
        assert!(
            report.events > 0,
            "the fixture is empty, which would make this test vacuous: {}",
            report.summary()
        );
    }

    #[test]
    fn ten_thousand_recorded_events_parse_with_zero_mismatches() {
        let report = replay(TEN_THOUSAND);
        assert_eq!(report.events, 10_000, "{}", report.summary());
        assert_eq!(report.lines, 10_000, "{}", report.summary());
        assert!(
            report.is_clean(),
            "{} — malformed {:?} — round trip {:?}",
            report.summary(),
            report.malformed,
            report.round_trip_failures
        );

        // Against the manifest the generator wrote, not against itself: this is
        // what catches a parser change that reclassifies lines while keeping the
        // totals identical.
        let expected = manifest_histogram(TEN_THOUSAND);
        assert!(
            !expected.is_empty(),
            "the fixture must carry a manifest histogram"
        );
        assert_eq!(
            report.histogram, expected,
            "the parsed histogram does not match the fixture's manifest"
        );
    }

    #[test]
    fn the_ten_thousand_fixture_covers_the_governor_events() {
        // Scale is not coverage: 10 000 copies of one line would pass the gate
        // above and prove nothing. These are the events P6's rules depend on.
        let report = replay(TEN_THOUSAND);
        for kind in [
            "activewindow",
            "activewindowv2",
            "fullscreen",
            "workspace",
            "focusedmon",
            "monitoradded",
            "monitorremoved",
        ] {
            assert!(
                report.histogram.get(kind).copied().unwrap_or(0) > 0,
                "the fixture must exercise `{kind}`: {:?}",
                report.histogram
            );
        }
        assert!(
            report.state_events * 2 > report.events,
            "most recorded events should carry state, not titles: {}",
            report.summary()
        );
    }

    #[test]
    fn only_the_documented_kinds_fall_through_untyped() {
        // The other half of the histogram check. Reclassification keeps every count
        // identical (`Other` reports the same kind name), so the thing to assert is
        // which names are *only* ever preserved raw.
        let report = replay(TEN_THOUSAND);
        let expected: std::collections::BTreeSet<String> = [
            "changefloatingmode",
            "movewindow",
            "minimize",
            "pin",
            "renameworkspace",
            "togglegroup",
        ]
        .iter()
        .map(|kind| (*kind).to_string())
        .collect();
        assert_eq!(
            report.untyped_kinds, expected,
            "a kind typed as `Other` here is either a regression (it was typed before) or \
             a new event the catalogue should cover"
        );
    }

    #[test]
    fn the_manifest_parser_reads_what_the_generator_writes() {
        let text = "#   activewindow: 12\n#   fullscreen: 3\nnot a manifest\n";
        let histogram = manifest_histogram(text);
        assert_eq!(histogram.get("activewindow"), Some(&12));
        assert_eq!(histogram.get("fullscreen"), Some(&3));
        assert_eq!(histogram.len(), 2);
    }

    #[test]
    fn a_partly_typed_stream_reports_its_damage_instead_of_hiding_it() {
        let report = replay("fullscreen>>1\nnonsense\nworkspace>>2\n");
        assert_eq!(report.events, 2);
        assert_eq!(report.malformed.len(), 1);
        assert!(!report.is_clean());
        assert!(
            report.summary().contains("1 malformed"),
            "{}",
            report.summary()
        );
    }
}
