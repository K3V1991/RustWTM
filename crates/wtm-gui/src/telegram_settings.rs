//! Settings → Telegram bot: token, allowed chats, alerts and control.

use eframe::egui::{self, Ui};
use wtm_policy::{TelegramConfig, parse_chats, token_looks_valid};

use crate::backend::{Data, Mode, NO_TELEGRAM, Panel, ServiceProblem, UiCommand, service_problem};
use crate::theme::{BAD, GOOD, WARN, icon};

/// The width of the token and chat ID fields.
const FIELD_WIDTH: f32 = 300.0;

#[derive(Debug, Default)]
pub struct TelegramView {
    /// Being edited; `None` until the service's settings arrive.
    form: Option<TelegramConfig>,
    chats: String,
    /// The settings the form started from.
    original: Option<TelegramConfig>,
    show_token: bool,
    requested: bool,
    session: u64,
}

impl TelegramView {
    /// The form as settings, or why it cannot be saved.
    fn edited(&self) -> Option<Result<TelegramConfig, String>> {
        let form = self.form.as_ref()?;
        Some(parse_chats(&self.chats).and_then(|chats| {
            let cfg = TelegramConfig {
                chats,
                token: form.token.trim().to_owned(),
                ..form.clone()
            };
            cfg.validate().map(|()| cfg)
        }))
    }
}

