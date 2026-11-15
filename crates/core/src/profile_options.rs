//! Agent profiles in the new-session and assistant dialogs: which profiles a
//! picker lists, the option form of the selected profile, the
//! `agent_options` map a create sends, and how remembered choices meet the
//! host's current profiles.
//!
//! Every id the form sends comes from the host's current `host.inspect`
//! answer; labels and reasons are shown sanitized.

use std::collections::BTreeMap;

use pohunek_client::protocol::{
    AgentOption, AgentOptionChoice, AgentOptionUnavailable, ProfileAudience,
};

use crate::badge::sanitize;
use crate::new_session::{AgentChoice, Choices};

/// Who a profile is for, as `host.inspect` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Audience {
    /// Listed in owner pickers; also what an absent field means.
    #[default]
    Owner,
    /// Meant for launches by agents.
    Agent,
    /// Hidden everywhere.
    Hidden,
    /// A value this build does not know; treated as hidden.
    Unknown,
}

impl Audience {
    pub(crate) fn from_wire(audience: Option<ProfileAudience>) -> Self {
        match audience {
            None | Some(ProfileAudience::Owner) => Self::Owner,
            Some(ProfileAudience::Agent) => Self::Agent,
            Some(ProfileAudience::Hidden) => Self::Hidden,
            Some(ProfileAudience::Unknown) => Self::Unknown,
        }
    }

    /// Listed without "Show all".
    pub fn listed_by_default(self) -> bool {
        self == Self::Owner
    }
}

/// Indices into `agents` that a picker lists, in the order of `agents`
/// (launchable ones first, as the choices are built). Owner profiles always;
/// the others only with `show_all`; the `selected` agent always, so a
/// template, a remembered choice or a host switch never loses its agent.
pub fn listed_agents(agents: &[AgentChoice], show_all: bool, selected: Option<&str>) -> Vec<usize> {
    agents
        .iter()
        .enumerate()
        .filter(|(_, agent)| {
            show_all || agent.audience.listed_by_default() || selected == Some(agent.name.as_str())
        })
        .map(|(index, _)| index)
        .collect()
}

/// Number of agents only "Show all" lists; the chip is offered when it is
/// not zero.
pub fn held_back_agents(agents: &[AgentChoice], selected: Option<&str>) -> usize {
    agents.len() - listed_agents(agents, false, selected).len()
}

/// One choice of an option, as the form shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChoiceField {
    pub id: String,
    pub label: String,
    /// Why the choice cannot be launched on this host; `None` when it can.
    pub unavailable: Option<String>,
}

/// One option of the selected profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptionField {
    pub id: String,
    pub label: String,
    pub choices: Vec<ChoiceField>,
    /// Index of the profile's default choice.
    pub default: usize,
    /// Index of the chosen choice.
    pub selected: usize,
}

impl OptionField {
    fn chosen(&self) -> &ChoiceField {
        &self.choices[self.selected]
    }
}

/// The option form of one agent; empty when the host does not accept
/// `agent_options` or the agent reports no options.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OptionForm {
    /// The agent the form belongs to.
    pub agent: String,
    pub fields: Vec<OptionField>,
}

/// An option whose chosen choice the host cannot launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockedChoice {
    pub option: String,
    pub choice: String,
    pub reason: String,
}

/// Why a reported choice is unavailable, as a sentence fragment.
fn unavailable_text(choice: &AgentOptionChoice) -> Option<String> {
    if choice.available {
        return None;
    }
    let floor = choice
        .min_runtime
        .as_deref()
        .map(sanitize)
        .filter(|floor| !floor.is_empty());
    Some(match (choice.reason, floor) {
        (Some(AgentOptionUnavailable::RuntimeTooOld), Some(floor)) => {
            format!("needs runtime {floor} or newer")
        }
        (Some(AgentOptionUnavailable::RuntimeTooOld), None) => "the runtime is too old".to_owned(),
        (Some(AgentOptionUnavailable::RuntimeVersionUnknown), Some(floor)) => {
            format!("needs runtime {floor}, and the runtime reports no version")
        }
        (Some(AgentOptionUnavailable::RuntimeVersionUnknown), None) => {
            "the runtime reports no version".to_owned()
        }
        _ => "not available on this host".to_owned(),
    })
}

