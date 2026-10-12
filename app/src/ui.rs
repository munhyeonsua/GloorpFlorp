//! The window (egui). It only reads the engine's `Snapshot` and sends
//! `Command`s, so it never waits for the network, FFmpeg or audio. The
//! optional lobby works the same way through `gf_lobby::client` (its own
//! threads); lobby invites are built on the room commands here.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align, Color32, ComboBox, Grid, Layout, ProgressBar, RichText, Slider, TextEdit, Ui,
};
use gf_lobby::client::{Client, State as LobbyState};
use gf_lobby::{unix_now, Presence, ToClient, ToServer, MAX_TEXT};
use gf_proto::VideoCodec;
use gf_sys::AudioApp;
use gloorpflorp::engine::{
    Command, Engine, EngineOptions, Phase, ShareState, Snapshot, NO_ENCODER,
};
use gloorpflorp::settings::{
    default_bitrate_mbps, default_name, CodecChoice, GameAudioMode, Settings, VoiceModeChoice,
    FPS_CHOICES, HEIGHT_CHOICES, MAX_BITRATE_MBPS, MIN_BITRATE_MBPS,
};

const GREEN: Color32 = Color32::from_rgb(90, 200, 110);
const YELLOW: Color32 = Color32::from_rgb(230, 190, 70);
const ORANGE: Color32 = Color32::from_rgb(240, 140, 60);
const GREY: Color32 = Color32::from_rgb(150, 150, 150);
const RED_BG: Color32 = Color32::from_rgb(120, 30, 30);
const INFO: Color32 = Color32::from_rgb(140, 180, 230);
const INVITE_BG: Color32 = Color32::from_rgb(30, 60, 110);

/// An unanswered invite is dropped (and the inviter's room closed) after this.
const INVITE_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a lobby note ("Bob declined.") stays.
const NOTE_TIME: Duration = Duration::from_secs(15);

const PTT_BLOCKED: &str = "The program in front runs as administrator, so Windows hides your \
                           push-to-talk key from GloorpFlorp. Run GloorpFlorp as administrator \
                           too, or use Voice activation.";
const SHORTCUT_TIP: &str = "Click, then press the key or mouse button that switches this on and \
                            off. Works while the game has focus or GloorpFlorp is minimized, \
                            unless the game runs as administrator. The game still gets the key \
                            too, so pick one you don't use in the game.";

/// A key that can be bound with "click, then press a key".
#[derive(Clone, Copy, PartialEq, Eq)]
enum BindTarget {
    PushToTalk,
    Mute,
    Deafen,
}

impl BindTarget {
    const ALL: [BindTarget; 3] = [BindTarget::PushToTalk, BindTarget::Mute, BindTarget::Deafen];

    fn label(self) -> &'static str {
        match self {
            BindTarget::PushToTalk => "Push-to-talk",
            BindTarget::Mute => "Mute",
            BindTarget::Deafen => "Deafen",
        }
    }

    fn key(self, s: &Settings) -> Option<u32> {
        match self {
            BindTarget::PushToTalk => Some(s.audio.ptt_key),
            BindTarget::Mute => s.mute_key,
            BindTarget::Deafen => s.deafen_key,
        }
    }
}

/// Key capture in progress.
struct KeyBinding {
    target: BindTarget,
    /// Becomes true once no key is held, so a key that was already down
    /// (e.g. the one that clicked the button) isn't taken.
    armed: bool,
    /// Why the last key pressed wasn't taken (another binding has it).
    refused: Option<String>,
}

/// An invite sent through the lobby (or to send once my room is open).
struct Outgoing {
    to: String,
    name: String,
    /// The room code it was sent with.
    code: Option<String>,
    since: Instant,
}

/// An invite waiting for my answer.
struct Incoming {
    from: String,
    name: String,
    code: String,
    since: Instant,
}

#[derive(Debug, PartialEq)]
enum InviteStep {
    /// The room is opening, or the invite waits for the answer.
    Wait,
    /// The room is open: send the invite with this code.
    Send(String),
    /// No answer in time: close the room.
    TimedOut,
    /// They joined, or the room closed, changed or didn't open.
    Done,
}

/// What my invite needs, given the room's phase, the code it was sent with
/// and how long ago it was clicked (or sent).
fn invite_step(phase: &Phase, sent: Option<&str>, age: Duration) -> InviteStep {
    match (phase, sent) {
        (Phase::Hosting { code, waiting }, None) if *waiting && !code.is_empty() => {
            InviteStep::Send(code.clone())
        }
        (Phase::Hosting { code, .. }, Some(sent)) if code == sent => {
            if age > INVITE_TIMEOUT {
                InviteStep::TimedOut
            } else {
                InviteStep::Wait
            }
        }
        (Phase::Hosting { .. }, None) => InviteStep::Wait,
        // The engine hasn't seen the Host command yet.
        (Phase::Idle, None) if age < Duration::from_secs(1) => InviteStep::Wait,
        _ => InviteStep::Done,
    }
}

pub struct GloorpApp {
    engine: Engine,
    snap: Snapshot,
    settings: Settings,
    /// What the engine last got, to send only changes.
    sent: Settings,
    join_code: String,
    binding: Option<KeyBinding>,
    /// Shown until dismissed (e.g. the settings file was damaged).
    notice: Option<String>,
    copied_at: Option<Instant>,
    /// The program last picked for "Only one program" (kept while
    /// "Everything except GloorpFlorp" is selected).
    last_app: String,
    /// The name used when "Your name" is left empty (this PC's name).
    default_name: String,
    commands: Vec<Command>,
    /// The lobby connection (none without a lobby address), and the address
    /// and name it was started with.
    lobby: Option<Client>,
    lobby_for: (String, String),
    lobby_state: LobbyState,
    chat_input: String,
    outgoing: Option<Outgoing>,
    incoming: Option<Incoming>,
    lobby_note: Option<(String, Instant)>,
}