/// Returns the commands to send (load, save, test).
pub fn telegram(ui: &mut Ui, data: &Data, view: &mut TelegramView) -> Vec<UiCommand> {
    if view.session != data.service_session
        || !matches!(data.mode, Mode::Service { admin: true, .. })
    {
        *view = TelegramView {
            session: data.service_session,
            ..TelegramView::default()
        };
    }
    let mut commands = Vec::new();
    crate::theme::option_group(ui, crate::i18n::tr("🔔 Telegram bot"));
    ui.scope(|ui| {
        ui.add_space(0.0);
        match data.mode {
            Mode::Service { admin: true, .. } => {}
            Mode::Service { admin: false, .. } => {
                ui.horizontal(|ui| {
                    ui.colored_label(
                        crate::theme::text_color(ui, WARN),
                        crate::i18n::tr("Setting up the bot needs administrator rights."),
                    );
                    if ui
                        .small_button(crate::i18n::tr("Restart as administrator"))
                        .clicked()
                    {
                        commands.push(UiCommand::RestartElevated);
                    }
                });
                return;
            }
            Mode::Starting | Mode::Local => {
                let error = data.service_error.as_deref();
                let warning = match service_problem(error) {
                    ServiceProblem::NotRunning => crate::i18n::tr(
                        "The bot runs in the RustWTM service, which is not running. Install \
                         RustWTM with the MSI to use it.",
                    )
                    .to_owned(),
                    problem => problem.describe(NO_TELEGRAM, error),
                };
                ui.colored_label(crate::theme::text_color(ui, WARN), warning);
                return;
            }
        }
        match &data.telegram {
            None => {
                if !view.requested {
                    view.requested = true;
                    commands.push(UiCommand::Load(Panel::Telegram));
                }
                crate::theme::loading(ui, crate::i18n::tr("Loading the bot settings…"));
                return;
            }
            Some(Err(e)) => {
                ui.colored_label(crate::theme::text_color(ui, BAD), e);
                if ui.button(crate::i18n::tr("Retry")).clicked() {
                    commands.push(UiCommand::Load(Panel::Telegram));
                }
                return;
            }
            Some(Ok(saved)) => {
                if view.original.as_ref() != Some(saved) {
                    view.chats = saved
                        .chats
                        .iter()
                        .map(i64::to_string)
                        .collect::<Vec<_>>()
                        .join(", ");
                    view.form = Some(saved.clone());
                    view.original = Some(saved.clone());
                }
            }
        }
        steps(ui);
        let Some(form) = view.form.as_mut() else {
            return;
        };
        crate::theme::option_switch(
            ui,
            icon::POWER,
            crate::i18n::tr("Enabled"),
            "",
            &mut form.enabled,
        );
        let show_token = &mut view.show_token;
        crate::theme::option_labelled(ui, icon::KEY, crate::i18n::tr("Bot token"), "", |ui, id| {
            // The field and its Show button together are as wide as the chat
            // IDs field below, so both rows line up at both ends.
            let eye_width = [crate::i18n::tr("Show"), crate::i18n::tr("Hide")]
                .into_iter()
                .map(|text| {
                    egui::WidgetText::from(text)
                        .into_galley(
                            ui,
                            Some(egui::TextWrapMode::Extend),
                            f32::INFINITY,
                            egui::TextStyle::Button,
                        )
                        .size()
                        .x
                })
                .fold(0.0, f32::max)
                + 2.0 * ui.spacing().button_padding.x;
            let height = ui.spacing().interact_size.y;
            ui.add_sized(
                [
                    FIELD_WIDTH - eye_width - ui.spacing().item_spacing.x,
                    height,
                ],
                egui::TextEdit::singleline(&mut form.token)
                    .password(!*show_token)
                    .hint_text(crate::i18n::tr("123456789:ABC… from @BotFather")),
            )
            .labelled_by(id);
            let eye = if *show_token {
                crate::i18n::tr("Hide")
            } else {
                crate::i18n::tr("Show")
            };
            if ui
                .add_sized([eye_width, height], egui::Button::new(eye))
                .on_hover_text(crate::i18n::tr("Show or hide the token"))
                .clicked()
            {
                *show_token = !*show_token;
            }
        });
        crate::theme::option_labelled(
            ui,
            icon::CHAT,
            crate::i18n::tr("Allowed chat IDs"),
            crate::i18n::tr("Only these chats may use the bot; separate several with commas"),
            |ui, id| {
                ui.add_sized(
                    [FIELD_WIDTH, ui.spacing().interact_size.y],
                    egui::TextEdit::singleline(&mut view.chats)
                        .hint_text(crate::i18n::tr("e.g. 123456789, 987654321")),
                )
                .labelled_by(id);
            },
        );
        crate::theme::option_switch(
            ui,
            icon::BELL,
            crate::i18n::tr("Send alerts (CPU overload, swarms, programs ended by the kill list)"),
            "",
            &mut form.notify,
        );
        if cfg!(feature = "telegram-control")
            || view.original.as_ref().is_some_and(|cfg| cfg.control)
        {
            crate::theme::option_switch(
                ui,
                icon::ADMIN,
                crate::i18n::tr("Allow ending, suspending and resuming processes and services"),
                crate::i18n::tr(
                    "Without it the bot only reports; protected Windows processes are never ended",
                ),
                &mut form.control,
            );
        } else {
            ui.weak(crate::i18n::tr(
                "This build only reports: the bot cannot end, suspend or resume processes or \
                 services.",
            ));
        }
        if !form.token.trim().is_empty() && !token_looks_valid(form.token.trim()) {
            crate::theme::colored(
                ui,
                WARN,
                crate::i18n::tr("This does not look like a bot token (123456789:ABC…)."),
            );
        }
        let edited = view.edited();
        let dirty = match &edited {
            Some(Ok(cfg)) => view.original.as_ref() != Some(cfg),
            _ => true,
        };
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            let valid = matches!(edited, Some(Ok(_)));
            if ui
                .add_enabled(valid && dirty, egui::Button::new(crate::i18n::tr("Save")))
                .clicked()
                && let Some(Ok(cfg)) = edited.clone()
            {
                commands.push(UiCommand::SaveTelegram {
                    config: cfg,
                    session: view.session,
                });
            }
            let saved_token = view
                .original
                .as_ref()
                .is_some_and(|c| token_looks_valid(&c.token));
            if ui
                .add_enabled(
                    saved_token && !dirty,
                    egui::Button::new(crate::i18n::tr("Send test message")),
                )
                .on_hover_text(crate::i18n::tr(
                    "Checks the saved token and messages every allowed chat",
                ))
                .on_disabled_hover_text(crate::i18n::tr("Save a bot token first"))
                .clicked()
            {
                commands.push(UiCommand::TestTelegram {
                    session: view.session,
                });
            }
            match (&edited, &view.original) {
                (Some(Err(e)), _) => {
                    ui.colored_label(crate::theme::text_color(ui, BAD), format!("✖ {e}"));
                }
                (_, _) if dirty => {
                    ui.colored_label(
                        crate::theme::text_color(ui, WARN),
                        crate::i18n::tr("● unsaved changes"),
                    );
                }
                (_, Some(c)) if c.enabled && c.chats.is_empty() => {
                    crate::theme::colored(
                        ui,
                        WARN,
                        crate::i18n::tr(
                            "On, but no chat is allowed yet: message the bot to get your ID.",
                        ),
                    );
                }
                (_, Some(c)) if c.enabled => {
                    ui.colored_label(crate::theme::text_color(ui, GOOD), {
                        let __wtm_i18n_0 = &(c.chats.len());
                        crate::i18n::format("✔ On for {0} chat(s)", &[format!("{}", __wtm_i18n_0)])
                    });
                }
                _ => {
                    ui.weak(crate::i18n::tr("Off"));
                }
            }
        });
    });
    commands
}