fn label_or_id(label: &str, id: &str) -> String {
    let label = sanitize(label);
    if label.is_empty() {
        id.to_owned()
    } else {
        label
    }
}

/// The form field of a reported option; `None` when its default is not one
/// of its choices, which the daemon never reports.
fn option_field(option: &AgentOption) -> Option<OptionField> {
    let default = option
        .choices
        .iter()
        .position(|choice| choice.id == option.default)?;
    Some(OptionField {
        id: option.id.clone(),
        label: label_or_id(&option.label, &option.id),
        choices: option
            .choices
            .iter()
            .map(|choice| ChoiceField {
                id: choice.id.clone(),
                label: label_or_id(&choice.label, &choice.id),
                unavailable: unavailable_text(choice),
            })
            .collect(),
        default,
        selected: default,
    })
}

impl OptionForm {
    /// The form of `agent` at its defaults.
    pub fn for_agent(choices: &Choices, agent: &str) -> Self {
        let fields = match choices.agent(agent) {
            Some(found) if choices.agent_options_supported => {
                found.options.iter().filter_map(option_field).collect()
            }
            _ => Vec::new(),
        };
        Self {
            agent: agent.to_owned(),
            fields,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    /// Chooses choice `choice` of field `field`; an unavailable choice other
    /// than the default is not taken. Returns whether the choice was taken.
    pub fn select(&mut self, field: usize, choice: usize) -> bool {
        let Some(option) = self.fields.get_mut(field) else {
            return false;
        };
        let Some(candidate) = option.choices.get(choice) else {
            return false;
        };
        if candidate.unavailable.is_some() && choice != option.default {
            return false;
        }
        option.selected = choice;
        true
    }

    /// Sets every option back to its default.
    pub fn reset(&mut self) {
        for field in &mut self.fields {
            field.selected = field.default;
        }
    }

    /// The first option, in the reported order, whose chosen choice the
    /// host cannot launch.
    pub fn blocking_choice(&self) -> Option<BlockedChoice> {
        self.fields.iter().find_map(|field| {
            let chosen = field.chosen();
            chosen.unavailable.as_ref().map(|reason| BlockedChoice {
                option: field.label.clone(),
                choice: chosen.label.clone(),
                reason: reason.clone(),
            })
        })
    }

    /// Option id to choice id of every option not at its default.
    pub fn non_default(&self) -> BTreeMap<String, String> {
        self.fields
            .iter()
            .filter(|field| field.selected != field.default)
            .map(|field| (field.id.clone(), field.chosen().id.clone()))
            .collect()
    }

    /// Chooses the remembered choices that the form still offers as
    /// available; returns how many remembered entries it could not apply.
    pub fn apply_remembered(&mut self, remembered: &BTreeMap<String, String>) -> usize {
        let mut dropped = 0;
        for (option, choice) in remembered {
            let target = self.fields.iter().enumerate().find_map(|(index, field)| {
                (field.id == *option)
                    .then(|| field.choices.iter().position(|c| c.id == *choice))
                    .flatten()
                    .map(|position| (index, position))
            });
            match target {
                Some((field, position))
                    if self.fields[field].choices[position].unavailable.is_none() =>
                {
                    self.fields[field].selected = position;
                }
                _ => dropped += 1,
            }
        }
        dropped
    }
}

/// The `agent_options` a create of `agent` sends: the non-default choices of
/// `form`, each checked against the host's current report. Empty when the
/// host does not accept the field, the form belongs to another agent, the
/// agent reports no options, or every option is at its default; a choice
/// the host does not report, or reports as unavailable, is never sent.
pub fn outgoing_options(
    form: &OptionForm,
    choices: &Choices,
    agent: &str,
) -> BTreeMap<String, String> {
    if !choices.agent_options_supported || form.agent != agent {
        return BTreeMap::new();
    }
    let Some(reported) = choices.agent(agent) else {
        return BTreeMap::new();
    };
    form.non_default()
        .into_iter()
        .filter(|(option, choice)| {
            reported.options.iter().any(|declared| {
                declared.id == *option
                    && declared
                        .choices
                        .iter()
                        .any(|offered| offered.id == *choice && offered.available)
            })
        })
        .collect()
}

/// A remembered agent and choices, matched with the host's current report.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReconciledChoices {
    /// The remembered agent when the host lists it as launchable, under any
    /// audience.
    pub agent: Option<String>,
    /// The form of that agent with every remembered choice it still offers.
    pub options: OptionForm,
    /// Remembered choices that could not be applied.
    pub dropped: usize,
}

/// Matches a remembered `agent` and its non-default `options` with the
/// host's current `choices`.
pub fn reconcile(
    agent: &str,
    options: &BTreeMap<String, String>,
    choices: &Choices,
) -> ReconciledChoices {
    if choices.launchable_position(agent).is_none() {
        return ReconciledChoices {
            agent: None,
            options: OptionForm::default(),
            dropped: options.len(),
        };
    }
    let mut form = OptionForm::for_agent(choices, agent);
    let dropped = form.apply_remembered(options);
    ReconciledChoices {
        agent: Some(agent.to_owned()),
        options: form,
        dropped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choice(
        id: &str,
        available: bool,
        reason: Option<AgentOptionUnavailable>,
    ) -> AgentOptionChoice {
        AgentOptionChoice {
            id: id.to_owned(),
            label: id.to_uppercase(),
            available,
            reason,
            min_runtime: (!available).then(|| "9.9.0".to_owned()),
        }
    }

    fn option(id: &str, default: &str, choices: Vec<AgentOptionChoice>) -> AgentOption {
        AgentOption {
            id: id.to_owned(),
            label: format!("{id} label"),
            default: default.to_owned(),
            choices,
        }
    }

    fn agent(name: &str, audience: Audience, options: Vec<AgentOption>) -> AgentChoice {
        AgentChoice {
            name: name.to_owned(),
            label: name.to_owned(),
            audience,
            options,
            unlaunchable: None,
        }
    }

    /// `opt` has `mode` (plan default, build) and `model` (standard default,
    /// next unavailable).
    fn options() -> Vec<AgentOption> {
        vec![
            option(
                "mode",
                "plan",
                vec![choice("plan", true, None), choice("build", true, None)],
            ),
            option(
                "model",
                "standard",
                vec![
                    choice("standard", true, None),
                    choice(
                        "next",
                        false,
                        Some(AgentOptionUnavailable::RuntimeVersionUnknown),
                    ),
                ],
            ),
        ]
    }

    fn choices(supported: bool) -> Choices {
        Choices {
            agents: vec![
                agent("opt", Audience::Owner, options()),
                agent("plain", Audience::Owner, Vec::new()),
                agent("helper", Audience::Agent, Vec::new()),
                agent("secret", Audience::Hidden, Vec::new()),
                agent("future", Audience::Unknown, Vec::new()),
            ],
            agent_options_supported: supported,
            ..Choices::default()
        }
    }

    fn names(agents: &[AgentChoice], listed: &[usize]) -> Vec<String> {
        listed
            .iter()
            .map(|index| agents[*index].name.clone())
            .collect()
    }

    #[test]
    fn audience_decodes_with_absent_as_owner() {
        assert_eq!(Audience::from_wire(None), Audience::Owner);
        assert_eq!(
            Audience::from_wire(Some(ProfileAudience::Owner)),
            Audience::Owner
        );
        assert_eq!(
            Audience::from_wire(Some(ProfileAudience::Agent)),
            Audience::Agent
        );
        assert_eq!(
            Audience::from_wire(Some(ProfileAudience::Hidden)),
            Audience::Hidden
        );
        assert_eq!(
            Audience::from_wire(Some(ProfileAudience::Unknown)),
            Audience::Unknown
        );
    }

    #[test]
    fn the_picker_lists_owner_profiles_unless_show_all_is_on() {
        let agents = choices(true).agents;
        assert_eq!(
            names(&agents, &listed_agents(&agents, false, None)),
            ["opt", "plain"]
        );
        assert_eq!(
            names(&agents, &listed_agents(&agents, true, None)),
            ["opt", "plain", "helper", "secret", "future"]
        );
        assert_eq!(held_back_agents(&agents, None), 3);
    }

    #[test]
    fn the_selected_profile_is_listed_whatever_its_audience() {
        let agents = choices(true).agents;
        for selected in ["helper", "secret", "future"] {
            let listed = names(&agents, &listed_agents(&agents, false, Some(selected)));
            assert_eq!(listed, ["opt", "plain", selected]);
        }
        assert_eq!(held_back_agents(&agents, Some("secret")), 2);
    }

    #[test]
    fn the_form_starts_at_the_defaults_and_names_unavailable_choices() {
        let form = OptionForm::for_agent(&choices(true), "opt");
        assert_eq!(form.fields.len(), 2);
        assert_eq!(form.fields[0].selected, 0);
        assert_eq!(form.fields[1].choices[1].label, "NEXT");
        assert_eq!(
            form.fields[1].choices[1].unavailable.as_deref(),
            Some("needs runtime 9.9.0, and the runtime reports no version")
        );
        assert!(form.non_default().is_empty());
        assert!(form.blocking_choice().is_none());
    }

    #[test]
    fn reasons_cover_every_wire_value() {
        let text = |reason, floor: Option<&str>| {
            unavailable_text(&AgentOptionChoice {
                id: "c".into(),
                label: "C".into(),
                available: false,
                reason,
                min_runtime: floor.map(str::to_owned),
            })
        };
        assert_eq!(
            text(Some(AgentOptionUnavailable::RuntimeTooOld), Some("2.2.0")).as_deref(),
            Some("needs runtime 2.2.0 or newer")
        );
        assert_eq!(
            text(Some(AgentOptionUnavailable::RuntimeVersionUnknown), None).as_deref(),
            Some("the runtime reports no version")
        );
        assert_eq!(
            text(Some(AgentOptionUnavailable::Unknown), Some("1.0.0")).as_deref(),
            Some("not available on this host")
        );
        assert_eq!(
            text(None, None).as_deref(),
            Some("not available on this host")
        );
    }

    #[test]
    fn no_form_without_the_capability_or_without_options() {
        assert!(OptionForm::for_agent(&choices(false), "opt").is_empty());
        assert!(OptionForm::for_agent(&choices(true), "plain").is_empty());
        assert!(OptionForm::for_agent(&choices(true), "missing").is_empty());
    }

    #[test]
    fn an_unavailable_choice_is_not_taken_and_reset_restores_defaults() {
        let mut form = OptionForm::for_agent(&choices(true), "opt");
        assert!(!form.select(1, 1));
        assert!(form.select(0, 1));
        assert_eq!(
            form.non_default(),
            BTreeMap::from([("mode".to_owned(), "build".to_owned())])
        );
        form.reset();
        assert!(form.non_default().is_empty());
    }

    #[test]
    fn an_unavailable_default_blocks_create() {
        let mut reported = choices(true);
        reported.agents[0].options[1].default = "next".to_owned();
        let form = OptionForm::for_agent(&reported, "opt");
        assert_eq!(
            form.blocking_choice(),
            Some(BlockedChoice {
                option: "model label".to_owned(),
                choice: "NEXT".to_owned(),
                reason: "needs runtime 9.9.0, and the runtime reports no version".to_owned(),
            })
        );
        // The default can stay selected, so the form shows why Create waits.
        let mut form = form;
        assert!(form.select(1, 1));
        assert!(form.select(1, 0));
        assert!(form.blocking_choice().is_none());
    }

    #[test]
    fn the_outgoing_map_holds_only_reported_available_non_default_choices() {
        let reported = choices(true);
        let mut form = OptionForm::for_agent(&reported, "opt");
        assert!(outgoing_options(&form, &reported, "opt").is_empty());
        form.select(0, 1);
        let sent = outgoing_options(&form, &reported, "opt");
        assert_eq!(
            sent,
            BTreeMap::from([("mode".to_owned(), "build".to_owned())])
        );
        // The capability is gone.
        assert!(outgoing_options(&form, &choices(false), "opt").is_empty());
        // The form belongs to another agent.
        assert!(outgoing_options(&form, &reported, "plain").is_empty());
        // The host no longer reports the option or the choice, or reports it
        // as unavailable.
        let mut changed = choices(true);
        changed.agents[0].options.remove(0);
        assert!(outgoing_options(&form, &changed, "opt").is_empty());
        let mut changed = choices(true);
        changed.agents[0].options[0].choices.remove(1);
        assert!(outgoing_options(&form, &changed, "opt").is_empty());
        let mut changed = choices(true);
        changed.agents[0].options[0].choices[1].available = false;
        assert!(outgoing_options(&form, &changed, "opt").is_empty());
        // A locked preset or a bare runtime reports no options.
        let mut locked = choices(true);
        locked.agents[0].options.clear();
        assert!(outgoing_options(&form, &locked, "opt").is_empty());
    }

    #[test]
    fn remembered_choices_apply_when_still_offered() {
        let remembered = BTreeMap::from([("mode".to_owned(), "build".to_owned())]);
        let reconciled = reconcile("opt", &remembered, &choices(true));
        assert_eq!(reconciled.agent.as_deref(), Some("opt"));
        assert_eq!(reconciled.options.non_default(), remembered);
        assert_eq!(reconciled.dropped, 0);
    }

    #[test]
    fn remembered_choices_the_host_no_longer_offers_are_dropped() {
        let remembered = BTreeMap::from([
            ("mode".to_owned(), "build".to_owned()),
            ("model".to_owned(), "next".to_owned()),
            ("gone".to_owned(), "x".to_owned()),
        ]);
        // `model=next` is unavailable now and `gone` is not reported.
        let reconciled = reconcile("opt", &remembered, &choices(true));
        assert_eq!(
            reconciled.options.non_default(),
            BTreeMap::from([("mode".to_owned(), "build".to_owned())])
        );
        assert_eq!(reconciled.dropped, 2);
        // The `build` choice was removed from the profile.
        let mut changed = choices(true);
        changed.agents[0].options[0].choices.remove(1);
        let reconciled = reconcile("opt", &remembered, &changed);
        assert!(reconciled.options.non_default().is_empty());
        assert_eq!(reconciled.dropped, 3);
    }

    #[test]
    fn a_remembered_agent_no_longer_listed_is_dropped_with_its_choices() {
        let remembered = BTreeMap::from([("mode".to_owned(), "build".to_owned())]);
        let reconciled = reconcile("removed", &remembered, &choices(true));
        assert_eq!(reconciled.agent, None);
        assert_eq!(reconciled.dropped, 1);
        // A hidden profile is still preselected.
        let reconciled = reconcile("secret", &BTreeMap::new(), &choices(true));
        assert_eq!(reconciled.agent.as_deref(), Some("secret"));
    }

    #[test]
    fn without_the_capability_remembered_choices_are_dropped_and_the_agent_kept() {
        let remembered = BTreeMap::from([("mode".to_owned(), "build".to_owned())]);
        let reconciled = reconcile("opt", &remembered, &choices(false));
        assert_eq!(reconciled.agent.as_deref(), Some("opt"));
        assert!(reconciled.options.is_empty());
        assert_eq!(reconciled.dropped, 1);
    }
}
