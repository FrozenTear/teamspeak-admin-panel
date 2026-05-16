//! Automod rate-limit & false-positive safeguards — Phase 9.1.3 (PURA-302).
//!
//! False positives are the existential risk of automod: one misfiring
//! rule can mass-ban a community. This module implements the seven
//! defence layers of the Phase 9.1 brief §6, consulted by the `Moderate`
//! dispatcher ([`crate::flow::dispatch`]) before any TS6 effect.
//!
//! Layers, ordered by importance (brief §6):
//!
//! 1. **Shadow mode (default for every new rule).** A rule is `shadow`
//!    or `enforce`; an unknown rule defaults to `shadow`. In `shadow`
//!    the dispatcher records the case + audit row but applies **no** TS6
//!    effect. Operators promote a rule to `enforce` explicitly — via the
//!    `AUTOMOD_ENFORCE_RULES` boot allowlist or the runtime
//!    [`AutomodSafeguards::set_rule_mode`] setter.
//! 2. **Per-rule circuit breaker.** Enforce actions per rule are capped
//!    per sliding window. On breach the rule **auto-demotes to `shadow`**
//!    and the dispatcher raises an operator alert — a raid-detection bug
//!    cannot mass-ban.
//! 3. **Severity ceiling on `ban`.** Automod ban is temporary-only
//!    ([`check_severity_ceiling`]); a permanent ban from a flow is a
//!    config error and fails loud. The repeat-offender threshold is an
//!    optional knob, **disabled by default** (board §10).
//! 4. **Exemptions.** Allowlist of server groups + UIDs, checked before
//!    any effect.
//! 5. **Subject cooldown.** After actioning a subject, further automod
//!    effects on that subject are suppressed for a cooldown window —
//!    they fold into the existing case instead of kicking on every
//!    rejoin.
//! 6. **Idempotency.** Delivered by mechanisms outside this module: the
//!    engine's per-flow drop-on-busy semaphore ([`crate::flow::engine`])
//!    means a replayed trigger for a still-running flow is dropped, and
//!    the dispatcher's case-dedup folds a repeated effect into the open
//!    case rather than spawning a fresh one. The subject cooldown
//!    (layer 5) collapses what slips past those.
//! 7. **Global kill switch.** One operator toggle ([`AUTOMOD_KILL_SWITCH`]
//!    at boot, or [`AutomodSafeguards::set_kill_switch`] at runtime)
//!    forces *every* automod rule to `shadow` instantly.
//!
//! The decision splits in two so the shadow-by-default common path costs
//! nothing: [`AutomodSafeguards::gate`] resolves the kill switch + per-
//! rule mode with no I/O, and only an enforce-eligible rule pays the TS6
//! lookups that [`AutomodSafeguards::decide`] needs (subject groups,
//! prior case count).
//!
//! [`AUTOMOD_KILL_SWITCH`]: SafeguardConfig::from_env

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use ts6_manager_shared::flows::ModerateEffect;

/// Per-rule automod mode (brief §6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleMode {
    /// Record the case + audit row, apply no TS6 effect.
    Shadow,
    /// Apply the TS6 effect.
    Enforce,
}

/// Why an action was downgraded to `shadow` instead of enforced. Surfaced
/// on the case-action payload (`shadowReason`) so an operator reviewing
/// the would-have-done stream can see which safeguard fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShadowReason {
    /// The global kill switch is engaged (brief §6.7).
    KillSwitch,
    /// The rule is in `shadow` mode — the default for an unpromoted rule
    /// (brief §6.1).
    RuleShadowMode,
    /// The subject is on the exemption allowlist (brief §6.4).
    SubjectExempt,
    /// The subject is inside its post-action cooldown window (brief §6.5).
    SubjectCooldown,
    /// `ban` requires a repeat-offender history the subject does not yet
    /// have (brief §6.3, optional knob).
    RepeatOffenderFloor,
    /// The per-rule circuit breaker tripped; the rule was auto-demoted to
    /// `shadow` (brief §6.2).
    CircuitBreakerTripped,
}