fn steps(ui: &mut Ui) {
    egui::CollapsingHeader::new(crate::i18n::tr("How to set it up"))
        .id_salt("telegram_steps")
        .default_open(false)
        .show(ui, |ui| {
            ui.label(crate::i18n::tr(
                "1. In Telegram, open @BotFather, send /newbot and follow the questions.",
            ));
            ui.label(crate::i18n::tr(
                "2. Paste the token it gives you below, tick Enabled and Save.",
            ));
            ui.label(crate::i18n::tr(
                "3. Send your new bot any message: it answers with your chat ID.",
            ));
            ui.label(crate::i18n::tr(
                "4. Add that ID to Allowed chat IDs and Save. Send /help to the bot.",
            ));
            ui.weak(crate::i18n::tr(
                "The token is stored in %ProgramData%\\RustWTM\\telegram.toml, readable only by \
                 administrators. Anyone with the token can read what the bot sends, so keep it \
                 private; chats not on the list cannot give commands.",
            ));
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable;

    const TOKEN: &str = "123456789:AAFh3kq_Zx-9yLmnopQRstuVWxyz012345";

    fn run(data: Data, view: &mut TelegramView) -> Vec<UiCommand> {
        let mut out = Vec::new();
        {
            let mut harness = Harness::builder()
                .allow_missing_glyphs()
                .build_ui(|ui| out.extend(telegram(ui, &data, view)));
            harness.step();
        }
        out
    }

    fn admin() -> Data {
        Data {
            mode: Mode::Service {
                admin: true,
                verified: true,
            },
            ..Data::default()
        }
    }

    #[test]
    fn loads_once_and_asks_non_admins_to_elevate() {
        let mut view = TelegramView::default();
        let cmds = run(admin(), &mut view);
        assert!(matches!(
            cmds.as_slice(),
            [UiCommand::Load(Panel::Telegram)]
        ));
        assert!(run(admin(), &mut view).is_empty(), "requested only once");

        let data = Data {
            mode: Mode::Service {
                admin: false,
                verified: true,
            },
            ..Data::default()
        };
        let mut view = TelegramView::default();
        let mut harness = Harness::builder().allow_missing_glyphs().build_ui(|ui| {
            telegram(ui, &data, &mut view);
        });
        harness.run();
        harness.get_by_label("Restart as administrator");
    }

    #[test]
    fn connection_warnings_send_users_to_the_right_recovery() {
        let mut failures = Vec::new();
        for (error, expected, misleading) in [
            (None, "Install RustWTM with the MSI", "Restart RustWTM"),
            (
                Some("service speaks protocol 19, client 20"),
                "Update it with the installer of version",
                "Restart RustWTM",
            ),
            (
                Some("service speaks protocol 21, client 20"),
                "Restart RustWTM",
                "Update it with the installer of version",
            ),
            (
                Some("the pipe is not served by the RustWTM service"),
                "another program answers",
                "Install RustWTM with the MSI",
            ),
        ] {
            let data = Data {
                mode: Mode::Local,
                service_error: error.map(str::to_owned),
                ..Data::default()
            };
            let mut view = TelegramView::default();
            let mut harness = Harness::builder().allow_missing_glyphs().build_ui(|ui| {
                assert!(telegram(ui, &data, &mut view).is_empty());
            });
            harness.run();
            if harness.query_by_label_contains(expected).is_none() {
                failures.push(format!("expected {expected:?} for {error:?}"));
            }
            if harness.query_by_label_contains(misleading).is_some() {
                failures.push(format!("misleading recovery {misleading:?} for {error:?}"));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn reconnect_reloads_the_form_and_drops_the_previous_draft() {
        let mut view = TelegramView::default();
        assert!(matches!(
            run(admin(), &mut view).as_slice(),
            [UiCommand::Load(Panel::Telegram)]
        ));
        let loaded = Data {
            telegram: Some(Ok(TelegramConfig::default())),
            ..admin()
        };
        run(loaded, &mut view);
        view.chats = "123".into();
        view.show_token = true;

        run(Data::default(), &mut view);
        let commands = run(admin(), &mut view);
        assert!(
            view.form.is_none()
                && view.chats.is_empty()
                && !view.show_token
                && matches!(commands.as_slice(), [UiCommand::Load(Panel::Telegram)]),
            "old draft retained={}, reload requested={}",
            view.form.is_some(),
            !commands.is_empty(),
        );
        assert!(run(admin(), &mut view).is_empty(), "only one request");

        // The page may not have been visible when the connection went away.
        run(
            Data {
                service_session: 1,
                telegram: Some(Ok(TelegramConfig::default())),
                ..admin()
            },
            &mut view,
        );
        view.chats = "456".into();
        let commands = run(
            Data {
                service_session: 2,
                ..admin()
            },
            &mut view,
        );
        assert!(matches!(
            commands.as_slice(),
            [UiCommand::Load(Panel::Telegram)]
        ));
        assert!(view.form.is_none() && view.chats.is_empty());
    }

    #[test]
    fn the_test_button_sends_the_forms_service_session() {
        let data = Data {
            telegram: Some(Ok(TelegramConfig {
                token: TOKEN.into(),
                ..TelegramConfig::default()
            })),
            service_session: 17,
            ..admin()
        };
        let mut view = TelegramView::default();
        let mut commands = Vec::new();
        {
            let mut harness = Harness::builder().allow_missing_glyphs().build_ui(|ui| {
                commands.extend(telegram(ui, &data, &mut view));
            });
            harness.run();
            harness.get_by_label("Send test message").click();
            harness.run();
        }
        assert!(matches!(
            commands.as_slice(),
            [UiCommand::TestTelegram { session: 17 }]
        ));
    }

    #[test]
    fn edits_are_checked_and_saved() {
        let mut data = admin();
        data.telegram = Some(Ok(TelegramConfig::default()));
        let mut view = TelegramView::default();
        run(
            Data {
                telegram: data.telegram.clone(),
                ..admin()
            },
            &mut view,
        );
        let form = view.form.as_mut().unwrap();
        form.enabled = true;
        form.token = "nonsense".into();
        assert!(matches!(view.edited(), Some(Err(_))));
        view.form.as_mut().unwrap().token = format!(" {TOKEN} ");
        view.chats = "42, x".into();
        assert!(view.edited().unwrap().unwrap_err().contains('x'));
        view.chats = "42, 43".into();
        let cfg = view.edited().unwrap().unwrap();
        assert_eq!(
            (cfg.token.as_str(), cfg.chats.as_slice()),
            (TOKEN, &[42, 43][..])
        );

        let mut saved = Vec::new();
        {
            let mut harness = Harness::builder().allow_missing_glyphs().build_ui(|ui| {
                for c in telegram(ui, &data, &mut view) {
                    if let UiCommand::SaveTelegram { config, .. } = c {
                        saved.push(config);
                    }
                }
            });
            harness.run();
            harness.get_by_label("● unsaved changes");
            harness.get_by_label("Save").click();
            harness.run();
        }
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].chats, [42, 43]);
    }

    #[test]
    fn the_control_switch_matches_the_build_profile() {
        let mut data = admin();
        data.telegram = Some(Ok(TelegramConfig::default()));
        let mut view = TelegramView::default();
        let mut harness = Harness::builder().allow_missing_glyphs().build_ui(|ui| {
            telegram(ui, &data, &mut view);
        });
        harness.run();
        assert_eq!(
            harness
                .query_by_label("Allow ending, suspending and resuming processes and services")
                .is_some(),
            cfg!(feature = "telegram-control")
        );
        // Without the switch the page says why.
        assert_eq!(
            harness
                .query_by_label(
                    "This build only reports: the bot cannot end, suspend or resume processes \
                     or services."
                )
                .is_some(),
            !cfg!(feature = "telegram-control")
        );
    }
}
