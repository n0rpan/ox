//! One signed operator answer authorizes one entry into YOLO on this host.
use super::*;
use ox_desk_core::{host_protocol::AskView, modes::Level, runtime::Access};

pub(super) enum Activation {
    Session(String),
    Resume(String),
    Open {
        client: u64,
        key: String,
        action: Box<Action>,
        sender: Sender,
        source: Option<String>,
    },
    RemoteHandoff {
        client: u64,
        key: String,
        packet: Box<ox_desk_core::handoff::HandoffTransfer>,
    },
    Handoff {
        client: u64,
        key: String,
        start: HandoffStart,
    },
}

pub(super) struct ModeRequest {
    pub(super) scope: Scope,
    pub(super) activation: Activation,
    pub(super) view: AskView,
}

impl Engine {
    pub(super) fn waiting_open_client(&mut self, scope: &Scope, key: &str) -> Option<&mut u64> {
        if scope.route.runtime == ox_desk_core::runtime::Runtime::Antigravity
            && let Some((client, pending)) = self
                .running
                .get_mut(scope)
                .and_then(|live| live.opening.as_mut())
            && pending == key
        {
            return Some(client);
        }
        self.mode_requests
            .values_mut()
            .find_map(|r| match &mut r.activation {
                Activation::Open {
                    client,
                    key: opening,
                    ..
                } if &r.scope == scope && opening == key => Some(client),
                _ => None,
            })
    }

    fn finish_open_request(
        &mut self,
        scope: &Scope,
        client: u64,
        key: String,
        error: Option<String>,
    ) -> io::Result<()> {
        let frame = match error {
            Some(message) => Server::Error {
                id: Some(key.clone()),
                message,
            },
            None => Server::Ack { id: key.clone() },
        };
        self.receipts.finish(scope, &key, frame.clone());
        self.reply(client, frame)
    }
    pub(super) fn refresh_mode_policy(&mut self) -> io::Result<()> {
        let Ok(policy) = HostPolicy::read(&self.state_dir) else {
            // Mode-changing entry points report the invalid file. Keep stop and
            // approval traffic usable while the operator repairs it.
            return Ok(());
        };
        if self.last_mode_policy.replace(policy) == Some(policy) {
            return Ok(());
        }
        for live in self.running.values_mut() {
            live.store.use_host_policy(policy);
        }
        if policy != HostPolicy::YoloAsksEveryTime {
            let scopes: std::collections::HashSet<_> = self
                .mode_requests
                .values()
                .map(|r| r.scope.clone())
                .collect();
            self.withdraw_activations(
                |_| true,
                "The host mode policy changed. Review the session's mode again.",
            )?;
            for scope in scopes {
                if self.running.contains_key(&scope) {
                    self.process(&scope, vec![], true)?;
                }
            }
        }
        Ok(())
    }