impl ShadowReason {
    /// Stable wire string for the case-action `shadowReason` payload key.
    pub fn as_str(self) -> &'static str {
        match self {
            ShadowReason::KillSwitch => "killSwitch",
            ShadowReason::RuleShadowMode => "ruleShadowMode",
            ShadowReason::SubjectExempt => "subjectExempt",
            ShadowReason::SubjectCooldown => "subjectCooldown",
            ShadowReason::RepeatOffenderFloor => "repeatOffenderFloor",
            ShadowReason::CircuitBreakerTripped => "circuitBreakerTripped",
        }
    }
}

/// Outcome of the cheap pre-effect [`AutomodSafeguards::gate`]: whether
/// the rule may proceed to a full enforce decision, or resolves to
/// `shadow` with no subject lookup at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// Rule is in `enforce` mode and the kill switch is off — the caller
    /// must gather subject context and call [`AutomodSafeguards::decide`].
    EnforceEligible,
    /// Rule resolves to `shadow` without any TS6 round-trip.
    Shadowed(ShadowReason),
}

/// Outcome of the full [`AutomodSafeguards::decide`], run only for an
/// enforce-eligible rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Apply the TS6 effect.
    Enforce,
    /// Record the case but apply no effect.
    Shadow {
        reason: ShadowReason,
        /// `true` only when *this* call tripped the circuit breaker and
        /// auto-demoted the rule — the dispatcher raises one operator
        /// alert on the transition, not on every subsequent suppression.
        breaker_newly_tripped: bool,
    },
}

/// Subject context the enforce-eligibility decision needs (brief §6.3–6.5).
pub struct SubjectInput<'a> {
    /// Stable rule identifier — keys the circuit breaker and rule mode.
    pub rule_key: &'a str,
    /// The subject's durable TS6 UID — keys exemptions and the cooldown.
    pub subject_uid: &'a str,
    /// The subject's current server-group ids, for the group exemption.
    pub subject_server_groups: &'a [i64],
    /// The effect the rule wants to apply.
    pub effect: ModerateEffect,
    /// Count of prior `automod`-origin cases for this subject — consulted
    /// only when the repeat-offender knob is enabled.
    pub prior_automod_case_count: u32,
}

/// Static configuration for the safeguards. Built once at boot from env
/// ([`SafeguardConfig::from_env`]); [`SafeguardConfig::default`] supplies
/// the same values tests use.
#[derive(Debug, Clone)]
pub struct SafeguardConfig {
    /// Circuit breaker: max enforce actions per rule per window.
    pub breaker_max_actions: u32,
    /// Circuit breaker sliding-window length.
    pub breaker_window: Duration,
    /// Post-action per-subject cooldown window.
    pub subject_cooldown: Duration,
    /// UID exemption allowlist.
    pub exempt_uids: HashSet<String>,
    /// Server-group exemption allowlist (sgids).
    pub exempt_server_groups: HashSet<i64>,
    /// Minimum prior automod cases before `ban` may enforce. `None`
    /// disables the gate entirely — the board-accepted default (§10).
    pub ban_repeat_offender_threshold: Option<u32>,
    /// Rules that boot directly in `enforce` mode. Every other rule
    /// starts `shadow` (brief §6.1).
    pub enforce_rules: HashSet<String>,
    /// Boot-time kill switch — `true` forces every rule to `shadow`.
    pub kill_switch: bool,
}

impl Default for SafeguardConfig {
    fn default() -> Self {
        Self {
            breaker_max_actions: 10,
            breaker_window: Duration::from_secs(60),
            subject_cooldown: Duration::from_secs(300),
            exempt_uids: HashSet::new(),
            exempt_server_groups: HashSet::new(),
            ban_repeat_offender_threshold: None,
            enforce_rules: HashSet::new(),
            kill_switch: false,
        }
    }
}

