//! A local key explicitly chooses its custodian or delegates that choice to a trusted own computer.

use super::*;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnchorSelection {
    pub endpoint: Option<EndpointId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authority: Option<EndpointId>,
    pub ts: i64,
}

impl TeamState {
    /// Membership and Reason proofs never grant routing authority. Every delegation starts with the local key's
    /// own signature; a fresh computer establishes it through an explicit own-machine invitation or local command.
    pub fn anchor_choice(&self, root: &EndpointId) -> Option<&Signed<AnchorSelection>> {
        let person = &self.member(root)?.person;
        let mut signer = *root;
        let mut visited = HashSet::new();
        loop {
            if !visited.insert(signer)
                || !self.is_active(&signer)
                || self.member(&signer).is_none_or(|m| &m.person != person)
            {
                return None;
            }
            let choice = self.anchors.get(&signer.to_string())?;
            if let Some(authority) = choice.body.authority {
                signer = authority;
            } else {
                return Some(choice);
            }
        }
    }

    pub fn selected_anchor(&self, root: &EndpointId) -> Option<EndpointId> {
        let target = self.anchor_choice(root)?.body.endpoint?;
        (self.is_active(&target) && self.member(&target)?.person == self.member(root)?.person).then_some(target)
    }

    pub fn select_anchor(&mut self, key: &SecretKey, endpoint: Option<EndpointId>) -> Result<()> {
        self.write_anchor_choice(key, endpoint, None)
    }

    pub fn trust_anchor(&mut self, key: &SecretKey, authority: EndpointId) -> Result<()> {
        anyhow::ensure!(key.public() != authority, "a computer cannot delegate its anchor choice to itself");
        self.write_anchor_choice(key, None, Some(authority))
    }

    fn write_anchor_choice(
        &mut self,
        key: &SecretKey,
        endpoint: Option<EndpointId>,
        authority: Option<EndpointId>,
    ) -> Result<()> {
        let me = key.public();
        let owner = self.member(&me).filter(|_| self.is_active(&me)).ok_or_else(|| anyhow!("not an active member"))?;
        if let Some(target) = endpoint.or(authority) {
            anyhow::ensure!(
                self.is_active(&target) && self.member(&target).is_some_and(|m| m.person == owner.person),
                "select one of your own active computers"
            );
        }
        let previous = self.anchors.get(&me.to_string()).map_or(0, |entry| entry.body.ts);
        let entry =
            Signed::new(AnchorSelection { endpoint, authority, ts: next_ts(previous) }, key, &self.id, "anchor");
        self.anchors.insert(me.to_string(), entry);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_explicit_signed_choices_or_delegations_authorize_a_custodian() {
        let (owner, vps, cloud, teammate, impostor) = (
            SecretKey::generate(),
            SecretKey::generate(),
            SecretKey::generate(),
            SecretKey::generate(),
            SecretKey::generate(),
        );
        let mut team = TeamState::found("test", "alex", "laptop", &owner);
        team.put_member(&owner, vps.public(), "alex", "vps", true);
        team.put_member(&vps, cloud.public(), "alex", "cloud", true);
        team.put_member(&owner, teammate.public(), "bob", "other", true);
        team.put_member(&teammate, impostor.public(), "alex", "fake", true);
        team.select_anchor(&owner, Some(vps.public())).unwrap();
        team.select_anchor(&impostor, Some(impostor.public())).unwrap();
        assert_eq!(team.selected_anchor(&owner.public()), Some(vps.public()));
        assert_eq!(team.selected_anchor(&cloud.public()), None, "a server-provided name or admission is insufficient");
        team.trust_anchor(&cloud, owner.public()).unwrap();
        assert_eq!(team.selected_anchor(&cloud.public()), Some(vps.public()));
        let mut fresh = TeamState::verified(&team).unwrap();
        assert_eq!(fresh.selected_anchor(&cloud.public()), Some(vps.public()));
        let mut old = team.clone();
        old.anchors.clear();
        fresh.merge(&old).unwrap();
        assert_eq!(fresh.selected_anchor(&owner.public()), Some(vps.public()));
        team.select_anchor(&owner, None).unwrap();
        assert_eq!(team.selected_anchor(&cloud.public()), None);
    }

    #[test]
    fn a_removed_or_reclassified_delegation_bridge_cannot_restore_an_old_choice() {
        let (owner, bridge, selector, vps) =
            (SecretKey::generate(), SecretKey::generate(), SecretKey::generate(), SecretKey::generate());
        let mut team = TeamState::found("test", "alex", "laptop", &owner);
        for (key, name) in [(&bridge, "bridge"), (&selector, "selector"), (&vps, "vps")] {
            team.put_member(&owner, key.public(), "alex", name, true);
        }
        team.select_anchor(&owner, Some(vps.public())).unwrap();
        team.select_anchor(&selector, Some(selector.public())).unwrap();
        team.trust_anchor(&bridge, selector.public()).unwrap();
        team.trust_anchor(&owner, bridge.public()).unwrap();
        assert_eq!(team.selected_anchor(&owner.public()), Some(selector.public()));
        team.put_member(&selector, bridge.public(), "alex", "bridge", false);
        assert_eq!(team.selected_anchor(&owner.public()), None);
        team.put_member(&selector, bridge.public(), "bob", "bridge", true);
        assert_eq!(team.selected_anchor(&owner.public()), None);
        team.select_anchor(&owner, Some(vps.public())).unwrap();
        assert_eq!(team.selected_anchor(&owner.public()), Some(vps.public()));
    }

    #[test]
    fn cycles_and_forged_choices_fail_closed() {
        let (owner, vps) = (SecretKey::generate(), SecretKey::generate());
        let mut team = TeamState::found("test", "alex", "laptop", &owner);
        team.put_member(&owner, vps.public(), "alex", "vps", true);
        team.trust_anchor(&owner, vps.public()).unwrap();
        team.trust_anchor(&vps, owner.public()).unwrap();
        assert_eq!(team.selected_anchor(&owner.public()), None);
        let mut other = team.clone();
        other.select_anchor(&owner, Some(vps.public())).unwrap();
        other.anchors.get_mut(&owner.public().to_string()).unwrap().body.endpoint = Some(owner.public());
        team.merge(&other).unwrap();
        assert_eq!(team.selected_anchor(&owner.public()), None);
    }
}