    pub(super) fn withdraw_activations(
        &mut self,
        matches: impl Fn(&ModeRequest) -> bool,
        reason: &str,
    ) -> io::Result<()> {
        let jobs: Vec<_> = self
            .mode_requests
            .iter()
            .filter(|(_, r)| matches(r))
            .map(|(&job, _)| job)
            .collect();
        for job in jobs {
            let request = self.mode_requests.remove(&job).unwrap();
            match request.activation {
                Activation::Handoff { client, key, .. }
                | Activation::RemoteHandoff { client, key, .. } => {
                    self.answer(Replier::Client(client, key), Err(reason.to_owned()))?
                }
                Activation::Open { client, key, .. } => {
                    self.finish_open_request(&request.scope, client, key, Some(reason.to_owned()))?
                }
                Activation::Resume(id) => {
                    if let Some(live) = self.running.get_mut(&request.scope) {
                        live.store.cancel_held_resume(&id);
                    }
                }
                Activation::Session(_) => (),
            }
        }
        Ok(())
    }
    pub(super) fn ask_activation(
        &mut self,
        scope: &Scope,
        activation: Activation,
    ) -> io::Result<()> {
        if let Activation::Open {source, ..} = &activation
            && self
                .mode_requests
                .values()
                .any(|r| &r.scope == scope && matches!(&r.activation, Activation::Open {source: other, ..} if other == source))
        {
            return Err(io::Error::other(
                "Another session is waiting for approval on this route.",
            ));
        }
        let job = self.new_job_id().map_err(io::Error::other)?;
        let (thread, runtime, title, detail) = match &activation {
            Activation::Session(owner) => {
                let name = self.running[scope]
                    .store
                    .catalog_row(owner)
                    .map_or("Session", |row| row.title());
                (
                    Some(owner.clone()),
                    scope.route.runtime,
                    "Switch this session to YOLO?",
                    name.to_owned(),
                )
            }
            Activation::Resume(owner) => (
                Some(owner.clone()),
                scope.route.runtime,
                "Resume this session in YOLO?",
                self.running[scope]
                    .store
                    .catalog_row(owner)
                    .map_or("Saved session", |r| r.title())
                    .to_owned(),
            ),
            Activation::Open { source, .. } => (
                source.clone(),
                scope.route.runtime,
                "Open this session in YOLO?",
                scope.project.display().to_string(),
            ),
            Activation::RemoteHandoff { packet, .. } => (
                None,
                packet.start.runtime,
                "Start the incoming handoff in YOLO?",
                format!(
                    "From {} · {}\nTo {} · {}\n{}",
                    packet
                        .source
                        .host
                        .as_ref()
                        .map_or("Source host", |h| h.name.as_str()),
                    packet.source.title,
                    scope.project.display(),
                    scope.route.label(),
                    packet.start.request.chars().take(600).collect::<String>()
                ),
            ),
            Activation::Handoff { start, .. } => (
                Some(start.source.clone()),
                start.runtime,
                "Start the handoff in YOLO?",
                start.request.chars().take(600).collect(),
            ),
        };
        let lower = Level::Yolo.below_yolo().label(runtime);
        let view = AskView {
            id: crate::yolo::request_id(job),
            token: String::new(),
            thread,
            turn: None,
            item: None,
            command: None,
            title: title.to_owned(),
            detail,
            choices: vec![
                "Accept YOLO".into(),
                format!("Use {lower}"),
                "Decline".into(),
            ],
            choice_details: vec![
                Some("Run without permission prompts for this activation.".into()),
                None,
                None,
            ],
            questions: vec![],
        };
        self.mode_requests.insert(
            job,
            ModeRequest {
                scope: scope.clone(),
                activation,
                view,
            },
        );
        Ok(())
    }