impl GloorpApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        settings: Settings,
        notice: Option<String>,
        settings_path: Option<PathBuf>,
        dev: EngineOptions,
    ) -> anyhow::Result<GloorpApp> {
        cc.egui_ctx.set_theme(egui::Theme::Dark);
        cc.egui_ctx.global_style_mut(|s| {
            s.spacing.item_spacing = egui::vec2(8.0, 6.0);
        });
        let ctx = cc.egui_ctx.clone();
        let engine = Engine::start(
            settings.clone(),
            EngineOptions {
                settings_path,
                on_change: Some(Arc::new(move || ctx.request_repaint())),
                ..dev
            },
        )?;
        Ok(GloorpApp {
            engine,
            snap: Snapshot::default(),
            join_code: settings.last_code.clone(),
            sent: settings.clone(),
            binding: None,
            notice,
            copied_at: None,
            last_app: match &settings.game_audio {
                GameAudioMode::App { exe } => exe.clone(),
                GameAudioMode::AllExceptThisApp => String::new(),
            },
            settings,
            default_name: default_name(),
            commands: Vec::new(),
            lobby: None,
            lobby_for: Default::default(),
            lobby_state: Default::default(),
            chat_input: String::new(),
            outgoing: None,
            incoming: None,
            lobby_note: None,
        })
    }

    fn cmd(&mut self, c: Command) {
        self.commands.push(c);
    }

    fn connected(&self) -> bool {
        matches!(self.snap.phase, Phase::Connected { .. })
    }

    fn peer(&self) -> String {
        self.snap
            .peer_name
            .clone()
            .unwrap_or_else(|| "Your friend".into())
    }

    // ------------------------------------------------------------ top

    fn status_line(&self, ui: &mut Ui) {
        let (text, color) = match &self.snap.phase {
            Phase::Idle => ("Not connected".to_string(), GREY),
            Phase::Hosting { waiting: false, .. } => ("Opening a room…".to_string(), YELLOW),
            Phase::Hosting { .. } => ("Waiting for your friend…".to_string(), YELLOW),
            Phase::Joining => ("Connecting to your friend…".to_string(), YELLOW),
            Phase::Connected { peer } => {
                let ping = match self.snap.net.rtt_ms {
                    Some(ms) => format!(" (ping {} ms)", ms.round() as u32),
                    None => String::new(),
                };
                (format!("Connected to {}{ping}", shorten(peer, 24)), GREEN)
            }
            Phase::Reconnecting => ("Reconnecting…".to_string(), ORANGE),
        };
        ui.horizontal(|ui| {
            dot(ui, color, 18.0);
            ui.add(egui::Label::new(RichText::new(text).color(color).size(18.0).strong()).wrap());
        });
    }

    fn banners(&mut self, ui: &mut Ui) {
        if !self.engine.is_running() {
            banner(
                ui,
                "Something went wrong inside GloorpFlorp. Close it and start it again. \
                 (Details are in the log file.)",
                None,
            );
        }
        if let Some(err) = self.snap.error.clone() {
            if banner(ui, &err, Some("Hide this message")) {
                self.cmd(Command::DismissError);
            }
        }
        if let Some(n) = self.notice.clone() {
            if banner(ui, &n, Some("Hide this message")) {
                self.notice = None;
            }
        }
        if let Some(info) = &self.snap.info {
            ui.label(RichText::new(info).color(INFO));
        }
    }

    // ------------------------------------------------------------ room

    fn room(&mut self, ui: &mut Ui) {
        ui.heading("Room");
        ui.horizontal(|ui| {
            let label = ui.label("Your name:");
            let r = ui
                .add(
                    TextEdit::singleline(&mut self.settings.name)
                        .hint_text(self.default_name.as_str())
                        .desired_width(200.0)
                        .char_limit(40),
                )
                .labelled_by(label.id)
                .on_hover_text(
                    "Your friend sees this name. Changes apply the next time you host or join.",
                );
            accept_set_value(&r, &mut self.settings.name, 40);
        });
        match self.snap.phase.clone() {
            Phase::Idle => {
                if ui
                    .button(RichText::new("Host").size(16.0))
                    .on_hover_text(
                        "Open a room on this PC and get a code to send to your friend. \
                         Needs Tailscale running and signed in.",
                    )
                    .clicked()
                {
                    self.cmd(Command::Host);
                }
                let label = ui.label("or paste your friend's room code:");
                ui.horizontal(|ui| {
                    let edit = ui
                        .add(
                            TextEdit::singleline(&mut self.join_code)
                                .hint_text("K7QM-2XPA-9TD")
                                .desired_width(170.0)
                                .char_limit(40)
                                .font(egui::TextStyle::Monospace),
                        )
                        .labelled_by(label.id)
                        .on_hover_text(
                            "The code your friend got when they clicked Host. \
                             Upper or lower case, spaces and dashes don't matter.",
                        );
                    accept_set_value(&edit, &mut self.join_code, 40);
                    let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    let can_join = !self.join_code.trim().is_empty();
                    let join = ui
                        .add_enabled(can_join, egui::Button::new("Join"))
                        .on_hover_text("Connect to your friend's room.")
                        .on_disabled_hover_text("Paste your friend's room code first.")
                        .clicked();
                    if can_join && (join || enter) {
                        self.settings.last_code = self.join_code.trim().to_string();
                        self.cmd(Command::Join(self.join_code.clone()));
                    }
                });
            }
            Phase::Hosting { code, waiting } => {
                if !waiting || code.is_empty() {
                    // The status line says "Opening a room…".
                    ui.spinner();
                } else {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&code).monospace().size(28.0).strong());
                        let copied = self
                            .copied_at
                            .is_some_and(|t| t.elapsed() < Duration::from_secs(2));
                        if ui
                            .button(if copied { "Copied!" } else { "Copy" })
                            .on_hover_text("Copy the code so you can paste it into a chat.")
                            .clicked()
                        {
                            ui.ctx().copy_text(code.clone());
                            self.copied_at = Some(Instant::now());
                        }
                    });
                    ui.label(
                        "Send this code to your friend. Keep GloorpFlorp open until they join.",
                    );
                }
                ui.horizontal(|ui| {
                    if ui
                        .button("New code")
                        .on_hover_text("Close this room and open a new one with a new code.")
                        .clicked()
                    {
                        self.cmd(Command::Host);
                    }
                    if ui
                        .button("Leave")
                        .on_hover_text("Close the room. Your friend can't join any more. Also stops your sharing and game audio.")
                        .clicked()
                    {
                        self.cmd(Command::Leave);
                    }
                });
            }
            Phase::Joining => {
                // The status line says "Connecting to your friend…".
                ui.spinner();
                if ui
                    .button("Cancel")
                    .on_hover_text("Stop trying to join.")
                    .clicked()
                {
                    self.cmd(Command::Leave);
                }
            }
            Phase::Connected { peer } => {
                ui.label(format!("You're in a room with {peer}."));
                if let Some(code) = &self.snap.room_code {
                    ui.label(RichText::new(format!("Room code: {code}")).weak())
                        .on_hover_text(
                            "Your friend can rejoin with this code while GloorpFlorp stays open.",
                        );
                }
                if ui
                    .button("Leave")
                    .on_hover_text(
                        "Disconnect from your friend. Also stops your sharing and game audio.",
                    )
                    .clicked()
                {
                    self.cmd(Command::Leave);
                }
            }
            Phase::Reconnecting => {
                ui.horizontal_wrapped(|ui| {
                    ui.spinner();
                    ui.label("The connection dropped. Trying to reconnect for up to 30 seconds…");
                });
                if ui
                    .button("Leave")
                    .on_hover_text(
                        "Stop trying to reconnect. Also stops your sharing and game audio.",
                    )
                    .clicked()
                {
                    self.cmd(Command::Leave);
                }
            }
        }
    }

    // ------------------------------------------------------------ lobby

    /// Keeps the lobby connection in line with the settings, sends my invite
    /// once my room is open, and handles invites and answers.
    fn poll_lobby(&mut self, ctx: &egui::Context) {
        let addr = self.settings.lobby.trim().to_string();
        let name = match self.settings.name.trim() {
            "" => self.default_name.clone(),
            n => n.to_string(),
        };
        // Not while typing, so it doesn't reconnect at every key.
        if (&addr, &name) != (&self.lobby_for.0, &self.lobby_for.1) && !ctx.text_edit_focused() {
            let repaint = ctx.clone();
            self.lobby = (!addr.is_empty())
                .then(|| Client::start(&addr, &name, Arc::new(move || repaint.request_repaint())));
            self.lobby_for = (addr, name);
            self.lobby_state = LobbyState::default();
            self.outgoing = None;
            self.incoming = None;
        }
        let Some(lobby) = &self.lobby else { return };
        self.lobby_state = lobby.state();
        let now = Instant::now();
        let mut note = None;
        let mut done = false;
        if let Some(o) = &mut self.outgoing {
            let age = now.duration_since(o.since);
            match invite_step(&self.snap.phase, o.code.as_deref(), age) {
                InviteStep::Wait => {}
                InviteStep::Send(code) if self.lobby_state.connected => {
                    lobby.send(ToServer::Invite {
                        to: o.to.clone(),
                        code: code.clone(),
                    });
                    o.code = Some(code);
                    o.since = now;
                }
                // It would be dropped, and the room would wait for nothing.
                InviteStep::Send(_) => {
                    note = Some(format!(
                        "Lost the connection to the lobby, so {} wasn't invited and the room \
                         was closed.",
                        o.name
                    ));
                    self.commands.push(Command::Leave);
                    done = true;
                }
                InviteStep::TimedOut => {
                    note = Some(format!("{} didn't answer, so the room was closed.", o.name));
                    self.commands.push(Command::Leave);
                    done = true;
                }
                InviteStep::Done => done = true,
            }
        }
        if done {
            // Ended without an answer: take it back, so their box doesn't
            // offer a room that is gone (once they joined they have no box).
            if let Some(Outgoing {
                to,
                code: Some(code),
                ..
            }) = self.outgoing.take()
            {
                lobby.send(ToServer::Cancel { to, code });
            }
        }
        for ev in lobby.events.try_iter() {
            match ev {
                ToClient::Invited { from, name, code } => {
                    // A newer invite from the same friend replaces theirs.
                    let free = self.incoming.as_ref().is_none_or(|i| i.from == from);
                    if self.snap.phase == Phase::Idle && free {
                        ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(
                            egui::UserAttentionType::Informational,
                        ));
                        self.incoming = Some(Incoming {
                            from,
                            name,
                            code,
                            since: now,
                        });
                    } else {
                        // Busy: don't interrupt a call.
                        lobby.send(ToServer::Reply {
                            to: from,
                            code,
                            accept: false,
                        });
                    }
                }
                ToClient::Replied { from, code, accept } => {
                    let mine = |o: &mut Outgoing| o.to == from && o.code.as_ref() == Some(&code);
                    if let Some(o) = self.outgoing.take_if(mine) {
                        if !accept {
                            note = Some(format!("{} declined.", o.name));
                            if matches!(self.snap.phase, Phase::Hosting { .. }) {
                                self.commands.push(Command::Leave);
                            }
                        }
                    }
                }
                ToClient::InviteFailed { to, code, text } => {
                    let mine = |o: &mut Outgoing| o.to == to && o.code.as_ref() == Some(&code);
                    if let Some(o) = self.outgoing.take_if(mine) {
                        note = Some(format!(
                            "{} wasn't invited, so the room was closed. {text}",
                            o.name
                        ));
                        if matches!(self.snap.phase, Phase::Hosting { .. }) {
                            self.commands.push(Command::Leave);
                        }
                    }
                }
                ToClient::Cancelled { from, code } => {
                    if let Some(i) = self.incoming.take_if(|i| i.from == from && i.code == code) {
                        note = Some(format!("{} closed the room.", i.name));
                    }
                }
                ToClient::Error { text } => note = Some(text),
                _ => {}
            }
        }
        self.incoming
            .take_if(|i| now.duration_since(i.since) > INVITE_TIMEOUT);
        if let Some(n) = note {
            self.lobby_note = Some((n, now));
        }
    }

    fn invite_prompt(&mut self, ui: &mut Ui) {
        let Some(inv) = &self.incoming else { return };
        let mut answer = None;
        egui::Frame::new()
            .fill(INVITE_BG)
            .inner_margin(8)
            .corner_radius(4)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(
                    RichText::new(format!("{} invites you to a room.", inv.name))
                        .color(Color32::WHITE)
                        .size(16.0)
                        .strong(),
                );
                ui.horizontal(|ui| {
                    if ui
                        .button("Accept")
                        .on_hover_text(
                            "Join their room now. Leaves your own room, if you have one.",
                        )
                        .clicked()
                    {
                        answer = Some(true);
                    }
                    if ui
                        .button("Deny")
                        .on_hover_text("Tell them you can't right now.")
                        .clicked()
                    {
                        answer = Some(false);
                    }
                });
            });
        let Some(accept) = answer else { return };
        if let Some(inv) = self.incoming.take() {
            if let Some(lobby) = &self.lobby {
                lobby.send(ToServer::Reply {
                    to: inv.from,
                    code: inv.code.clone(),
                    accept,
                });
            }
            if accept {
                self.cmd(Command::Join(inv.code));
            }
        }
    }

    fn lobby(&mut self, ui: &mut Ui) {
        egui::CollapsingHeader::new(RichText::new("Lobby").heading())
            .default_open(!self.settings.lobby.trim().is_empty())
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let label = ui.label("Lobby server:");
                    let r = ui
                        .add(
                            TextEdit::singleline(&mut self.settings.lobby)
                                .hint_text("gloorp-lobby:47900")
                                .desired_width(200.0)
                                .char_limit(200),
                        )
                        .labelled_by(label.id)
                        .on_hover_text(
                            "Optional: the address of your own GloorpFlorp lobby server in your \
                             Tailscale network, for chat, seeing who is online, and invites. \
                             Leave it empty if you don't have one.",
                        );
                    accept_set_value(&r, &mut self.settings.lobby, 200);
                });
                if self.lobby.is_some() {
                    self.lobby_contents(ui);
                }
            });
    }

    fn lobby_contents(&mut self, ui: &mut Ui) {
        let s = &self.lobby_state;
        ui.horizontal(|ui| {
            let (text, color) = if s.connected {
                ("Connected to the lobby", GREEN)
            } else {
                ("Connecting to the lobby…", YELLOW)
            };
            dot(ui, color, 12.0);
            ui.label(RichText::new(text).color(color));
        });
        if let Some(p) = s.problem.as_ref().filter(|_| !s.connected) {
            ui.label(RichText::new(p).color(ORANGE));
        }
        let can_invite = self.snap.phase == Phase::Idle && self.outgoing.is_none();
        let mut invite = None;
        Grid::new("lobby-members")
            .num_columns(3)
            .spacing([10.0, 4.0])
            .show(ui, |ui| {
                for m in &s.members {
                    let (color, state, tip) = match m.state {
                        Presence::Lobby => (
                            GREEN,
                            "In the lobby",
                            "GloorpFlorp is open and connected to the lobby.",
                        ),
                        Presence::Online => (
                            YELLOW,
                            "Online",
                            "Their PC is on in Tailscale, but GloorpFlorp isn't in the lobby.",
                        ),
                        Presence::Offline => (
                            GREY,
                            "Offline",
                            "Their PC is off or not connected to Tailscale.",
                        ),
                    };
                    ui.horizontal(|ui| {
                        dot(ui, color, 12.0);
                        ui.label(shorten(&m.name, 24));
                    });
                    ui.label(RichText::new(state).color(color))
                        .on_hover_text(tip);
                    if m.id == s.me {
                        ui.label(RichText::new("(you)").weak());
                    } else if m.state == Presence::Lobby {
                        let busy = if self.outgoing.is_some() {
                            "Wait for the answer to your invite first."
                        } else {
                            "Leave your room first."
                        };
                        let r = ui
                            .add_enabled(can_invite, egui::Button::new("Invite"))
                            .on_hover_text(format!("Open a room and ask {} to join it.", m.name))
                            .on_disabled_hover_text(busy);
                        if r.clicked() {
                            invite = Some(m.clone());
                        }
                    } else {
                        ui.label("");
                    }
                    ui.end_row();
                }
            });
        if let Some(o) = &self.outgoing {
            let text = match o.code {
                None => format!("Opening a room for {}…", o.name),
                Some(_) => format!("Waiting for {} to answer…", o.name),
            };
            ui.label(RichText::new(text).color(YELLOW));
        }
        if let Some((note, at)) = &self.lobby_note {
            if at.elapsed() < NOTE_TIME {
                ui.label(RichText::new(note).color(INFO));
            }
        }

        egui::ScrollArea::vertical()
            .id_salt("lobby-chat")
            .max_height(160.0)
            .stick_to_bottom(true)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                if s.chat.is_empty() {
                    ui.label(RichText::new("No messages yet.").weak());
                }
                let now = unix_now();
                for c in &s.chat {
                    ui.horizontal_wrapped(|ui| {
                        if c.at > 0 {
                            ui.label(RichText::new(ago(c.at, now)).weak());
                        }
                        ui.label(RichText::new(format!("{}:", shorten(&c.name, 24))).strong());
                        ui.label(&c.text);
                    });
                }
            });
        let mut send = None;
        ui.horizontal(|ui| {
            let edit = ui.add(
                TextEdit::singleline(&mut self.chat_input)
                    .hint_text("Message")
                    .desired_width(260.0)
                    .char_limit(MAX_TEXT),
            );
            set_accessible_name(&edit, "Chat message");
            accept_set_value(&edit, &mut self.chat_input, MAX_TEXT);
            let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let can_send = s.connected && !self.chat_input.trim().is_empty();
            let clicked = ui
                .add_enabled(can_send, egui::Button::new("Send"))
                .on_hover_text("Send to everyone in the lobby. Enter sends too.")
                .on_disabled_hover_text("Type a message first (and wait until you're connected).")
                .clicked();
            if can_send && (clicked || enter) {
                send = Some(std::mem::take(&mut self.chat_input));
                if enter {
                    edit.request_focus();
                }
            }
        });
        if let (Some(text), Some(lobby)) = (send, &self.lobby) {
            lobby.send(ToServer::Chat { text });
        }
        if let Some(m) = invite {
            self.lobby_note = None;
            self.outgoing = Some(Outgoing {
                to: m.id,
                name: m.name,
                code: None,
                since: Instant::now(),
            });
            self.cmd(Command::Host);
        }
    }

    // ------------------------------------------------------------ my screen

    fn my_screen(&mut self, ui: &mut Ui) {
        ui.heading("Your screen");
        let snap = &self.snap;
        let s = &mut self.settings;
        let no_encoder =
            snap.video_available && !snap.probing_encoders && snap.my_encoders.is_empty();
        if no_encoder {
            ui.label(RichText::new(NO_ENCODER).color(ORANGE));
        }
        let monitor = snap.monitors.iter().find(|m| m.index == s.monitor).cloned();
        // Pick a monitor that exists (e.g. after unplugging one).
        if monitor.is_none() {
            if let Some(m) = snap
                .monitors
                .iter()
                .find(|m| m.primary)
                .or(snap.monitors.first())
            {
                s.monitor = m.index;
            }
        }
        let mut refresh = false;
        Grid::new("my-screen").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
            let label = ui.label("Share").on_hover_text(
                "A whole monitor: your friend sees everything on it. Or only one program's \
                 window: your friend sees just that window, even when other windows cover it, \
                 and never your other windows, notifications or popups. If you minimize that \
                 window, your friend sees its last picture until you restore it; if you close \
                 it, sharing stops. Games in exclusive fullscreen can't be shared as one \
                 window: switch them to borderless windowed mode, or share the monitor.",
            );
            let selected = match &s.share_app {
                Some(exe) if snap.audio_apps.iter().any(|a| a.exe.eq_ignore_ascii_case(exe)) => {
                    format!("Only one program: {exe}")
                }
                Some(exe) => format!("Only one program: {exe} (not running)"),
                None => monitor
                    .as_ref()
                    .map(|m| m.name.clone())
                    .unwrap_or_else(|| format!("Monitor {}", s.monitor + 1)),
            };
            ui.horizontal(|ui| {
                ComboBox::from_id_salt("monitor")
                    .selected_text(selected)
                    .width(240.0)
                    .show_ui(ui, |ui| {
                        for m in &snap.monitors {
                            let picked = s.share_app.is_none() && s.monitor == m.index;
                            if ui.selectable_label(picked, &m.name).clicked() {
                                s.monitor = m.index;
                                s.share_app = None;
                            }
                        }
                        if snap.monitors.is_empty() {
                            ui.label("No monitors found");
                        }
                        ui.separator();
                        for app in programs(&snap.audio_apps) {
                            let picked = s
                                .share_app
                                .as_ref()
                                .is_some_and(|e| e.eq_ignore_ascii_case(&app.exe));
                            let label = format!("Only one program: {}", program_label(app));
                            if ui.selectable_label(picked, label).clicked() {
                                s.share_app = Some(app.exe.clone());
                            }
                        }
                    })
                    .response
                    .labelled_by(label.id);
                if ui
                    .button("Refresh")
                    .on_hover_text("Look for monitors and running programs again.")
                    .clicked()
                {
                    refresh = true;
                }
            });
            ui.end_row();

            let label = ui.label("Resolution").on_hover_text(
                "Your screen is scaled down to this height if it is bigger. \
                 Lower needs less upload speed. Scaling uses about 250 MB more \
                 graphics card memory and slows games a little more than sending \
                 your screen at its own size.",
            );
            // A window's size can change, so it isn't shown.
            let native = monitor
                .as_ref()
                .filter(|_| s.share_app.is_none())
                .map(|m| (m.width, m.height));
            let height_label = |h: Option<u32>| match (h, native) {
                (None, Some((w, hh))) => format!("Native ({w}×{hh})"),
                (None, None) => "Native".to_string(),
                (Some(h), Some((mw, mh))) => {
                    let (w, h2) = gf_video::output_size(mw, mh, Some(h));
                    if mh > h {
                        format!("{h}p ({w}×{h2})")
                    } else {
                        format!("{h}p (native {mw}×{mh})")
                    }
                }
                (Some(h), None) => format!("{h}p"),
            };
            ComboBox::from_id_salt("resolution")
                .selected_text(height_label(s.max_height))
                .width(240.0)
                .show_ui(ui, |ui| {
                    for h in HEIGHT_CHOICES {
                        ui.selectable_value(&mut s.max_height, h, height_label(h));
                    }
                })
                .response
                .labelled_by(label.id);
            ui.end_row();

            let label = ui.label("Frame rate").on_hover_text(
                "Pictures per second. 60 is smooth and enough for most games. 120 and 144 \
                 send that many pictures, but the same bitrate is shared by more pictures, \
                 so each one gets less detail: raise the bitrate too if your upload allows. \
                 Higher rates also use a little more of the computer's power.",
            );
            let fps_label = |f: u32| format!("{f} fps");
            ComboBox::from_id_salt("fps")
                .selected_text(fps_label(s.fps))
                .width(240.0)
                .show_ui(ui, |ui| {
                    for f in FPS_CHOICES {
                        ui.selectable_value(&mut s.fps, f, fps_label(f));
                    }
                })
                .response
                .labelled_by(label.id);
            ui.end_row();

            let caption = ui.label("Codec").on_hover_text(
                "The video format. Leave it on Auto: AV1 looks best for the bandwidth \
                 (RTX 40 series and newer), then HEVC, then H.264.",
            );
            let probed = !snap.probing_encoders;
            let auto_label = match (snap.probing_encoders, snap.my_encoders.first()) {
                (true, _) => "Auto (checking your graphics card…)".to_string(),
                (false, Some(c)) => format!("Auto (best available: {})", c.label()),
                (false, None) if no_encoder => "Auto (no working video encoder found)".to_string(),
                (false, None) => "Auto (best available)".to_string(),
            };
            let label = |c: CodecChoice| match c {
                CodecChoice::Auto => auto_label.clone(),
                c => c.label().to_string(),
            };
            ComboBox::from_id_salt("codec")
                .selected_text(label(s.codec))
                .width(240.0)
                .show_ui(ui, |ui| {
                    for c in CodecChoice::ALL {
                        let available = match c.codec() {
                            None => true,
                            Some(codec) => !probed || snap.my_encoders.contains(&codec),
                        };
                        let why = if c == CodecChoice::Av1 {
                            "Your graphics card can't send this format. AV1 needs an RTX 40 series or newer."
                        } else {
                            "Your graphics card can't send this format."
                        };
                        let r = ui
                            .add_enabled(available, egui::Button::selectable(s.codec == c, label(c)))
                            .on_disabled_hover_text(why);
                        if r.clicked() {
                            s.codec = c;
                        }
                    }
                })
                .response
                .labelled_by(caption.id);
            ui.end_row();

            let label = ui.label("Bitrate").on_hover_text(
                "How much data per second the video may use. More looks better but needs \
                 more upload speed: keep it well below your upload speed.",
            );
            ui.horizontal(|ui| {
                let codec = s
                    .codec
                    .codec()
                    .or(snap.my_encoders.first().copied())
                    .unwrap_or(VideoCodec::Hevc);
                let mut auto = s.bitrate_mbps.is_none();
                if ui
                    .checkbox(&mut auto, "Auto")
                    .on_hover_text("AV1 20 Mbps, HEVC 25 Mbps, H.264 35 Mbps.")
                    .changed()
                {
                    s.bitrate_mbps = if auto { None } else { Some(default_bitrate_mbps(codec)) };
                }
                match &mut s.bitrate_mbps {
                    Some(b) => {
                        labelled_slider(ui, b, MIN_BITRATE_MBPS..=MAX_BITRATE_MBPS, " Mbps", label.id);
                    }
                    None => {
                        ui.label(format!("{} Mbps for {}", default_bitrate_mbps(codec), codec.label()));
                    }
                }
            });
            ui.end_row();
        });

        let sharing = !matches!(snap.my_share, ShareState::Off);
        ui.horizontal(|ui| {
            if sharing {
                if ui
                    .button(RichText::new("Stop sharing").size(16.0))
                    .on_hover_text("Stop showing your screen to your friend.")
                    .clicked()
                {
                    self.commands.push(Command::StopSharing);
                }
            } else {
                let why_not = if snap.video_available {
                    "No working video encoder was found on your graphics card. Update your NVIDIA \
                     driver, restart the PC, then start GloorpFlorp again."
                } else {
                    "FFmpeg is missing, so screen sharing doesn't work. Extract the whole GloorpFlorp zip again."
                };
                let r = ui
                    .add_enabled(
                        snap.video_available && !no_encoder,
                        egui::Button::new(RichText::new("Start sharing").size(16.0)),
                    )
                    .on_hover_text(
                        "Show what you chose under Share to your friend. You can start before \
                         they join.",
                    )
                    .on_disabled_hover_text(why_not);
                if r.clicked() {
                    self.commands.push(Command::StartSharing);
                }
            }
        });
        let status = match snap.my_share {
            ShareState::Off => "Not sharing.".to_string(),
            ShareState::Waiting | ShareState::Starting => "Starting…".to_string(),
            ShareState::Live(i) => {
                let who = match snap.phase {
                    Phase::Connected { .. } if snap.peer_watching => "friend is watching",
                    Phase::Connected { .. } => "friend's viewer is closed",
                    Phase::Hosting { .. } => "waiting for your friend to join",
                    Phase::Reconnecting => "reconnecting",
                    Phase::Idle | Phase::Joining => "not connected to anyone yet",
                };
                format!(
                    "Sharing {} {}×{} @ {} fps, {who}",
                    i.codec.label(),
                    i.width,
                    i.height,
                    i.fps
                )
            }
        };
        ui.label(status);
        if sharing {
            ui.label(
                RichText::new("Changes to these settings restart sharing after a moment.").weak(),
            );
        }
        if refresh {
            self.commands.push(Command::RefreshLists);
        }
    }

    // ------------------------------------------------------------ game audio

    fn game_audio(&mut self, ui: &mut Ui) {
        ui.heading("Game audio");
        ui.label("Send your PC's sound to your friend, with or without sharing your screen.");
        let snap = &self.snap;
        let s = &mut self.settings;
        let mut refresh = false;
        ui.radio_value(
            &mut s.game_audio,
            GameAudioMode::AllExceptThisApp,
            "Everything except GloorpFlorp",
        )
        .on_hover_text(
            "All sound your PC plays (game, music, videos, other voice chats) except \
             GloorpFlorp's own, so your friend never hears themselves. If you're also in a \
             Discord call together, use \"Only one program\" instead.",
        );
        ui.horizontal_wrapped(|ui| {
            let only_one = matches!(s.game_audio, GameAudioMode::App { .. });
            if ui
                .radio(only_one, "Only one program")
                .on_hover_text("Only the sound of the program you pick, e.g. your game. Start it first so it shows up in the list.")
                .clicked()
                && !only_one
            {
                s.game_audio = GameAudioMode::App {
                    exe: self.last_app.clone(),
                };
            }
            let current = match &s.game_audio {
                GameAudioMode::App { exe } => exe.clone(),
                GameAudioMode::AllExceptThisApp => String::new(),
            };
            let running = snap.audio_apps.iter().any(|a| a.exe.eq_ignore_ascii_case(&current));
            let selected = match (current.is_empty(), running) {
                (true, _) => "Choose a program".to_string(),
                (false, true) => current.clone(),
                (false, false) => format!("{current} (not running)"),
            };
            let programs = ComboBox::from_id_salt("game-app")
                .selected_text(selected)
                .width(190.0)
                .show_ui(ui, |ui| {
                    for app in programs(&snap.audio_apps) {
                        let picked = current.eq_ignore_ascii_case(&app.exe);
                        if ui.selectable_label(picked, program_label(app)).clicked() {
                            s.game_audio = GameAudioMode::App {
                                exe: app.exe.clone(),
                            };
                            self.last_app = app.exe.clone();
                        }
                    }
                    if snap.audio_apps.is_empty() {
                        ui.label("No programs found. Start your game, then click Refresh.");
                    }
                })
                .response
                .on_hover_text("Programs with a window that are running right now.");
            set_accessible_name(&programs, "Program to send game audio from");
            if ui.button("Refresh").on_hover_text("Look for running programs again.").clicked() {
                refresh = true;
            }
        });
        ui.horizontal(|ui| {
            if snap.my_game_audio {
                if ui
                    .button("Stop sending game audio")
                    .on_hover_text("Your friend stops hearing your PC's sound.")
                    .clicked()
                {
                    self.commands.push(Command::StopGameAudio);
                }
                let level = ui
                    .add(
                        ProgressBar::new(snap.audio.game_level.clamp(0.0, 1.0))
                            .desired_width(120.0)
                            .desired_height(10.0),
                    )
                    .on_hover_text("How loud the sound you're sending is.");
                set_accessible_name(&level, "Game audio level");
            } else if ui
                .add_enabled(
                    snap.audio_running,
                    egui::Button::new("Start sending game audio"),
                )
                .on_hover_text("Your friend hears the sound chosen above. You keep hearing it too.")
                .on_disabled_hover_text(
                    "Sound is still starting (a few seconds after GloorpFlorp opens). \
                     If this stays greyed out, sound couldn't start: restart GloorpFlorp.",
                )
                .clicked()
            {
                self.commands.push(Command::StartGameAudio);
            }
        });
        ui.label(if snap.game_audio_starting {
            "Starting game audio…"
        } else if snap.my_game_audio {
            "Sending game audio."
        } else {
            "Not sending game audio."
        });
        if let Some(note) = &snap.game_audio_note {
            ui.label(RichText::new(note).color(ORANGE));
        }
        if refresh {
            self.commands.push(Command::RefreshLists);
        }
    }

    // ------------------------------------------------------------ friend's screen

    fn friend_screen(&mut self, ui: &mut Ui) {
        ui.heading("Friend's screen");
        let peer = self.peer();
        match self.snap.peer_share {
            Some(i) => {
                ui.label(format!(
                    "{peer} is sharing ({}, {}×{}, {} fps).",
                    i.codec.label(),
                    i.width,
                    i.height,
                    i.fps
                ));
                if self.snap.viewer_open {
                    ui.label(
                        RichText::new(
                            "The viewer window is open. Press f or double-click it for fullscreen.",
                        )
                        .weak(),
                    )
                    .on_hover_text(
                        "Close it while you play and aren't watching: that gives its graphics \
                         card memory and time back to your game.",
                    );
                } else if ui
                    .button("Open viewer")
                    .on_hover_text("Show your friend's screen in its own window.")
                    .clicked()
                {
                    self.cmd(Command::OpenViewer);
                }
            }
            None if self.connected() => {
                ui.label(format!("{peer} isn't sharing their screen."));
            }
            None => {
                ui.label("Not connected.");
            }
        }
        if self.snap.peer_game_audio {
            ui.horizontal(|ui| {
                dot(ui, GREEN, 12.0);
                ui.label(RichText::new(format!("{peer} is sending game audio.")).color(GREEN));
            });
        }
        ui.checkbox(
            &mut self.settings.auto_open_viewer,
            "Open the viewer by itself when my friend starts sharing",
        )
        .on_hover_text("When off, click Open viewer to watch.");
        ui.horizontal(|ui| {
            let label = ui.label("Friend's game audio volume");
            percent_slider(ui, &mut self.settings.audio.game_volume, label.id)
                .on_hover_text("How loud your friend's game sound is for you (100% = unchanged).");
        });
    }

    // ------------------------------------------------------------ voice

    fn voice(&mut self, ui: &mut Ui) {
        ui.heading("Voice");
        let snap = &self.snap;
        let peer = self
            .snap
            .peer_name
            .clone()
            .unwrap_or_else(|| "Your friend".into());
        let mut refresh = false;
        {
            let a = &mut self.settings.audio;
            Grid::new("devices").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                let label = ui.label("Microphone");
                device_combo(ui, "mic", &mut a.input_device, &snap.input_devices, label.id)
                    .on_hover_text("\"System default\" follows Windows' sound settings.");
                ui.end_row();
                let label = ui.label("Speakers");
                ui.horizontal(|ui| {
                    device_combo(ui, "speakers", &mut a.output_device, &snap.output_devices, label.id)
                        .on_hover_text("Where you hear your friend. \"System default\" follows Windows' sound settings.");
                    if ui.button("Refresh").on_hover_text("Look for microphones and speakers again.").clicked() {
                        refresh = true;
                    }
                });
                ui.end_row();
            });

            ui.label("How your microphone sends:");
            ui.horizontal(|ui| {
                ui.radio_value(&mut a.voice_mode, VoiceModeChoice::PushToTalk, "Push-to-talk")
                    .on_hover_text(
                        "Your voice is sent only while you hold a key. Works while the game has \
                         focus, unless the game runs as administrator; then run GloorpFlorp as \
                         administrator too, or use Voice activation.",
                    );
                ui.radio_value(&mut a.voice_mode, VoiceModeChoice::VoiceActivity, "Voice activation")
                    .on_hover_text("Sends when you speak.");
                ui.radio_value(&mut a.voice_mode, VoiceModeChoice::AlwaysOn, "Always on")
                    .on_hover_text("Sends all the time. Noise suppression still removes steady background noise.");
            });
            match a.voice_mode {
                VoiceModeChoice::PushToTalk => {
                    ui.horizontal_wrapped(|ui| {
                        ui.label("Push-to-talk key:");
                        key_button(
                            ui,
                            &mut self.binding,
                            BindTarget::PushToTalk,
                            Some(a.ptt_key),
                            "Click, then press the key or mouse button to hold while talking. \
                             The side mouse buttons (Mouse 4 / Mouse 5) work well. The game \
                             still gets the key too, so pick one you don't use in the game.",
                        );
                    });
                    if snap.ptt_blocked {
                        ui.label(RichText::new(PTT_BLOCKED).color(ORANGE));
                    }
                }
                VoiceModeChoice::VoiceActivity => {
                    ui.horizontal(|ui| {
                        let label = ui.label("Sensitivity");
                        let mut pct = (a.vad_sensitivity * 100.0).round();
                        if labelled_slider(ui, &mut pct, 0.0..=100.0, " %", label.id)
                            .on_hover_text(
                                "If the start of your words gets cut off, raise it. \
                                 If keyboard clicks get through, lower it.",
                            )
                            .changed()
                        {
                            a.vad_sensitivity = pct / 100.0;
                        }
                    });
                }
                VoiceModeChoice::AlwaysOn => {}
            }
            ui.horizontal(|ui| {
                if ui
                    .checkbox(&mut a.mic_muted, "Mute")
                    .on_hover_text("Your microphone sends nothing.")
                    .changed()
                {
                    self.commands.push(Command::SetMuted(a.mic_muted));
                }
                if ui
                    .checkbox(&mut a.deafened, "Deafen")
                    .on_hover_text("You hear nothing from your friend (no voice, no game audio).")
                    .changed()
                {
                    self.commands.push(Command::SetDeafened(a.deafened));
                }
            });
            ui.horizontal_wrapped(|ui| {
                ui.label("Shortcut keys: Mute");
                let (mute, deafen) = (self.settings.mute_key, self.settings.deafen_key);
                if key_button(ui, &mut self.binding, BindTarget::Mute, mute, SHORTCUT_TIP) {
                    self.settings.mute_key = None;
                }
                ui.label("Deafen");
                if key_button(
                    ui,
                    &mut self.binding,
                    BindTarget::Deafen,
                    deafen,
                    SHORTCUT_TIP,
                ) {
                    self.settings.deafen_key = None;
                }
            });
            ui.horizontal(|ui| {
                let label = ui.label("Friend's voice volume");
                percent_slider(ui, &mut a.voice_volume, label.id)
                    .on_hover_text("How loud your friend's voice is for you (100% = unchanged).");
            });
            ui.horizontal(|ui| {
                let label = ui.label("Your mic");
                ui.add(
                    ProgressBar::new(snap.audio.mic_level.clamp(0.0, 1.0))
                        .desired_width(150.0)
                        .desired_height(10.0),
                )
                .labelled_by(label.id)
                .on_hover_text("Moves when your microphone hears you.");
                if a.mic_muted {
                    ui.label(RichText::new("Muted").color(ORANGE));
                } else if snap.audio.transmitting {
                    dot(ui, GREEN, 12.0);
                    ui.label(RichText::new("Talking").color(GREEN))
                        .on_hover_text("Your voice is being sent right now.");
                } else {
                    ui.label(RichText::new("Not sending").color(GREY));
                }
            });
            if snap.audio.peer_talking {
                ui.horizontal(|ui| {
                    dot(ui, GREEN, 12.0);
                    ui.label(RichText::new(format!("{peer} is talking")).color(GREEN));
                });
            }
            ui.horizontal_wrapped(|ui| {
                ui.checkbox(&mut a.echo_cancel, "Echo cancellation")
                    .on_hover_text(
                        "Stops your friend hearing themselves through your speakers. Leave it on.",
                    );
                ui.checkbox(&mut a.noise_suppress, "Noise suppression")
                    .on_hover_text("Removes background noise like fans. Leave it on.");
                ui.checkbox(&mut a.auto_gain, "Automatic mic volume")
                    .on_hover_text("Keeps your voice at an even volume. Leave it on.");
            });
            if let Some(e) = &snap.audio.error {
                ui.label(RichText::new(e).color(ORANGE));
            }
        }
        if refresh {
            self.cmd(Command::RefreshLists);
        }
    }

    /// While binding a key: take the first key pressed, unless another
    /// binding has it.
    fn poll_key_binding(&mut self, ctx: &egui::Context) {
        let Some(b) = &mut self.binding else { return };
        const ESC: u32 = 0x1B;
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.binding = None;
            return;
        }
        match gf_sys::keys::first_pressed() {
            None => b.armed = true,
            Some(ESC) => self.binding = None,
            Some(vk) if b.armed => {
                let s = &mut self.settings;
                if let Some(why) = refusal(s, b.target, vk) {
                    b.refused = Some(why);
                    return;
                }
                log::info!("{} key set to {}", b.target.label(), gf_sys::keys::name(vk));
                match b.target {
                    BindTarget::PushToTalk => s.audio.ptt_key = vk,
                    BindTarget::Mute => s.mute_key = Some(vk),
                    BindTarget::Deafen => s.deafen_key = Some(vk),
                }
                self.binding = None;
            }
            Some(_) => {}
        }
    }

    // ------------------------------------------------------------ details

    fn details(&self, ui: &mut Ui) {
        egui::CollapsingHeader::new("Connection details")
            .default_open(false)
            .show(ui, |ui| {
                let n = &self.snap.net;
                let a = &self.snap.audio;
                let mbps = |kbps: f32| {
                    if kbps >= 1000.0 {
                        format!("{:.1} Mbps", kbps / 1000.0)
                    } else {
                        format!("{kbps:.0} kbps")
                    }
                };
                Grid::new("details").num_columns(2).striped(true).show(ui, |ui| {
                    ui.label("Ping").on_hover_text("Round trip time between the PCs. Under about 50 ms is great.");
                    ui.label(
                        n.rtt_ms
                            .filter(|_| n.connected)
                            .map_or("-".to_string(), |ms| format!("{ms:.0} ms")),
                    );
                    ui.end_row();
                    ui.label("Bitrates: video").on_hover_text("How much video is being sent and received.");
                    ui.label(format!(
                        "sent {}, received {}",
                        mbps(n.video_send_kbps),
                        mbps(n.video_recv_kbps)
                    ));
                    ui.end_row();
                    ui.label("Bitrates: audio").on_hover_text("Voice and game audio, both directions.");
                    ui.label(format!(
                        "sent {}, received {}",
                        mbps(n.audio_send_kbps),
                        mbps(n.audio_recv_kbps)
                    ));
                    ui.end_row();
                    ui.label("Video frames");
                    ui.label(format!(
                        "sent {:.0} fps, received {:.0} fps",
                        self.snap.send_fps, self.snap.recv_fps
                    ));
                    ui.end_row();
                    ui.label("Send queue").on_hover_text("Video frames waiting to be sent. Should stay near 0.");
                    ui.label(format!("{} frames", n.video_queue_frames));
                    ui.end_row();
                    ui.label("Dropped frames").on_hover_text(
                        "Video frames skipped because the network couldn't keep up. \
                         If this keeps climbing, lower the bitrate.",
                    );
                    ui.label(n.video_frames_dropped.to_string());
                    ui.end_row();
                    ui.label("Audio loss").on_hover_text("Missing audio packets. A few percent is fine; small gaps are filled in.");
                    ui.label(format!("{:.1} %", n.audio_loss_pct));
                    ui.end_row();
                    ui.label("Audio delay buffer").on_hover_text(
                        "How much incoming audio is held back to smooth out network hiccups.",
                    );
                    ui.label(format!(
                        "voice {} ms, game {} ms",
                        a.voice_jitter.delay_ms, a.game_jitter.delay_ms
                    ));
                    ui.end_row();
                    ui.label("Your Tailscale address");
                    ui.label(self.snap.tailscale_ip.map_or("not found".to_string(), |ip| ip.to_string()));
                    ui.end_row();
                    if let Some(addr) = self.snap.listen_addr {
                        ui.label("Room open on")
                            .on_hover_text("Your friend connects to this address and port (TCP and UDP).");
                        ui.label(addr.to_string());
                        ui.end_row();
                    }
                    let codecs = |list: &[VideoCodec]| {
                        if list.is_empty() {
                            "-".to_string()
                        } else {
                            list.iter().map(|c| c.label()).collect::<Vec<_>>().join(", ")
                        }
                    };
                    ui.label("Video formats you can send");
                    ui.label(if self.snap.probing_encoders {
                        "checking…".to_string()
                    } else {
                        codecs(&self.snap.my_encoders)
                    });
                    ui.end_row();
                    if self.connected() && !self.snap.peer_encoders.is_empty() {
                        ui.label("Video formats your friend can send");
                        ui.label(codecs(&self.snap.peer_encoders));
                        ui.end_row();
                    }
                });
            });
    }
}

