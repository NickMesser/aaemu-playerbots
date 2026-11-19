use serde::{Deserialize, Serialize};

use super::JournalBook;

const MAX_COMPANIONS: usize = 32;
const SESSION_GAP_SECS: u64 = 30 * 60;
const BLOODLUST_MEMORY_GAP_SECS: u64 = 15 * 60;
const THEFT_MEMORY_GAP_SECS: u64 = 15 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mediation {
    pub requester: u32,
    pub recipient: u32,
    pub expires_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediationReply {
    NoOffer,
    Declined,
    Accepted,
}

impl JournalBook {
    pub fn finish_mediation(
        &self,
        origin: (u32, u32, &str),
        recipient: (u32, u32, &str),
        requester: u32,
        now: u64,
        min_trust: i8,
    ) -> MediationReply {
        let (origin_bot, origin_char, origin_name) = origin;
        let (recipient_bot, recipient_char, recipient_name) = recipient;
        if origin_bot == recipient_bot || origin_char == recipient_char {
            return MediationReply::NoOffer;
        }
        let mut book = self.inner.lock();
        let Some(source) = book.get(&origin_bot) else {
            return MediationReply::NoOffer;
        };
        if !source.mediation.is_some_and(|quest| {
            quest.requester == requester
                && quest.recipient == recipient_char
                && quest.expires_ms > now
        }) {
            return MediationReply::NoOffer;
        }
        let origin_trust = source.relationships.trust(recipient_char);
        let target = book.entry(recipient_bot).or_default();
        let target_trust = target.relationships.trust(origin_char);
        let accepted = origin_trust < 0 && target_trust >= min_trust;
        if accepted {
            target.relationships.ease_grudge(origin_char);
            target.notes.push_clean(
                now / 1_000,
                &format!("I talked through my disagreement with {origin_name} with a mediator."),
            );
        }
        let source = book.entry(origin_bot).or_default();
        source.mediation = None;
        source.last_mediation_ms = Some(now);
        if accepted {
            source.relationships.ease_grudge(recipient_char);
            source.notes.push_clean(
                now / 1_000,
                &format!(
                    "A mediator helped me talk through my disagreement with {recipient_name}."
                ),
            );
        }
        drop(book);
        self.dirty.store(true, std::sync::atomic::Ordering::Release);
        self.wake.notify_one();
        if accepted {
            MediationReply::Accepted
        } else {
            MediationReply::Declined
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Preference {
    Questing,
    Dungeons,
    Grinding,
    Exploring,
    Farming,
    Trading,
}

impl Preference {
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text.trim().to_ascii_lowercase().as_str() {
            "questing" => Self::Questing,
            "dungeons" => Self::Dungeons,
            "grinding" => Self::Grinding,
            "exploring" => Self::Exploring,
            "farming" => Self::Farming,
            "trading" => Self::Trading,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Questing => "questing",
            Self::Dungeons => "dungeons",
            Self::Grinding => "grinding",
            Self::Exploring => "exploring",
            Self::Farming => "farming",
            Self::Trading => "trading",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Relationship {
    pub character: u32,
    pub shared_parties: u16,
    pub trust: i8,
    pub preference: Option<Preference>,
    last_party: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_bloodlust: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_theft: Option<u64>,
}

impl Relationship {
    pub fn joined_party(&mut self, now: u64) {
        if self
            .last_party
            .is_none_or(|at| now.saturating_sub(at) >= SESSION_GAP_SECS)
        {
            self.last_party = Some(now);
            self.shared_parties = self.shared_parties.saturating_add(1);
            self.change_trust(5);
        }
    }

    pub fn change_trust(&mut self, delta: i8) {
        self.trust = self.trust.saturating_add(delta).clamp(-100, 100);
    }

    pub fn witnessed_bloodlust(&mut self, now: u64, outlaw: f32) -> bool {
        if self
            .last_bloodlust
            .is_some_and(|at| now.saturating_sub(at) < BLOODLUST_MEMORY_GAP_SECS)
        {
            return false;
        }
        self.last_bloodlust = Some(now);
        let loss = ((1.0 - outlaw.clamp(0.0, 1.0)) * 15.0).round() as i8;
        self.change_trust(-loss);
        true
    }

    pub fn remembers_bloodlust(&self) -> bool {
        self.last_bloodlust.is_some()
    }

    pub fn witnessed_theft(&mut self, at: u64, outlaw: f32) -> bool {
        if self
            .last_theft
            .is_some_and(|last| at.saturating_sub(last) < THEFT_MEMORY_GAP_SECS)
        {
            return false;
        }
        self.last_theft = Some(at);
        let loss = ((1.0 - outlaw.clamp(0.0, 1.0)) * 20.0).round() as i8;
        self.change_trust(-loss);
        true
    }

    pub fn remembers_theft(&self) -> bool {
        self.last_theft.is_some()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Relationships {
    pub entries: Vec<Relationship>,
}

impl Relationships {
    pub fn get(&self, character: u32) -> Option<&Relationship> {
        self.entries.iter().find(|r| r.character == character)
    }

    fn trust(&self, character: u32) -> i8 {
        self.get(character).map_or(0, |r| r.trust)
    }

    fn ease_grudge(&mut self, character: u32) {
        let trust = self.trust(character);
        if trust < 0 {
            self.update(character, |r| r.change_trust(8.min(trust.saturating_neg())));
        }
    }

    pub fn update(&mut self, character: u32, apply: impl FnOnce(&mut Relationship)) {
        if character == 0 {
            return;
        }
        let mut record = self
            .entries
            .iter()
            .position(|r| r.character == character)
            .map(|i| self.entries.remove(i))
            .unwrap_or(Relationship {
                character,
                ..Relationship::default()
            });
        apply(&mut record);
        self.entries.push(record);
        self.entries
            .drain(..self.entries.len().saturating_sub(MAX_COMPANIONS));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mediation_requires_the_appointed_player_and_consumes_the_offer() {
        let book = JournalBook::memory();
        book.update(1, |j| {
            j.relationships.update(20, |r| r.change_trust(-17));
            j.mediation = Some(Mediation {
                requester: 30,
                recipient: 20,
                expires_ms: 2_000,
            });
        });
        book.update(2, |j| j.relationships.update(10, |r| r.change_trust(-5)));
        let finish = |requester, now, minimum| {
            book.finish_mediation(
                (1, 10, "Vonurn"),
                (2, 20, "Farsignal"),
                requester,
                now,
                minimum,
            )
        };
        assert_eq!(finish(31, 1_000, -30), MediationReply::NoOffer);
        assert_eq!(finish(30, 2_000, -30), MediationReply::NoOffer);
        assert_eq!(finish(30, 1_001, -30), MediationReply::Accepted);
        assert_eq!(finish(30, 1_002, -30), MediationReply::NoOffer);
        book.read(1, |j| {
            assert_eq!(j.relationships.trust(20), -9);
            assert_eq!(j.last_mediation_ms, Some(1_001));
            assert_eq!(j.notes.lines.len(), 1);
        });
        book.read(2, |j| {
            assert_eq!(j.relationships.trust(10), 0);
            assert_eq!(j.notes.lines.len(), 1);
        });

        book.update(1, |j| {
            j.mediation = Some(Mediation {
                requester: 30,
                recipient: 20,
                expires_ms: 3_000,
            });
        });
        book.update(2, |j| j.relationships.update(10, |r| r.change_trust(-60)));
        assert_eq!(finish(30, 2_001, -30), MediationReply::Declined);
        assert_eq!(book.read(1, |j| j.relationships.trust(20)), -9);
        assert_eq!(book.read(2, |j| j.relationships.trust(10)), -60);
    }

    #[test]
    fn witnessed_bloodlust_respects_personality_and_survives_reconnect() {
        let mut lawful = Relationship::default();
        assert!(lawful.witnessed_bloodlust(100, 0.0));
        assert_eq!(lawful.trust, -15);

        let json = serde_json::to_string(&lawful).unwrap();
        let mut lawful: Relationship = serde_json::from_str(&json).unwrap();
        assert!(!lawful.witnessed_bloodlust(101, 0.0));
        assert_eq!(lawful.trust, -15);
        assert!(lawful.witnessed_bloodlust(100 + BLOODLUST_MEMORY_GAP_SECS, 1.0));
        assert_eq!(lawful.trust, -15);
    }

    #[test]
    fn theft_memory_survives_reconnect_and_groups_nearby_evidence() {
        let mut relationship = Relationship::default();
        assert!(relationship.witnessed_theft(100, 0.1));
        assert_eq!(relationship.trust, -18);

        let saved = serde_json::to_string(&relationship).unwrap();
        let mut reopened: Relationship = serde_json::from_str(&saved).unwrap();
        assert!(reopened.remembers_theft());
        assert!(!reopened.witnessed_theft(100, 0.1));
        assert!(!reopened.witnessed_theft(101, 0.1));
        assert!(reopened.witnessed_theft(100 + THEFT_MEMORY_GAP_SECS, 0.9));
        assert_eq!(reopened.trust, -20);
    }

    #[test]
    fn meetings_and_preferences_survive_serialization_without_inflating_trust() {
        let mut book = Relationships::default();
        book.update(7, |r| {
            r.joined_party(10);
            r.joined_party(11);
            r.preference = Preference::parse("Dungeons");
        });
        let data = serde_json::to_string(&book).unwrap();
        let mut book: Relationships = serde_json::from_str(&data).unwrap();
        let r = book.get(7).unwrap();
        assert_eq!(
            (r.shared_parties, r.trust, r.preference),
            (1, 5, Some(Preference::Dungeons))
        );
        book.update(7, |r| r.change_trust(-30));
        assert_eq!(book.get(7).unwrap().trust, -25);
        for id in 10..50 {
            book.update(id, |r| r.joined_party(1));
        }
        assert_eq!(book.entries.len(), MAX_COMPANIONS);
        assert!(book.get(7).is_none());
    }
}