    pub(super) fn answer_activation(
        &mut self,
        client: u64,
        scope: &Scope,
        job: u64,
        choice: usize,
    ) -> io::Result<()> {
        let answer = crate::yolo::Answer::of(choice)
            .ok_or_else(|| io::Error::other("Choose a mode or decline."))?;
        if self
            .mode_requests
            .get(&job)
            .is_none_or(|r| &r.scope != scope)
        {
            return Err(io::Error::other(
                "This host mode request is no longer active.",
            ));
        }
        let request = self.mode_requests.remove(&job).unwrap();
        match (request.activation, answer) {
            (
                Activation::Handoff {
                    client: owner, key, ..
                }
                | Activation::RemoteHandoff {
                    client: owner, key, ..
                },
                crate::yolo::Answer::Decline,
            ) => self.answer(
                Replier::Client(owner, key),
                Err("The operator declined this handoff.".into()),
            ),
            (
                Activation::Open {
                    client: owner, key, ..
                },
                crate::yolo::Answer::Decline,
            ) => self.finish_open_request(
                scope,
                owner,
                key,
                Some("The operator declined to open this session in YOLO.".into()),
            ),
            (Activation::Resume(id), crate::yolo::Answer::Decline) => self
                .running
                .get_mut(scope)
                .unwrap()
                .store
                .decline_resume_review(&id),
            (Activation::Resume(id), crate::yolo::Answer::Accept) => {
                self.running
                    .get_mut(scope)
                    .unwrap()
                    .store
                    .accept_resume_review(&id);
                Ok(())
            }
            (Activation::Resume(id), crate::yolo::Answer::Lower) => {
                let level = Level::Yolo.below_yolo();
                let result = self.act_with_mode_approval(
                    client,
                    "",
                    scope,
                    Action::SetAccess(id.clone(), level.preset(scope.route.runtime)),
                    None,
                    Some((level, self.sender(client))),
                );
                if result.is_err() {
                    self.mode_requests.insert(
                        job,
                        ModeRequest {
                            scope: scope.clone(),
                            activation: Activation::Resume(id),
                            view: request.view,
                        },
                    );
                }
                result
            }
            (_, crate::yolo::Answer::Decline) => Ok(()),
            (Activation::Session(owner), answer) => {
                let level = match answer {
                    crate::yolo::Answer::Accept => Level::Yolo,
                    _ => Level::Yolo.below_yolo(),
                };
                let access = level.preset(scope.route.runtime);
                self.act_with_mode_approval(
                    client,
                    "",
                    scope,
                    Action::SetAccess(owner, access),
                    None,
                    Some((level, self.sender(client))),
                )
            }
            (
                Activation::Open {
                    client: owner,
                    key,
                    sender,
                    action,
                    ..
                },
                answer,
            ) => {
                let level = match answer {
                    crate::yolo::Answer::Accept => Level::Yolo,
                    _ => Level::Yolo.below_yolo(),
                };
                let result = self.act_with_mode_approval(
                    owner,
                    &key,
                    scope,
                    *action,
                    None,
                    Some((level, sender)),
                );
                if result.is_ok()
                    && scope.route.runtime == ox_desk_core::runtime::Runtime::Antigravity
                {
                    // ACP startup still needs to confirm the native session.
                    return Ok(());
                }
                self.finish_open_request(
                    scope,
                    owner,
                    key,
                    result.as_ref().err().map(ToString::to_string),
                )?;
                result
            }
            (
                Activation::RemoteHandoff {
                    client: owner,
                    key,
                    mut packet,
                },
                answer,
            ) => {
                packet.start.mode = ox_desk_core::modes::ModeRequest::Level(match answer {
                    crate::yolo::Answer::Accept => Level::Yolo,
                    _ => Level::Yolo.below_yolo(),
                });
                let approval = YoloAllowance::Accepted {
                    request: format!("{}{job}", crate::yolo::PREFIX),
                    device: format!("client-{client}"),
                };
                self.receive_handoff(owner, key, scope.clone(), packet, Some(approval))
            }
            (
                Activation::Handoff {
                    client: owner,
                    key,
                    mut start,
                },
                answer,
            ) => {
                start.mode = ox_desk_core::modes::ModeRequest::Level(match answer {
                    crate::yolo::Answer::Accept => Level::Yolo,
                    _ => Level::Yolo.below_yolo(),
                });
                let approval = YoloAllowance::Accepted {
                    request: format!("{}{job}", crate::yolo::PREFIX),
                    device: format!("client-{client}"),
                };
                self.start_handoff_checked(owner, &key, scope, start, Some(approval))
            }
        }
    }

    pub(super) fn stage_mode_change(
        &mut self,
        scope: &Scope,
        owner: &str,
        mode: Access,
        approved: bool,
    ) -> io::Result<bool> {
        self.mode_requests.retain(|_, r| {
            !(r.scope == *scope
                && matches!(&r.activation, Activation::Session(session) if session == owner))
        });
        if approved || Level::of(scope.route.runtime, mode) != Some(Level::Yolo) {
            return Ok(false);
        }
        if HostPolicy::read(&self.state_dir)? != HostPolicy::YoloAsksEveryTime {
            return Ok(false);
        }
        self.ask_activation(scope, Activation::Session(owner.to_owned()))?;
        Ok(true)
    }
}