impl eframe::App for GloorpApp {
    // Runs before every `ui`, and also while the window is minimized (when
    // `ui` doesn't), so invites are shown (taskbar flash), answered and timed
    // out there too.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.snap = self.engine.snapshot();
        // Mute and deafen belong to the engine (the shortcut keys change
        // them there); the checkboxes send SetMuted / SetDeafened.
        self.settings.audio.mic_muted = self.snap.mic_muted;
        self.settings.audio.deafened = self.snap.deafened;
        self.poll_lobby(ctx);
        for c in self.commands.drain(..) {
            self.engine.send(c);
        }
        let busy = self.binding.is_some();
        let live = !matches!(self.snap.phase, Phase::Idle)
            || !matches!(self.snap.my_share, ShareState::Off);
        let every = if busy {
            Duration::from_millis(50)
        } else if live {
            Duration::from_millis(100)
        } else {
            Duration::from_millis(250)
        };
        ctx.request_repaint_after(every);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll_key_binding(ui.ctx());
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink(false)
                .show(ui, |ui| {
                    self.status_line(ui);
                    self.banners(ui);
                    self.invite_prompt(ui);
                    ui.separator();
                    self.room(ui);
                    ui.separator();
                    self.lobby(ui);
                    ui.separator();
                    self.my_screen(ui);
                    ui.separator();
                    self.game_audio(ui);
                    ui.separator();
                    self.friend_screen(ui);
                    ui.separator();
                    self.voice(ui);
                    ui.separator();
                    self.details(ui);
                });
        });
        self.sent.audio.mic_muted = self.settings.audio.mic_muted;
        self.sent.audio.deafened = self.settings.audio.deafened;
        if self.settings != self.sent {
            self.sent = self.settings.clone();
            self.commands
                .push(Command::ApplySettings(Box::new(self.settings.clone())));
        }
        for c in self.commands.drain(..) {
            self.engine.send(c);
        }
    }

    fn on_exit(&mut self) {
        if self.settings != self.sent {
            self.engine
                .send(Command::ApplySettings(Box::new(self.settings.clone())));
        }
        log::info!("window closed; shutting down");
        self.engine.shutdown(Duration::from_secs(3));
        // Even if the engine got stuck, no mpv or FFmpeg outlives the window.
        gf_sys::close_children();
        // main ends the process right after the window is gone; this covers
        // the window's own teardown hanging.
        let _ = std::thread::Builder::new()
            .name("exit-watchdog".into())
            .spawn(|| {
                std::thread::sleep(Duration::from_secs(5));
                log::warn!("closing took over 5 s; ending GloorpFlorp now");
                gf_sys::exit_now(1);
            });
    }
}

