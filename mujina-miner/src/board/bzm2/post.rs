//! Power-on self test: compare the machine in front of us against what this
//! platform variant says it should be.
//!
//! WHY THIS IS DATA AND NOT A CONDITIONAL.
//!
//! The fan preflight originally decided its own policy in code: no fan nodes
//! at all meant "this descriptor does not describe this host" and warned,
//! while fans that existed and would not turn refused the bring-up. That
//! heuristic was invented at the keyboard, and it cannot express the thing it
//! most needs to: an immersion chassis has no fans *by design*, and an
//! air-cooled one missing all four is a machine that must not start. Both
//! present as zero fan nodes. No amount of probing distinguishes them, because
//! the difference is not observable — **it is declared.**
//!
//! So fitment is declared per variant, in `platforms/*.json`, and the code
//! only compares. `board/bzm2/platform.rs` keeps the *wiring* — board count,
//! I²C adapter numbering, chain-port naming — as a `const` per machine,
//! because that is a property of a chassis and changing it is a code change.
//! Which of those slots is populated, and what it means when one is not, is a
//! property of a *unit* and multiplies combinatorially: air or immersion,
//! times three, two, one or no hashboards. Those are not consts; they are
//! configuration, and a config file is the honest place for them.
//!
//! THREE VERDICTS, BECAUSE TWO LOSE THE CASE THAT MATTERS.
//!
//! - `Block`  — do not start. An air-cooled chassis with a dead fan.
//! - `Warn`   — start, loudly. A fitted hashboard that did not answer.
//! - `Note`   — record it; it decides nothing. A dead ASIC on a board this
//!   variant does not fit.
//!
//! Collapsing `Note` into `Warn` is what makes an operator stop reading the
//! warnings. Collapsing `Warn` into `Block` is what makes a machine with one
//! dead device refuse to mine on its other 299.

use std::collections::BTreeMap;

use serde::Deserialize;

/// What the POST decided about one component.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    /// Recorded, decides nothing.
    Note,
    /// Start, but say so loudly.
    Warn,
    /// Do not start.
    Block,
}

impl Verdict {
    pub fn label(self) -> &'static str {
        match self {
            Verdict::Note => "note",
            Verdict::Warn => "WARN",
            Verdict::Block => "BLOCK",
        }
    }
}

/// What we observed about one component.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// It is there and it works.
    Present,
    /// It is not there at all.
    Absent,
    /// It is there and it does not work. Distinct from absent: a fan that
    /// answers its tacho with zero is a different fault from a fan with no
    /// tacho node, and an operator acts differently on each.
    Failed,
}

impl State {
    pub fn label(self) -> &'static str {
        match self {
            State::Present => "present",
            State::Absent => "absent",
            State::Failed => "FAILED",
        }
    }
}

/// One component we looked at.
#[derive(Debug, Clone)]
pub struct Observation {
    pub kind: String,
    pub index: usize,
    /// The slot this sits in, for components that have a parent — an ASIC's
    /// hashboard. `None` for top-level components.
    pub parent_index: Option<usize>,
    pub state: State,
    /// What was actually measured, for the record.
    pub detail: String,
}

#[derive(Debug, Clone)]
pub struct Finding {
    pub kind: String,
    pub index: usize,
    pub state: State,
    pub verdict: Verdict,
    pub why: String,
    pub detail: String,
}

#[derive(Debug, Deserialize)]
struct Rule {
    kind: String,
    #[serde(default)]
    when: BTreeMap<String, String>,
    #[serde(default)]
    parent: Option<String>,
    absent: Verdict,
    failed: Verdict,
    why: String,
}

#[derive(Debug, Deserialize)]
struct Variant {
    title: String,
    cooling: String,
    fitted: BTreeMap<String, Vec<usize>>,
}

#[derive(Debug, Deserialize)]
struct ParentNotFitted {
    verdict: Verdict,
    why: String,
}

#[derive(Debug, Deserialize)]
pub struct PlatformDef {
    pub class: String,
    pub title: String,
    default_variant: String,
    variants: BTreeMap<String, Variant>,
    rules: Vec<Rule>,
    parent_not_fitted: ParentNotFitted,
}

impl PlatformDef {
    pub fn parse(json: &str) -> anyhow::Result<Self> {
        Ok(serde_json::from_str(json)?)
    }

    pub fn default_variant(&self) -> &str {
        &self.default_variant
    }

    /// The human name of a variant, for the operator reading the verdict.
    pub fn variant_title(&self, name: &str) -> Option<&str> {
        self.variants.get(name).map(|v| v.title.as_str())
    }

    pub fn variant_titles(&self) -> Vec<(&str, &str)> {
        self.variants
            .iter()
            .map(|(k, v)| (k.as_str(), v.title.as_str()))
            .collect()
    }