impl SafeguardConfig {
    /// Load the config from environment variables. Unset / empty vars
    /// fall back to [`SafeguardConfig::default`]; a malformed value is
    /// logged and the default kept, so a typo never blocks boot.
    ///
    /// - `AUTOMOD_BREAKER_MAX_ACTIONS` — u32, default `10`.
    /// - `AUTOMOD_BREAKER_WINDOW` — duration (`60s`, `2m`…), default `60s`.
    /// - `AUTOMOD_SUBJECT_COOLDOWN` — duration, default `300s`.
    /// - `AUTOMOD_EXEMPT_UIDS` — comma-separated UID allowlist.
    /// - `AUTOMOD_EXEMPT_SERVER_GROUPS` — comma-separated sgid allowlist.
    /// - `AUTOMOD_BAN_REPEAT_OFFENDER_THRESHOLD` — u32; unset disables it.
    /// - `AUTOMOD_ENFORCE_RULES` — comma-separated rule-key allowlist.
    /// - `AUTOMOD_KILL_SWITCH` — `1` / `true` engages the kill switch.
    pub fn from_env() -> Self {
        let d = SafeguardConfig::default();
        Self {
            breaker_max_actions: env_u32("AUTOMOD_BREAKER_MAX_ACTIONS", d.breaker_max_actions),
            breaker_window: env_duration("AUTOMOD_BREAKER_WINDOW", d.breaker_window),
            subject_cooldown: env_duration("AUTOMOD_SUBJECT_COOLDOWN", d.subject_cooldown),
            exempt_uids: env_csv("AUTOMOD_EXEMPT_UIDS").into_iter().collect(),
            exempt_server_groups: env_csv("AUTOMOD_EXEMPT_SERVER_GROUPS")
                .iter()
                .filter_map(|s| s.parse::<i64>().ok())
                .collect(),
            ban_repeat_offender_threshold: env_opt_u32("AUTOMOD_BAN_REPEAT_OFFENDER_THRESHOLD"),
            enforce_rules: env_csv("AUTOMOD_ENFORCE_RULES").into_iter().collect(),
            kill_switch: env_flag("AUTOMOD_KILL_SWITCH"),
        }
    }
}

/// Runtime safeguard state — the rule modes, circuit-breaker windows and
/// subject cooldowns. Owned behind a [`Mutex`]; every critical section is
/// a few map operations with no `.await`, so contention is negligible.
#[derive(Debug)]
struct State {
    /// Live kill switch. Seeded from config; flipped by `set_kill_switch`.
    kill_switch: bool,
    /// Per-rule mode. A rule absent from the map defaults to `shadow`.
    rule_modes: HashMap<String, RuleMode>,
    /// Per-rule sliding window of recent enforce-action instants.
    breaker_windows: HashMap<String, VecDeque<Instant>>,
    /// Per-subject cooldown expiry instants.
    cooldowns: HashMap<String, Instant>,
}

/// The seven §6 safeguards as one component. Cheap to share behind an
/// `Arc`; the dispatcher holds one and a future operator-routes child can
/// reach the same handle to drive [`set_kill_switch`] / [`set_rule_mode`].
///
/// [`set_kill_switch`]: AutomodSafeguards::set_kill_switch
/// [`set_rule_mode`]: AutomodSafeguards::set_rule_mode
#[derive(Debug)]
pub struct AutomodSafeguards {
    config: SafeguardConfig,
    state: Mutex<State>,
}

impl AutomodSafeguards {
    /// Build the safeguards from a static config. `enforce_rules` seed
    /// the rule-mode map; every other rule defaults to `shadow`.
    pub fn new(config: SafeguardConfig) -> Self {
        let rule_modes = config
            .enforce_rules
            .iter()
            .map(|k| (k.clone(), RuleMode::Enforce))
            .collect();
        let kill_switch = config.kill_switch;
        Self {
            config,
            state: Mutex::new(State {
                kill_switch,
                rule_modes,
                breaker_windows: HashMap::new(),
                cooldowns: HashMap::new(),
            }),
        }
    }

    /// Build the safeguards from environment configuration.
    pub fn from_env() -> Self {
        Self::new(SafeguardConfig::from_env())
    }