/// The button for `target`'s key: click it, then press the key (see
/// `poll_key_binding`); while binding, a prompt and Cancel instead.
/// Mute and Deafen also get Clear: returns true when it was clicked.
fn key_button(
    ui: &mut Ui,
    binding: &mut Option<KeyBinding>,
    target: BindTarget,
    key: Option<u32>,
    tip: &str,
) -> bool {
    if let Some(b) = binding.as_ref().filter(|b| b.target == target) {
        let prompt = match &b.refused {
            Some(why) => format!("{why} Press another key… (Esc cancels)"),
            None => "Press a key or mouse button… (Esc cancels)".to_string(),
        };
        ui.label(RichText::new(prompt).color(YELLOW));
        if ui.button("Cancel").clicked() {
            *binding = None;
        }
        return false;
    }
    let name = key.map_or_else(|| "Not set".to_string(), gf_sys::keys::name);
    let r = ui.button(&name).on_hover_text(tip);
    set_accessible_name(&r, &format!("{} key: {name}", target.label()));
    if r.clicked() {
        *binding = Some(KeyBinding {
            target,
            armed: false,
            refused: None,
        });
    }
    if target == BindTarget::PushToTalk || key.is_none() {
        return false;
    }
    let r = ui.button("Clear").on_hover_text("Remove this shortcut.");
    set_accessible_name(&r, &format!("Clear the {} key", target.label()));
    r.clicked()
}

