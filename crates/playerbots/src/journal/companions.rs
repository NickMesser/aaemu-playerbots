use serde::{Deserialize, Serialize};

const MAX_COMPANIONS: usize = 32;
const SESSION_GAP_SECS: u64 = 30 * 60;

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