    /// Is this slot populated in this variant?
    ///
    /// `None` means the variant says NOTHING about this kind -- which is not
    /// the same as saying the slot is empty. A kind absent from the fitted map
    /// is a gap in the definition and must reach the caller as one; a kind
    /// present with this index missing is a slot deliberately left unfitted.
    /// Collapsing those two is how an unknown component would quietly pass.
    fn fitted(&self, variant: &Variant, kind: &str, index: usize) -> Option<bool> {
        variant.fitted.get(kind).map(|list| list.contains(&index))
    }

    /// Judge every observation against what this variant declares.
    ///
    /// An unknown variant is an ERROR, never a fallback to the default. A POST
    /// that silently judged a machine against the wrong contract would be
    /// worse than no POST: it would report a verdict nobody asked for.
    pub fn evaluate(
        &self,
        variant_name: &str,
        observed: &[Observation],
    ) -> anyhow::Result<Vec<Finding>> {
        let variant = self.variants.get(variant_name).ok_or_else(|| {
            anyhow::anyhow!(
                "no variant {variant_name:?} in platform {} ({}); known:\n{}",
                self.class,
                self.title,
                self.variant_titles()
                    .iter()
                    .map(|(k, t)| format!("  {k} — {t}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        })?;

        let mut out = Vec::new();
        for obs in observed {
            if obs.state == State::Present {
                continue;
            }

            // THE PARENT RULE, and it comes first because it overrides
            // everything: a component on a slot this variant does not fit
            // cannot be a fault of this configuration.
            if let Some(rule_parent) = self
                .rules
                .iter()
                .find(|r| r.kind == obs.kind)
                .and_then(|r| r.parent.as_deref())
                && let Some(pidx) = obs.parent_index
                && self.fitted(variant, rule_parent, pidx) == Some(false)
            {
                out.push(Finding {
                    kind: obs.kind.clone(),
                    index: obs.index,
                    state: obs.state,
                    verdict: self.parent_not_fitted.verdict,
                    why: format!(
                        "{} {pidx} is not fitted in variant {variant_name}. {}",
                        rule_parent, self.parent_not_fitted.why
                    ),
                    detail: obs.detail.clone(),
                });
                continue;
            }

            // A slot this variant does not fit, for a top-level component:
            // absent is the expected shape of the machine.
            if obs.parent_index.is_none()
                && self.fitted(variant, &obs.kind, obs.index) == Some(false)
            {
                out.push(Finding {
                    kind: obs.kind.clone(),
                    index: obs.index,
                    state: obs.state,
                    verdict: self.parent_not_fitted.verdict,
                    why: format!(
                        "{} {} is not fitted in variant {variant_name}; its absence is \
                         declared, not discovered.",
                        obs.kind, obs.index
                    ),
                    detail: obs.detail.clone(),
                });
                continue;
            }

            let Some(rule) = self.rules.iter().find(|r| {
                r.kind == obs.kind
                    && r.when
                        .iter()
                        .all(|(k, v)| k == "cooling" && *v == variant.cooling)
            }) else {
                // NO RULE IS NOT A PASS. A component the definition does not
                // describe is a gap in the definition, and saying so is the
                // only way it gets closed.
                out.push(Finding {
                    kind: obs.kind.clone(),
                    index: obs.index,
                    state: obs.state,
                    verdict: Verdict::Warn,
                    why: format!(
                        "platform {} declares no rule for a {} in variant {variant_name} \
                         (cooling {}), so what this means is UNDECLARED -- not benign.",
                        self.class, obs.kind, variant.cooling
                    ),
                    detail: obs.detail.clone(),
                });
                continue;
            };

            out.push(Finding {
                kind: obs.kind.clone(),
                index: obs.index,
                state: obs.state,
                verdict: match obs.state {
                    State::Absent => rule.absent,
                    State::Failed => rule.failed,
                    State::Present => unreachable!("filtered above"),
                },
                why: rule.why.clone(),
                detail: obs.detail.clone(),
            });
        }
        Ok(out)
    }
}

/// The worst verdict in a set of findings, or `None` if everything passed.
pub fn worst(findings: &[Finding]) -> Option<Verdict> {
    findings.iter().map(|f| f.verdict).max()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEF: &str = include_str!("../../../../platforms/rds-dvt2.json");

    fn def() -> PlatformDef {
        PlatformDef::parse(DEF).expect("the shipped platform definition must parse")
    }

    fn obs(kind: &str, index: usize, parent: Option<usize>, state: State) -> Observation {
        Observation {
            kind: kind.into(),
            index,
            parent_index: parent,
            state,
            detail: String::new(),
        }
    }

    #[test]
    fn the_shipped_definition_parses_and_names_its_default() {
        let d = def();
        assert_eq!(d.class, "rds-dvt2");
        assert!(
            d.variant_titles()
                .iter()
                .any(|(k, _)| *k == d.default_variant())
        );
    }

    /// "It must have all fans when air cooled."
    #[test]
    fn an_air_cooled_chassis_blocks_on_a_dead_fan() {
        let d = def();
        let f = d
            .evaluate("air-3b", &[obs("fan", 2, None, State::Failed)])
            .unwrap();
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].verdict, Verdict::Block);
        assert_eq!(worst(&f), Some(Verdict::Block));
    }

    #[test]
    fn an_air_cooled_chassis_blocks_on_a_missing_fan() {
        let d = def();
        let f = d
            .evaluate("air-3b", &[obs("fan", 0, None, State::Absent)])
            .unwrap();
        assert_eq!(f[0].verdict, Verdict::Block);
    }

    /// The case the old hand-written heuristic could not express at all: an
    /// immersion chassis has no fans BY DESIGN, and zero fan nodes looks
    /// identical to an air-cooled machine with four dead ones. The difference
    /// is declared, not observable.
    #[test]
    fn an_immersion_chassis_does_not_block_on_absent_fans() {
        let d = def();
        let all_absent: Vec<_> = (0..4).map(|i| obs("fan", i, None, State::Absent)).collect();
        let air = d.evaluate("air-3b", &all_absent).unwrap();
        assert_eq!(
            worst(&air),
            Some(Verdict::Block),
            "air-cooled: must not start"
        );
        let imm = d.evaluate("immersion-3b", &all_absent).unwrap();
        assert_eq!(
            worst(&imm),
            Some(Verdict::Note),
            "immersion: expected shape"
        );
    }

    /// "It has 3 hashboards but doesn't have to have all 3 or even any to run."
    #[test]
    fn a_missing_hashboard_warns_and_does_not_block() {
        let d = def();
        let f = d
            .evaluate("air-3b", &[obs("hashboard", 1, None, State::Absent)])
            .unwrap();
        assert_eq!(f[0].verdict, Verdict::Warn);
        assert_ne!(worst(&f), Some(Verdict::Block));
    }

    #[test]
    fn a_control_board_with_no_hashboards_at_all_can_still_start() {
        let d = def();
        let none: Vec<_> = (0..3)
            .map(|i| obs("hashboard", i, None, State::Absent))
            .collect();
        let f = d.evaluate("air-0b", &none).unwrap();
        assert_eq!(
            worst(&f),
            Some(Verdict::Note),
            "declared absent, so not even a warning"
        );
    }

    /// "A failed asic on a hashboard we already excluded is interesting, but
    /// not actionable or a blocker."
    #[test]
    fn a_failed_asic_on_an_unfitted_hashboard_is_only_interesting() {
        let d = def();
        let f = d
            .evaluate("air-1b", &[obs("asic", 42, Some(2), State::Failed)])
            .unwrap();
        assert_eq!(f[0].verdict, Verdict::Note);
        assert!(f[0].why.contains("not fitted"), "{}", f[0].why);
    }

    /// ...and the same ASIC on a board we ARE driving is loud, because it
    /// changes the hashrate to expect and may be the first sign of a fault.
    #[test]
    fn the_same_failed_asic_on_a_fitted_hashboard_warns() {
        let d = def();
        let f = d
            .evaluate("air-3b", &[obs("asic", 42, Some(2), State::Failed)])
            .unwrap();
        assert_eq!(f[0].verdict, Verdict::Warn);
    }

    /// One dead device must not cost a whole machine.
    #[test]
    fn a_dead_asic_never_blocks_a_start() {
        let d = def();
        let many: Vec<_> = (0..50)
            .map(|i| obs("asic", i, Some(0), State::Failed))
            .collect();
        assert_ne!(
            worst(&d.evaluate("air-3b", &many).unwrap()),
            Some(Verdict::Block)
        );
    }

    #[test]
    fn a_healthy_machine_produces_no_findings() {
        let d = def();
        let mut all: Vec<_> = (0..4)
            .map(|i| obs("fan", i, None, State::Present))
            .collect();
        all.extend((0..3).map(|i| obs("hashboard", i, None, State::Present)));
        let f = d.evaluate("air-3b", &all).unwrap();
        assert!(f.is_empty());
        assert_eq!(worst(&f), None);
    }

    /// An unknown variant must be an error, never a quiet fallback to the
    /// default: judging a machine against the wrong contract is worse than
    /// judging it against none, because it reports a verdict nobody asked for.
    #[test]
    fn an_unknown_variant_is_an_error_not_a_default() {
        let d = def();
        let err = d.evaluate("air-9b", &[]).unwrap_err().to_string();
        assert!(err.contains("air-9b"), "{err}");
        assert!(err.contains("known:"), "{err}");
    }

    /// A component the definition does not describe is a gap in the
    /// definition. Silence there would be the fail-open this whole file exists
    /// to prevent.
    #[test]
    fn an_undeclared_component_warns_rather_than_passing() {
        let d = def();
        let f = d
            .evaluate("air-3b", &[obs("psu", 0, None, State::Failed)])
            .unwrap();
        assert_eq!(f[0].verdict, Verdict::Warn);
        assert!(f[0].why.contains("UNDECLARED"), "{}", f[0].why);
    }
}