/// Why `vk` can't be `target`'s key (another binding has it), or `None`.
fn refusal(s: &Settings, target: BindTarget, vk: u32) -> Option<String> {
    let other = BindTarget::ALL
        .into_iter()
        .find(|&t| t != target && t.key(s) == Some(vk))?;
    let (name, label) = (gf_sys::keys::name(vk), other.label());
    Some(match other {
        // shortcut: the push-to-talk key counts as taken even while
        // push-to-talk is off (it's kept for when it's back on); free it
        // then if people want that key for Mute or Deafen.
        BindTarget::PushToTalk if s.audio.voice_mode != VoiceModeChoice::PushToTalk => format!(
            "{name} is your push-to-talk key. To use it here, choose Push-to-talk and change \
             that key first."
        ),
        BindTarget::PushToTalk => format!("{name} is already the {label} key."),
        // shortcut: the shortcut keys still work while a key is being bound
        // (the engine's key thread doesn't know), so that press switched it
        // (also when re-binding the same key); tell the engine to pause them
        // while binding if people mind.
        _ => format!(
            "{name} is already the {label} key, so that press also switched {label} on or off."
        ),
    })
}

/// A small filled circle (the default fonts have no "●").
fn dot(ui: &mut Ui, color: Color32, size: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    ui.painter()
        .circle_filled(rect.center(), size * 0.35, color);
}