    /// Cheap pre-effect gate (brief §6.1 / §6.7). Resolves the kill
    /// switch and per-rule mode with no I/O. Only an [`Gate::EnforceEligible`]
    /// result requires the caller to gather subject context and call
    /// [`decide`](Self::decide).
    pub fn gate(&self, rule_key: &str) -> Gate {
        let state = self.lock();
        if state.kill_switch {
            return Gate::Shadowed(ShadowReason::KillSwitch);
        }
        match state.rule_modes.get(rule_key).copied() {
            Some(RuleMode::Enforce) => Gate::EnforceEligible,
            // Unknown rule → shadow by default (brief §6.1).
            Some(RuleMode::Shadow) | None => Gate::Shadowed(ShadowReason::RuleShadowMode),
        }
    }

    /// Full enforce-eligibility decision (brief §6.2–6.5). Assumes
    /// [`gate`](Self::gate) already returned [`Gate::EnforceEligible`].
    /// `now` is injected so tests can drive the breaker window and the
    /// cooldown deterministically; production passes `Instant::now()`.
    ///
    /// Returning [`Decision::Enforce`] has two side effects: the enforce
    /// action is recorded against the rule's circuit-breaker window, and
    /// the subject's cooldown window is (re)started. A `Shadow` outcome
    /// consumes neither — a suppressed action neither spends breaker
    /// budget nor extends a cooldown.
    pub fn decide(&self, input: &SubjectInput<'_>, now: Instant) -> Decision {
        let mut state = self.lock();

        // §6.4 — exemptions. An exempt subject is never actioned.
        if self.config.exempt_uids.contains(input.subject_uid)
            || input
                .subject_server_groups
                .iter()
                .any(|g| self.config.exempt_server_groups.contains(g))
        {
            return Decision::Shadow {
                reason: ShadowReason::SubjectExempt,
                breaker_newly_tripped: false,
            };
        }

        // §6.5 — subject cooldown. A live cooldown suppresses the effect;
        // an expired entry is swept so the map cannot grow unbounded.
        match state.cooldowns.get(input.subject_uid).copied() {
            Some(expiry) if expiry > now => {
                return Decision::Shadow {
                    reason: ShadowReason::SubjectCooldown,
                    breaker_newly_tripped: false,
                };
            }
            Some(_) => {
                state.cooldowns.remove(input.subject_uid);
            }
            None => {}
        }

        // §6.3 — repeat-offender floor for `ban`. Disabled by default.
        if input.effect == ModerateEffect::Ban
            && let Some(threshold) = self.config.ban_repeat_offender_threshold
            && input.prior_automod_case_count < threshold
        {
            return Decision::Shadow {
                reason: ShadowReason::RepeatOffenderFloor,
                breaker_newly_tripped: false,
            };
        }

        // §6.2 — per-rule circuit breaker. Prune the rule's window of
        // aged-out entries, then measure it; the borrow is scoped so the
        // demotion / record paths below can touch `state` again.
        let rule = input.rule_key.to_string();
        let breached = {
            let window = state.breaker_windows.entry(rule.clone()).or_default();
            if let Some(cutoff) = now.checked_sub(self.config.breaker_window) {
                while window.front().is_some_and(|t| *t < cutoff) {
                    window.pop_front();
                }
            }
            window.len() as u32 >= self.config.breaker_max_actions
        };
        if breached {
            // Breach — auto-demote the rule to `shadow`. Every later
            // trigger now short-circuits in `gate`, so the breaker trips
            // exactly once per demotion.
            state.rule_modes.insert(rule, RuleMode::Shadow);
            return Decision::Shadow {
                reason: ShadowReason::CircuitBreakerTripped,
                breaker_newly_tripped: true,
            };
        }
        // Record this enforce action against the breaker window.
        state
            .breaker_windows
            .entry(rule)
            .or_default()
            .push_back(now);

        // Enforce — start the subject's cooldown so the next trigger on
        // this subject folds into the case instead of re-actioning.
        let expiry = now
            .checked_add(self.config.subject_cooldown)
            .unwrap_or(now);
        state
            .cooldowns
            .insert(input.subject_uid.to_string(), expiry);
        Decision::Enforce
    }

