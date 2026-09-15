//! Versioned, canonical run manifests for deterministic swarm runs.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::domain::persona::{AgentSpec, Persona};
use crate::domain::ring::{pinned_rings, validate_ring_set, RingError, RingSpec};
use crate::domain::rng::derive_seed;
use crate::domain::schedule::ScheduleConfig;

pub const MANIFEST_SCHEMA_VERSION: u16 = 1;
pub const DECISION_VERSION: &str = "d28-v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Profile {
    Smoke,
    Full,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScenarioConfig {
    pub schedule: ScheduleConfig,
    pub lifecycle_market: String,
    pub fat_pot_market: String,
    pub near_close_market: String,
    pub suppress_market: String,
    pub pad_market: String,
    pub payout_hold_threshold_micro: i64,
    pub pad_oi_floor_micro: i64,
    pub slo_enforce: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RngRules {
    pub algorithm: String,
    pub derivation: String,
    pub streams: [String; 2],
    pub consumption: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Roster {
    pub lifecycle_voters: BTreeSet<String>,
    pub fat_pot_honest_voters: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunManifest {
    pub schema_version: u16,
    pub code_version: String,
    pub decision_version: String,
    pub profile: Profile,
    pub seed: u64,
    pub scenario: ScenarioConfig,
    pub agents: Vec<AgentSpec>,
    pub rings: Vec<RingSpec>,
    pub roster: Roster,
    pub rng: RngRules,
}

impl RunManifest {
    #[must_use]
    pub fn profile(profile: Profile, seed: u64) -> Self {
        let count = match profile {
            Profile::Smoke => 100,
            Profile::Full => 2_000,
        };
        let mut agents = (0..count)
            .map(|index| {
                // D35: the pinned release profile carries a 10% money path.
                // The smoke mix is the unchanged six-persona round robin.
                let persona =
                    if profile == Profile::Full && index % Persona::MONEY_PATH_EVERY_NTH == 0 {
                        Persona::MoneyPath
                    } else {
                        Persona::ALL[usize::try_from(index).unwrap_or(0) % Persona::ALL.len()]
                    };
                AgentSpec::synthetic(index, persona, "lifecycle")
            })
            .collect::<Vec<_>>();
        let rings = pinned_rings(&agents);
        for ring in &rings {
            for agent in agents
                .iter_mut()
                .filter(|agent| ring.members.contains(&agent.id))
            {
                agent.device_id.clone_from(&ring.shared_device_id);
                agent.forwarded_for.clone_from(&ring.forwarded_for);
                agent.target_market.clone_from(&ring.target_market);
                if ring.kind == crate::domain::ring::RingKind::WashPair
                    && ring.members.first() == Some(&agent.id)
                {
                    agent.rep_seed_micro = 400_000;
                }
            }
        }
        let ring_members = rings
            .iter()
            .flat_map(|ring| ring.members.iter().cloned())
            .collect::<BTreeSet<_>>();
        let lifecycle_voters = agents
            .iter()
            .filter(|agent| !ring_members.contains(&agent.id))
            .take(if profile == Profile::Full { 1_000 } else { 50 })
            .map(|agent| agent.id.clone())
            .collect();
        let fat_pot_honest_voters = agents
            .iter()
            .filter(|agent| !ring_members.contains(&agent.id))
            .take(15)
            .map(|agent| agent.id.clone())
            .collect();
        Self {
            schema_version: MANIFEST_SCHEMA_VERSION,
            code_version: env!("CARGO_PKG_VERSION").into(),
            decision_version: DECISION_VERSION.into(),
            profile,
            seed,
            scenario: ScenarioConfig {
                schedule: ScheduleConfig::smoke(),
                lifecycle_market: "lifecycle".into(),
                fat_pot_market: "fat-pot".into(),
                near_close_market: "near-close".into(),
                suppress_market: "void-suppress".into(),
                pad_market: "threshold-pad".into(),
                payout_hold_threshold_micro: 500_000_000,
                pad_oi_floor_micro: 50_000_000,
                slo_enforce: false,
            },
            agents,
            rings,
            roster: Roster {
                lifecycle_voters,
                fat_pot_honest_voters,
            },
            rng: RngRules {
                algorithm: "ChaCha8".into(),
                derivation: "sha256(seed_le || NUL || actor || NUL || stream)".into(),
                streams: ["schedule".into(), "decision".into()],
                consumption: "one u64 word per bounded/chance draw".into(),
            },
        }
    }

    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.schema_version != MANIFEST_SCHEMA_VERSION {
            return Err(ManifestError::SchemaVersion);
        }
        if self.decision_version != DECISION_VERSION || self.code_version.is_empty() {
            return Err(ManifestError::DecisionVersion);
        }
        self.scenario
            .schedule
            .validate()
            .map_err(|_| ManifestError::Schedule)?;
        let known = self
            .agents
            .iter()
            .map(|agent| agent.id.clone())
            .collect::<BTreeSet<_>>();
        if known.len() != self.agents.len() {
            return Err(ManifestError::DuplicateAgent);
        }
        validate_ring_set(&self.rings, &known).map_err(|error| match error {
            RingError::PinnedSet => ManifestError::PinnedRings,
            RingError::UnknownMember => ManifestError::UnknownRingMember,
            _ => ManifestError::RingContract,
        })?;
        let ring_members = self
            .rings
            .iter()
            .flat_map(|ring| ring.members.iter())
            .collect::<BTreeSet<_>>();
        if self.roster.fat_pot_honest_voters.len() > 15
            || self
                .roster
                .fat_pot_honest_voters
                .iter()
                .any(|id| ring_members.contains(id) || !known.contains(id))
            || self
                .roster
                .lifecycle_voters
                .iter()
                .any(|id| ring_members.contains(id) || !known.contains(id))
        {
            return Err(ManifestError::RosterIsolation);
        }
        Ok(())
    }

    #[must_use]
    pub fn persona_counts(&self) -> BTreeMap<Persona, usize> {
        let mut counts = BTreeMap::new();
        for agent in &self.agents {
            *counts.entry(agent.persona).or_default() += 1;
        }
        counts
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>, ManifestError> {
        self.validate()?;
        serde_json::to_vec(self).map_err(|_| ManifestError::Serialization)
    }

    pub fn from_slice(bytes: &[u8]) -> Result<Self, ManifestError> {
        let manifest: Self =
            serde_json::from_slice(bytes).map_err(|_| ManifestError::Serialization)?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn hash_hex(&self) -> Result<String, ManifestError> {
        Ok(hex(&Sha256::digest(self.canonical_bytes()?)))
    }

    #[must_use]
    pub fn stream_seed(&self, actor: &str, stream: &str) -> [u8; 32] {
        derive_seed(self.seed, actor, stream)
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ManifestError {
    #[error("unsupported manifest schema version")]
    SchemaVersion,
    #[error("unsupported or missing decision/code version")]
    DecisionVersion,
    #[error("schedule is invalid")]
    Schedule,
    #[error("agent ids must be unique")]
    DuplicateAgent,
    #[error("all six pinned rings are required")]
    PinnedRings,
    #[error("ring references an unknown agent")]
    UnknownRingMember,
    #[error("a pinned ring contract is invalid")]
    RingContract,
    #[error("fat-pot and lifecycle rosters must remain isolated")]
    RosterIsolation,
    #[error("manifest serialization failed")]
    Serialization,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ring::RingKind;

    #[test]
    fn smoke_manifest_is_valid_canonical_and_complete() {
        let manifest = RunManifest::profile(Profile::Smoke, 41);
        manifest.validate().unwrap();

        assert_eq!(manifest.agents.len(), 100);
        assert_eq!(manifest.persona_counts().len(), 6);
        assert_eq!(manifest.rings.len(), 6);
        assert_eq!(
            manifest.canonical_bytes().unwrap(),
            manifest.canonical_bytes().unwrap()
        );
        assert_eq!(manifest.hash_hex().unwrap().len(), 64);
        assert_eq!(manifest.rng.streams, ["schedule", "decision"]);
    }

    #[test]
    fn full_profile_keeps_attack_and_load_rosters_isolated() {
        let manifest = RunManifest::profile(Profile::Full, 99);
        manifest.validate().unwrap();

        assert_eq!(manifest.agents.len(), 2_000);
        assert!(manifest.roster.lifecycle_voters.len() >= 1_000);
        assert!(manifest.roster.fat_pot_honest_voters.len() <= 15);
        let sybil = manifest
            .rings
            .iter()
            .find(|ring| ring.kind == RingKind::AgedSybil)
            .unwrap();
        assert!(sybil.members.len() >= 31);
        assert!(sybil.members.iter().all(|id| {
            manifest
                .agents
                .iter()
                .find(|agent| &agent.id == id)
                .is_some_and(|agent| agent.aged)
        }));
        assert!(sybil
            .members
            .iter()
            .all(|id| !manifest.roster.lifecycle_voters.contains(id)));
        let wash = manifest
            .rings
            .iter()
            .find(|ring| ring.kind == RingKind::WashPair)
            .unwrap();
        assert_eq!(
            wash.members
                .iter()
                .filter_map(|id| manifest.agents.iter().find(|agent| &agent.id == id))
                .filter(|agent| agent.rep_seed_micro >= 400_000)
                .count(),
            1
        );
        assert!(wash.members.iter().all(|id| {
            manifest
                .agents
                .iter()
                .find(|agent| &agent.id == id)
                .is_some_and(|agent| agent.aged)
        }));
    }

    #[test]
    fn manifest_validation_rejects_each_pinned_contract_break() {
        let base = RunManifest::profile(Profile::Smoke, 7);

        let mut changed = base.clone();
        changed.schema_version += 1;
        assert_eq!(changed.validate(), Err(ManifestError::SchemaVersion));

        let mut changed = base.clone();
        changed.decision_version.clear();
        assert_eq!(changed.validate(), Err(ManifestError::DecisionVersion));

        let mut changed = base.clone();
        changed.agents[1].id = changed.agents[0].id.clone();
        assert_eq!(changed.validate(), Err(ManifestError::DuplicateAgent));

        let mut changed = base.clone();
        changed.rings.pop();
        assert_eq!(changed.validate(), Err(ManifestError::PinnedRings));

        let mut changed = base.clone();
        changed
            .roster
            .fat_pot_honest_voters
            .push("agent-099".into());
        assert_eq!(changed.validate(), Err(ManifestError::RosterIsolation));

        let mut changed = base.clone();
        changed.rings[0].members.push("missing-agent".into());
        assert_eq!(changed.validate(), Err(ManifestError::UnknownRingMember));

        let mut changed = base;
        changed.rings[1].members.pop();
        assert_eq!(changed.validate(), Err(ManifestError::RingContract));
    }

    #[test]
    fn rng_derivation_separates_actor_and_purpose() {
        let manifest = RunManifest::profile(Profile::Smoke, 123);
        let a = manifest.stream_seed("agent-001", "schedule");
        let b = manifest.stream_seed("agent-001", "decision");
        let c = manifest.stream_seed("agent-002", "schedule");
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_eq!(a, manifest.stream_seed("agent-001", "schedule"));
    }

    #[test]
    fn manifest_round_trip_revalidates_and_rejects_bad_json() {
        let manifest = RunManifest::profile(Profile::Smoke, 8);
        let bytes = manifest.canonical_bytes().unwrap();
        assert_eq!(RunManifest::from_slice(&bytes).unwrap(), manifest);
        assert_eq!(
            RunManifest::from_slice(b"not-json"),
            Err(ManifestError::Serialization)
        );
        let mut value = serde_json::to_value(&manifest).unwrap();
        value["schema_version"] = serde_json::json!(999);
        assert_eq!(
            RunManifest::from_slice(&serde_json::to_vec(&value).unwrap()),
            Err(ManifestError::SchemaVersion)
        );
    }
}