/// A red message with an optional × button; true when it was clicked.
fn banner(ui: &mut Ui, text: &str, close_tip: Option<&str>) -> bool {
    let mut closed = false;
    egui::Frame::new()
        .fill(RED_BG)
        .inner_margin(8)
        .corner_radius(4)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_top(|ui| {
                let w = ui.available_width() - if close_tip.is_some() { 32.0 } else { 0.0 };
                ui.allocate_ui(egui::vec2(w.max(50.0), 0.0), |ui| {
                    ui.add(egui::Label::new(RichText::new(text).color(Color32::WHITE)).wrap());
                });
                if let Some(tip) = close_tip {
                    ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                        let close =
                            egui::Button::new(RichText::new("×").size(16.0).color(Color32::WHITE))
                                .frame(false);
                        let r = ui.add(close);
                        // Screen readers say the tip, not "×".
                        set_accessible_name(&r, tip);
                        if r.on_hover_text(tip).clicked() {
                            closed = true;
                        }
                    });
                }
            });
        });
    closed
}

/// "System default" plus the devices; a saved device that is gone stays
/// selectable so the choice isn't lost while it's unplugged.
fn device_combo(
    ui: &mut Ui,
    id: &str,
    value: &mut Option<String>,
    devices: &[String],
    label: egui::Id,
) -> egui::Response {
    let text = match value {
        None => "System default".to_string(),
        Some(d) if devices.contains(d) => shorten(d, 34),
        Some(d) => format!("{} (not found)", shorten(d, 24)),
    };
    ComboBox::from_id_salt(id)
        .selected_text(text)
        .width(230.0)
        .show_ui(ui, |ui| {
            ui.selectable_value(value, None, "System default");
            for d in devices {
                ui.selectable_value(value, Some(d.clone()), d);
            }
        })
        .response
        .labelled_by(label)
}