    /// `true` when the repeat-offender knob is enabled — lets the
    /// dispatcher skip the prior-case-count query on the default path.
    pub fn repeat_offender_enabled(&self) -> bool {
        self.config.ban_repeat_offender_threshold.is_some()
    }

    /// Engage / release the global kill switch (brief §6.7).
    pub fn set_kill_switch(&self, on: bool) {
        self.lock().kill_switch = on;
    }

    /// `true` when the global kill switch is engaged.
    pub fn kill_switch(&self) -> bool {
        self.lock().kill_switch
    }

    /// Operator promote / demote of a single rule (brief §6.1).
    pub fn set_rule_mode(&self, rule_key: &str, mode: RuleMode) {
        self.lock()
            .rule_modes
            .insert(rule_key.to_string(), mode);
    }

    /// Current effective mode of a rule — `shadow` for an unknown rule.
    /// Observability surface for tests and a future operator route.
    pub fn effective_mode(&self, rule_key: &str) -> RuleMode {
        let state = self.lock();
        if state.kill_switch {
            return RuleMode::Shadow;
        }
        state
            .rule_modes
            .get(rule_key)
            .copied()
            .unwrap_or(RuleMode::Shadow)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        // Critical sections never `.await`; the only failure mode is a
        // panic mid-section, which we recover from rather than propagate.
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// Severity ceiling (brief §6.3) — automod `ban` is temporary-only.
/// A permanent ban (`ban` with no `duration_secs`) from a flow is a
/// configuration error: it returns `Err` so the action fails loud
/// regardless of the rule's shadow/enforce mode. Permanent bans stay an
/// operator-only primitive.
pub fn check_severity_ceiling(
    effect: ModerateEffect,
    duration_secs: Option<u32>,
) -> Result<(), String> {
    if effect == ModerateEffect::Ban && duration_secs.is_none() {
        return Err(
            "Moderate: automod `ban` must be temporary — set `duration_secs`; \
             a permanent ban is operator-only (brief §6.3)"
                .to_string(),
        );
    }
    Ok(())
}

// ---- env parsing helpers -----------------------------------------------
//
// Mirror the `crate::config` posture: an unset / empty var falls back to
// the default, a malformed value is logged and the default kept.

fn env_csv(key: &str) -> Vec<String> {
    match std::env::var(key) {
        Ok(v) => v
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn env_u32(key: &str, default: u32) -> u32 {
    match std::env::var(key) {
        Ok(v) if !v.is_empty() => v.parse().unwrap_or_else(|_| {
            tracing::warn!(env = key, value = %v, "invalid u32; using default");
            default
        }),
        _ => default,
    }
}

fn env_opt_u32(key: &str) -> Option<u32> {
    match std::env::var(key) {
        Ok(v) if !v.is_empty() => match v.parse() {
            Ok(n) => Some(n),
            Err(_) => {
                tracing::warn!(env = key, value = %v, "invalid u32; ignoring");
                None
            }
        },
        _ => None,
    }
}

fn env_duration(key: &str, default: Duration) -> Duration {
    match std::env::var(key) {
        Ok(v) if !v.is_empty() => crate::config::parse_duration(&v).unwrap_or_else(|_| {
            tracing::warn!(env = key, value = %v, "invalid duration; using default");
            default
        }),
        _ => default,
    }
}

fn env_flag(key: &str) -> bool {
    matches!(
        std::env::var(key).as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE") | Ok("True")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> SafeguardConfig {
        SafeguardConfig::default()
    }

    fn input<'a>(
        rule_key: &'a str,
        uid: &'a str,
        groups: &'a [i64],
        effect: ModerateEffect,
    ) -> SubjectInput<'a> {
        SubjectInput {
            rule_key,
            subject_uid: uid,
            subject_server_groups: groups,
            effect,
            prior_automod_case_count: 0,
        }
    }

    // ---- layer 1: shadow mode is the default --------------------------

    #[test]
    fn unknown_rule_defaults_to_shadow() {
        let sg = AutomodSafeguards::new(cfg());
        assert_eq!(
            sg.gate("never-seen"),
            Gate::Shadowed(ShadowReason::RuleShadowMode)
        );
        assert_eq!(sg.effective_mode("never-seen"), RuleMode::Shadow);
    }

    #[test]
    fn enforce_rules_config_boots_a_rule_into_enforce() {
        let mut c = cfg();
        c.enforce_rules.insert("promoted".to_string());
        let sg = AutomodSafeguards::new(c);
        assert_eq!(sg.gate("promoted"), Gate::EnforceEligible);
        assert_eq!(sg.gate("other"), Gate::Shadowed(ShadowReason::RuleShadowMode));
    }

    #[test]
    fn set_rule_mode_promotes_and_demotes() {
        let sg = AutomodSafeguards::new(cfg());
        sg.set_rule_mode("r", RuleMode::Enforce);
        assert_eq!(sg.gate("r"), Gate::EnforceEligible);
        sg.set_rule_mode("r", RuleMode::Shadow);
        assert_eq!(sg.gate("r"), Gate::Shadowed(ShadowReason::RuleShadowMode));
    }

    // ---- layer 7: global kill switch ----------------------------------

    #[test]
    fn kill_switch_forces_every_rule_to_shadow() {
        let mut c = cfg();
        c.enforce_rules.insert("r".to_string());
        let sg = AutomodSafeguards::new(c);
        assert_eq!(sg.gate("r"), Gate::EnforceEligible);

        sg.set_kill_switch(true);
        assert_eq!(sg.gate("r"), Gate::Shadowed(ShadowReason::KillSwitch));
        assert_eq!(sg.effective_mode("r"), RuleMode::Shadow);

        sg.set_kill_switch(false);
        assert_eq!(sg.gate("r"), Gate::EnforceEligible);
    }

    #[test]
    fn kill_switch_can_boot_engaged() {
        let mut c = cfg();
        c.enforce_rules.insert("r".to_string());
        c.kill_switch = true;
        let sg = AutomodSafeguards::new(c);
        assert!(sg.kill_switch());
        assert_eq!(sg.gate("r"), Gate::Shadowed(ShadowReason::KillSwitch));
    }

    // ---- layer 3: severity ceiling ------------------------------------

    #[test]
    fn permanent_automod_ban_is_rejected() {
        assert!(check_severity_ceiling(ModerateEffect::Ban, None).is_err());
        assert!(check_severity_ceiling(ModerateEffect::Ban, Some(3600)).is_ok());
        // Non-ban effects never need a duration.
        assert!(check_severity_ceiling(ModerateEffect::Kick, None).is_ok());
        assert!(check_severity_ceiling(ModerateEffect::Warn, None).is_ok());
        assert!(check_severity_ceiling(ModerateEffect::Mute, None).is_ok());
    }

    // ---- layer 3: repeat-offender floor -------------------------------

    #[test]
    fn repeat_offender_floor_disabled_by_default() {
        let sg = AutomodSafeguards::new(cfg());
        assert!(!sg.repeat_offender_enabled());
        sg.set_rule_mode("r", RuleMode::Enforce);
        // A first-offence ban enforces because the knob is off.
        let mut inp = input("r", "uid", &[], ModerateEffect::Ban);
        inp.prior_automod_case_count = 0;
        assert_eq!(sg.decide(&inp, Instant::now()), Decision::Enforce);
    }

    #[test]
    fn repeat_offender_floor_suppresses_first_offence_ban() {
        let mut c = cfg();
        c.ban_repeat_offender_threshold = Some(2);
        let sg = AutomodSafeguards::new(c);
        sg.set_rule_mode("r", RuleMode::Enforce);

        // 0 prior cases < 2 → shadow.
        let mut inp = input("r", "uid", &[], ModerateEffect::Ban);
        inp.prior_automod_case_count = 0;
        assert_eq!(
            sg.decide(&inp, Instant::now()),
            Decision::Shadow {
                reason: ShadowReason::RepeatOffenderFloor,
                breaker_newly_tripped: false,
            }
        );

        // 2 prior cases ≥ 2 → enforce.
        inp.prior_automod_case_count = 2;
        assert_eq!(sg.decide(&inp, Instant::now()), Decision::Enforce);

        // The floor only gates `ban` — a kick enforces regardless.
        let kick = input("r", "uid2", &[], ModerateEffect::Kick);
        assert_eq!(sg.decide(&kick, Instant::now()), Decision::Enforce);
    }

    // ---- layer 4: exemptions ------------------------------------------

    #[test]
    fn exempt_uid_is_never_actioned() {
        let mut c = cfg();
        c.exempt_uids.insert("trusted-uid".to_string());
        let sg = AutomodSafeguards::new(c);
        sg.set_rule_mode("r", RuleMode::Enforce);

        let exempt = input("r", "trusted-uid", &[], ModerateEffect::Kick);
        assert_eq!(
            sg.decide(&exempt, Instant::now()),
            Decision::Shadow {
                reason: ShadowReason::SubjectExempt,
                breaker_newly_tripped: false,
            }
        );
        // A non-exempt subject still enforces.
        let other = input("r", "rando", &[], ModerateEffect::Kick);
        assert_eq!(sg.decide(&other, Instant::now()), Decision::Enforce);
    }

    #[test]
    fn exempt_server_group_is_never_actioned() {
        let mut c = cfg();
        c.exempt_server_groups.insert(6); // e.g. the admin group
        let sg = AutomodSafeguards::new(c);
        sg.set_rule_mode("r", RuleMode::Enforce);

        let admin = input("r", "uid", &[2, 6, 9], ModerateEffect::Kick);
        assert_eq!(
            sg.decide(&admin, Instant::now()),
            Decision::Shadow {
                reason: ShadowReason::SubjectExempt,
                breaker_newly_tripped: false,
            }
        );
        let member = input("r", "uid2", &[2, 9], ModerateEffect::Kick);
        assert_eq!(sg.decide(&member, Instant::now()), Decision::Enforce);
    }

    // ---- layer 5: subject cooldown ------------------------------------

    #[test]
    fn subject_cooldown_suppresses_repeat_then_expires() {
        let mut c = cfg();
        c.subject_cooldown = Duration::from_secs(300);
        let sg = AutomodSafeguards::new(c);
        sg.set_rule_mode("r", RuleMode::Enforce);

        let t0 = Instant::now();
        let inp = input("r", "uid", &[], ModerateEffect::Kick);
        // First action enforces and arms the cooldown.
        assert_eq!(sg.decide(&inp, t0), Decision::Enforce);
        // A rejoin 100s later is inside the window → shadow.
        assert_eq!(
            sg.decide(&inp, t0 + Duration::from_secs(100)),
            Decision::Shadow {
                reason: ShadowReason::SubjectCooldown,
                breaker_newly_tripped: false,
            }
        );
        // 301s later the cooldown has expired → enforce again.
        assert_eq!(
            sg.decide(&inp, t0 + Duration::from_secs(301)),
            Decision::Enforce
        );
    }

    #[test]
    fn cooldown_is_per_subject() {
        let sg = AutomodSafeguards::new(cfg());
        sg.set_rule_mode("r", RuleMode::Enforce);
        let t0 = Instant::now();
        assert_eq!(
            sg.decide(&input("r", "uid-a", &[], ModerateEffect::Kick), t0),
            Decision::Enforce
        );
        // A different subject is unaffected by uid-a's cooldown.
        assert_eq!(
            sg.decide(
                &input("r", "uid-b", &[], ModerateEffect::Kick),
                t0 + Duration::from_secs(1)
            ),
            Decision::Enforce
        );
    }

    // ---- layer 2: circuit breaker -------------------------------------

    #[test]
    fn circuit_breaker_caps_a_misfiring_rule() {
        let mut c = cfg();
        c.breaker_max_actions = 5;
        c.breaker_window = Duration::from_secs(60);
        // Disable the cooldown so it doesn't mask the breaker — each
        // trigger is a distinct subject anyway, but be explicit.
        c.subject_cooldown = Duration::from_secs(0);
        let sg = AutomodSafeguards::new(c);
        sg.set_rule_mode("raid-rule", RuleMode::Enforce);

        let t0 = Instant::now();
        let mut enforced = 0u32;
        let mut shadowed = 0u32;
        // A misfiring rule fires 100 times inside one window.
        for i in 0..100 {
            let uid = format!("victim-{i}");
            let inp = input("raid-rule", &uid, &[], ModerateEffect::Kick);
            match sg.decide(&inp, t0 + Duration::from_millis(i)) {
                Decision::Enforce => enforced += 1,
                Decision::Shadow { .. } => shadowed += 1,
            }
        }
        // At most `breaker_max_actions` real effects — no mass-action.
        assert_eq!(enforced, 5, "breaker caps enforce actions");
        assert_eq!(shadowed, 95);
        // The breach auto-demoted the rule to shadow (brief §6.2).
        assert_eq!(
            sg.gate("raid-rule"),
            Gate::Shadowed(ShadowReason::RuleShadowMode)
        );
    }

    #[test]
    fn circuit_breaker_trips_exactly_once() {
        let mut c = cfg();
        c.breaker_max_actions = 2;
        c.subject_cooldown = Duration::from_secs(0);
        let sg = AutomodSafeguards::new(c);
        sg.set_rule_mode("r", RuleMode::Enforce);

        let t0 = Instant::now();
        let d = |n: u64| {
            sg.decide(
                &input("r", &format!("u{n}"), &[], ModerateEffect::Kick),
                t0 + Duration::from_millis(n),
            )
        };
        assert_eq!(d(0), Decision::Enforce);
        assert_eq!(d(1), Decision::Enforce);
        // 3rd action breaches the cap — this call trips the breaker.
        assert_eq!(
            d(2),
            Decision::Shadow {
                reason: ShadowReason::CircuitBreakerTripped,
                breaker_newly_tripped: true,
            }
        );
        // The rule is now demoted; the gate short-circuits before
        // `decide` runs, so the breaker never re-trips.
        assert_eq!(sg.gate("r"), Gate::Shadowed(ShadowReason::RuleShadowMode));
    }

    #[test]
    fn circuit_breaker_window_slides() {
        let mut c = cfg();
        c.breaker_max_actions = 3;
        c.breaker_window = Duration::from_secs(60);
        c.subject_cooldown = Duration::from_secs(0);
        let sg = AutomodSafeguards::new(c);
        sg.set_rule_mode("r", RuleMode::Enforce);

        let t0 = Instant::now();
        let d = |t: Instant| {
            sg.decide(&input("r", "u", &[], ModerateEffect::Kick), t)
        };
        // Three actions fill the window.
        assert_eq!(d(t0), Decision::Enforce);
        assert_eq!(d(t0 + Duration::from_secs(10)), Decision::Enforce);
        assert_eq!(d(t0 + Duration::from_secs(20)), Decision::Enforce);
        // 90s after t0 the first three have aged out of the 60s window,
        // so a 4th action enforces rather than tripping the breaker.
        assert_eq!(d(t0 + Duration::from_secs(90)), Decision::Enforce);
    }

    #[test]
    fn breaker_is_per_rule() {
        let mut c = cfg();
        c.breaker_max_actions = 1;
        c.subject_cooldown = Duration::from_secs(0);
        let sg = AutomodSafeguards::new(c);
        sg.set_rule_mode("rule-a", RuleMode::Enforce);
        sg.set_rule_mode("rule-b", RuleMode::Enforce);

        let t0 = Instant::now();
        // rule-a burns its single slot and trips.
        assert_eq!(
            sg.decide(&input("rule-a", "u1", &[], ModerateEffect::Kick), t0),
            Decision::Enforce
        );
        assert!(matches!(
            sg.decide(&input("rule-a", "u2", &[], ModerateEffect::Kick), t0),
            Decision::Shadow { .. }
        ));
        // rule-b is independent — its slot is untouched.
        assert_eq!(
            sg.decide(&input("rule-b", "u3", &[], ModerateEffect::Kick), t0),
            Decision::Enforce
        );
    }
}