/// 0.0..=2.0 shown as 0–200 %.
fn percent_slider(ui: &mut Ui, value: &mut f32, label: egui::Id) -> egui::Response {
    let mut pct = (*value * 100.0).round();
    let r = labelled_slider(ui, &mut pct, 0.0..=200.0, " %", label);
    if r.changed() {
        *value = pct / 100.0;
    }
    r
}

/// A slider with its number box, both named after the `label` next to them
/// for screen readers (egui's own number box would have no name).
fn labelled_slider<N: egui::emath::Numeric>(
    ui: &mut Ui,
    value: &mut N,
    range: std::ops::RangeInclusive<N>,
    suffix: &str,
    label: egui::Id,
) -> egui::Response {
    let slider = ui
        .add(Slider::new(value, range.clone()).show_value(false))
        .labelled_by(label);
    let number = ui
        .add(egui::DragValue::new(value).range(range).suffix(suffix))
        .labelled_by(label);
    slider | number
}

/// Names a control for screen readers when no visible text says what it is.
fn set_accessible_name(r: &egui::Response, name: &str) {
    r.ctx
        .accesskit_node_builder(r.id, |node| node.set_label(name));
}

/// egui's text boxes ignore "set value" requests from accessibility tools
/// (Windows UI Automation); this applies them.
fn accept_set_value(r: &egui::Response, text: &mut String, max_chars: usize) {
    use egui::accesskit::{Action, ActionData};
    r.ctx
        .accesskit_node_builder(r.id, |node| node.add_action(Action::SetValue));
    let new = r.ctx.input(|i| {
        i.accesskit_action_requests(r.id, Action::SetValue)
            .filter_map(|req| match &req.data {
                Some(ActionData::Value(v)) => Some(v.to_string()),
                _ => None,
            })
            .last()
    });
    if let Some(v) = new {
        *text = v.chars().take(max_chars).collect();
        r.ctx.request_repaint();
    }
}

/// One entry per program (the first of several processes with one exe,
/// which is also the one game audio and sharing look up).
fn programs(apps: &[AudioApp]) -> Vec<&AudioApp> {
    let mut seen = Vec::new();
    apps.iter()
        .filter(|a| {
            let exe = a.exe.to_lowercase();
            let new = !seen.contains(&exe);
            seen.push(exe);
            new
        })
        .collect()
}

fn program_label(app: &AudioApp) -> String {
    if app.title.is_empty() {
        app.exe.clone()
    } else {
        format!("{} - {}", app.exe, shorten(&app.title, 40))
    }
}

fn shorten(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

/// How long before `now` the Unix time `at` was, in short.
fn ago(at: u64, now: u64) -> String {
    match now.saturating_sub(at) {
        s if s < 60 => "just now".into(),
        s if s < 3600 => format!("{} min ago", s / 60),
        s if s < 86400 => format!("{} h ago", s / 3600),
        s => format!("{} d ago", s / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::accesskit::{Action, ActionData, ActionRequest, Node, NodeId, Role, TreeId};

    /// One frame; returns the accessibility tree (what screen readers and
    /// Windows UI Automation see).
    fn frame(
        ctx: &egui::Context,
        events: Vec<egui::Event>,
        mut f: impl FnMut(&mut Ui),
    ) -> Vec<(NodeId, Node)> {
        let input = egui::RawInput {
            events,
            ..Default::default()
        };
        let mut out = ctx.run_ui(input, |ui| f(ui));
        let tree = out.platform_output.accesskit_update.take().unwrap();
        // No renderer here.
        out.drop_without_applying_deltas();
        tree.nodes
    }

    /// The text of the labels that name `node` (as UI Automation reads them).
    fn name(nodes: &[(NodeId, Node)], node: &Node) -> String {
        if let Some(l) = node.label() {
            return l.to_string();
        }
        let label_of = |id: &NodeId| {
            let (_, n) = nodes.iter().find(|(i, _)| i == id).unwrap();
            n.value().or(n.label()).unwrap_or_default().to_string()
        };
        node.labelled_by()
            .iter()
            .map(label_of)
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn names_of(nodes: &[(NodeId, Node)], role: Role) -> Vec<String> {
        nodes
            .iter()
            .filter(|(_, n)| n.role() == role)
            .map(|(_, n)| name(nodes, n))
            .collect()
    }

    #[test]
    fn controls_have_accessible_names() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut volume = 1.0;
        let mut device = None;
        let nodes = frame(&ctx, vec![], |ui| {
            let label = ui.label("Friend's voice volume");
            percent_slider(ui, &mut volume, label.id);
            let label = ui.label("Microphone");
            device_combo(ui, "mic", &mut device, &["Yeti".into()], label.id);
            banner(ui, "Something failed.", Some("Hide this message"));
        });
        assert_eq!(names_of(&nodes, Role::Slider), ["Friend's voice volume"]);
        assert_eq!(
            names_of(&nodes, Role::SpinButton),
            ["Friend's voice volume"]
        );
        assert_eq!(names_of(&nodes, Role::ComboBox), ["Microphone"]);
        assert_eq!(names_of(&nodes, Role::Button), ["Hide this message"]);
    }

    #[test]
    fn invite_follows_the_room() {
        let hosting = |code: &str, waiting| Phase::Hosting {
            code: code.into(),
            waiting,
        };
        let s = Duration::from_secs;
        // Clicked: the engine hasn't started hosting yet, then the room opens.
        assert_eq!(invite_step(&Phase::Idle, None, s(0)), InviteStep::Wait);
        assert_eq!(
            invite_step(&hosting("", false), None, s(3)),
            InviteStep::Wait
        );
        assert_eq!(
            invite_step(&hosting("K7QM", true), None, s(3)),
            InviteStep::Send("K7QM".into())
        );
        // Sent: waits for the answer, up to the timeout.
        let sent = Some("K7QM");
        assert_eq!(
            invite_step(&hosting("K7QM", true), sent, s(59)),
            InviteStep::Wait
        );
        assert_eq!(
            invite_step(&hosting("K7QM", true), sent, s(61)),
            InviteStep::TimedOut
        );
        // They joined; the room closed, didn't open, or got a new code.
        let joined = Phase::Connected { peer: "B".into() };
        assert_eq!(invite_step(&joined, sent, s(5)), InviteStep::Done);
        assert_eq!(invite_step(&Phase::Idle, sent, s(5)), InviteStep::Done);
        assert_eq!(invite_step(&Phase::Idle, None, s(2)), InviteStep::Done);
        assert_eq!(
            invite_step(&hosting("NEW", true), sent, s(5)),
            InviteStep::Done
        );
    }

    #[test]
    fn chat_ages() {
        let t = 1_790_000_000;
        assert_eq!(ago(t, t + 59), "just now");
        assert_eq!(ago(t, t + 5 * 60 + 1), "5 min ago");
        assert_eq!(ago(t, t + 3 * 3600), "3 h ago");
        assert_eq!(ago(t, t + 2 * 86400 + 5), "2 d ago");
        // The server's clock may be a little ahead.
        assert_eq!(ago(t + 5, t), "just now");
    }

    #[test]
    fn text_boxes_take_values_from_ui_automation() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut text = "HYEON-SU".to_string();
        let edit_id = std::cell::Cell::new(None);
        let show = |ui: &mut Ui, text: &mut String| {
            let label = ui.label("Your name:");
            let r = ui
                .add(TextEdit::singleline(text).char_limit(10))
                .labelled_by(label.id);
            accept_set_value(&r, text, 10);
            edit_id.set(Some(r.id));
        };
        let nodes = frame(&ctx, vec![], |ui| show(ui, &mut text));
        let (_, edit) = nodes
            .iter()
            .find(|(_, n)| n.role() == Role::TextInput)
            .unwrap();
        assert_eq!(name(&nodes, edit), "Your name:");
        assert!(edit.supports_action(Action::SetValue));

        let set = egui::Event::AccessKitActionRequest(ActionRequest {
            action: Action::SetValue,
            target_tree: TreeId::ROOT,
            target_node: edit_id.get().unwrap().accesskit_id(),
            data: Some(ActionData::Value("Friend B, a long name".into())),
        });
        frame(&ctx, vec![set], |ui| show(ui, &mut text));
        assert_eq!(text, "Friend B, ", "set, and cut to the box's limit");
    }

    #[test]
    fn a_key_does_one_thing() {
        // Defaults: Voice activation, push-to-talk key Mouse 5.
        let mut s = Settings {
            mute_key: Some(0x77),
            ..Settings::default()
        };
        assert_eq!(
            refusal(&s, BindTarget::Mute, 0x77),
            None,
            "its own key again"
        );
        assert_eq!(refusal(&s, BindTarget::Deafen, 0x78), None);
        assert!(refusal(&s, BindTarget::Deafen, 0x77)
            .unwrap()
            .ends_with("is already the Mute key, so that press also switched Mute on or off."));
        assert!(refusal(&s, BindTarget::PushToTalk, 0x77).is_some());
        // The push-to-talk key, with its button hidden: say how to free it.
        assert!(refusal(&s, BindTarget::Mute, 0x06)
            .unwrap()
            .contains("choose Push-to-talk and change that key"));
        s.audio.voice_mode = VoiceModeChoice::PushToTalk;
        assert_eq!(
            refusal(&s, BindTarget::Deafen, 0x06).unwrap(),
            "Mouse 5 is already the Push-to-talk key."
        );
    }
}
