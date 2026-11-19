//! Rank allowances and bounty rewards paid from this week's confirmed contributions.

use std::collections::{BTreeMap, BTreeSet};

use aa_proto::cs;
use aa_proto::guild::role;
use aa_proto::sc_activity::{mail_type, ActivityEvent, MailHeader};

use super::{Terms, EXPRESS_FEE, MIN_SEND_LEVEL, RETRY_MS, WEEK_MS};
use crate::bot::{ctx::Ctx, state::dist3d};
use crate::hub::unix_ms;

const PREFIX: &str = "Guild allowance ";
const BOUNTY_PREFIX: &str = "Guild bounty ";
const LOAN_PREFIX: &str = "Guild loan ";
const COPPER_BITS: u32 = 19;
const COPPER_MASK: u64 = (1 << COPPER_BITS) - 1;
const MAX_BOUNTY_PAYMENT: u64 = 100_000;
const MAX_SENT: usize = 512;

#[derive(Debug, Default)]
pub(super) struct Support {
    week: u64,
    rank_sent: BTreeSet<u32>,
    loan_sent: BTreeSet<u32>,
    spent: BTreeMap<i64, u64>,
    bounty_through: BTreeMap<u32, u32>,
    checked: bool,
    listing_until: Option<u64>,
    retry_at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Payment {
    pub(super) week: u64,
    pub(super) recipient: u32,
    pub(super) copper: u64,
    /// Cumulative rewarded copper for the guild, present only in bounty mail.
    pub(super) through: Option<u32>,
    pub(super) loan: bool,
}

pub(super) fn payment(mail: &MailHeader, guild: u32) -> Option<Payment> {
    if mail.mail_type != mail_type::EXPRESS || mail.returned || mail.attachments > 1 {
        return None;
    }
    let (title, bounty, loan) = if let Some(title) = mail.title.strip_prefix(BOUNTY_PREFIX) {
        (title, true, false)
    } else if let Some(title) = mail.title.strip_prefix(LOAN_PREFIX) {
        (title, false, true)
    } else {
        (mail.title.strip_prefix(PREFIX)?, false, false)
    };
    let (group, rest) = title.split_once('/')?;
    let (week, recipient) = rest.split_once('/')?;
    if group.parse::<u32>().ok()? != guild {
        return None;
    }
    let extra = u64::try_from(mail.extra).ok()?;
    let recipient = recipient.parse().ok()?;
    let (copper, through, max) = if bounty {
        let copper = extra & COPPER_MASK;
        let through = u32::try_from(extra >> COPPER_BITS).ok()?;
        if u64::from(through) < copper {
            return None;
        }
        (copper, Some(through), MAX_BOUNTY_PAYMENT)
    } else {
        (extra, None, if loan { 100_000 } else { 400_000 })
    };
    (recipient != 0 && copper > 0 && copper <= max).then_some(Payment {
        week: week.parse().ok()?,
        recipient,
        copper,
        through,
        loan,
    })
}

impl Support {
    fn for_week(&mut self, week: u64) {
        if self.week != week {
            *self = Self {
                week,
                ..Self::default()
            };
        }
    }

    pub(super) fn listing(&self) -> bool {
        self.listing_until.is_some()
    }

    pub(super) fn start_listing(&mut self, week: u64, now: u64) {
        self.for_week(week);
        self.listing_until = Some(now + 8_000);
    }

    pub(super) fn observe(&mut self, event: &ActivityEvent, guild: u32, week: u64) {
        self.for_week(week);
        match event {
            ActivityEvent::MailSent { mail, .. } => self.note_sent(mail, guild),
            ActivityEvent::MailList { is_sent, mails } if *is_sent => {
                for mail in mails {
                    self.note_sent(mail, guild);
                }
            }
            ActivityEvent::MailListEnd { .. } if self.listing_until.take().is_some() => {
                self.checked = true;
            }
            _ => {}
        }
    }

    fn note_sent(&mut self, mail: &MailHeader, guild: u32) {
        if let Some(p) = payment(mail, guild).filter(|p| p.week <= self.week) {
            if p.week == self.week && self.spent.len() < MAX_SENT {
                self.spent.entry(mail.mail_id).or_insert(p.copper);
                if p.through.is_none() && !p.loan {
                    self.rank_sent.insert(p.recipient);
                }
            }
            if p.loan
                && self.loan_sent.len() < MAX_SENT
                && (p.week == self.week || mail.attachments > 0)
            {
                self.loan_sent.insert(p.recipient);
            }
            if let Some(through) = p.through {
                if self.bounty_through.len() < MAX_SENT {
                    let recorded = self.bounty_through.entry(p.recipient).or_default();
                    *recorded = (*recorded).max(through);
                }
            }
        }
    }

    fn remaining(&self, collected: u64) -> u64 {
        let spent: u64 = self
            .spent
            .values()
            .map(|copper| copper + EXPRESS_FEE as u64)
            .sum();
        collected.saturating_sub(spent)
    }
}

fn rank_weight(rank: u8) -> Option<u64> {
    match rank {
        role::APPRENTICE => Some(1),
        role::MEMBER => Some(2),
        role::OFFICER => Some(3),
        role::COMMANDER => Some(4),
        _ => None,
    }
}

#[derive(Clone, Copy)]
enum Grant {
    Rank,
    Loan,
    Bounty(u32),
}

pub(super) fn pay(ctx: &mut Ctx<'_>, guild: u32, terms: Terms) {
    let unix = unix_ms();
    let week = unix / WEEK_MS;
    let now = ctx.now_ms;
    let money = ctx.state.me.money - i64::from(terms.reserve_copper);
    if ctx.state.me.level < MIN_SEND_LEVEL || money <= EXPRESS_FEE {
        return;
    }
    let confirmed = ctx.hub.journal.read(ctx.persona.id, |j| {
        j.guild_treasury
            .get(&guild)
            .map_or(0, |a| a.current_contributions(week))
    });
    if confirmed <= EXPRESS_FEE as u64 {
        return;
    }
    let Some((&mailbox, _)) = ctx.state.doodads.iter().find(|(_, d)| {
        dist3d(ctx.pos(), d.pos) <= 3.5 && ctx.world.is_mailbox(d.template, d.phase)
    }) else {
        return;
    };
    let mem = ctx.state.guild.mem();
    let support = &mut mem.treasury.support;
    support.for_week(week);
    if support
        .listing_until
        .is_some_and(|deadline| now >= deadline)
    {
        support.listing_until = None;
        support.retry_at = now + RETRY_MS;
    }
    if now < support.retry_at
        || support.listing()
        || support.spent.len() >= MAX_SENT
        || support.bounty_through.len() >= MAX_SENT
        || support.loan_sent.len() >= MAX_SENT
    {
        return;
    }
    if !support.checked {
        ctx.out.push(cs::list_mail());
        support.start_listing(week, now);
        return;
    }
    let budget = support
        .remaining(confirmed)
        .min(money as u64)
        .saturating_sub(EXPRESS_FEE as u64);
    let candidate = mem
        .log
        .members
        .iter()
        .filter(|m| m.char_id != ctx.state.me.char_id && ctx.hub.is_bot_char(m.char_id))
        .filter_map(|m| {
            if terms.bounty_reward_copper > 0 {
                let (earned, received) = ctx.hub.bot_id_of(m.char_id).map_or((0, 0), |bot| {
                    ctx.hub.journal.read(bot, |j| {
                        j.guild_treasury
                            .get(&guild)
                            .map_or((0, 0), |a| (a.bounty_earned, a.bounty_paid))
                    })
                });
                let mailed = support
                    .bounty_through
                    .get(&m.char_id)
                    .copied()
                    .unwrap_or_default();
                let settled = received.max(mailed);
                let copper = u64::from(earned.saturating_sub(settled))
                    .min(budget)
                    .min(MAX_BOUNTY_PAYMENT);
                if copper > 0 {
                    return Some((m, copper, Grant::Bounty(settled + copper as u32)));
                }
            }
            if terms.trade_loan_copper > 0 && !support.loan_sent.contains(&m.char_id) {
                let requested = ctx.hub.bot_id_of(m.char_id).map_or(0, |bot| {
                    ctx.hub.journal.read(bot, |j| {
                        j.guild_treasury
                            .get(&guild)
                            .map_or(0, |a| a.loan.requested(unix))
                    })
                });
                if requested > 0
                    && requested <= terms.trade_loan_copper
                    && u64::from(requested) <= budget
                {
                    return Some((m, u64::from(requested), Grant::Loan));
                }
            }
            if support.rank_sent.contains(&m.char_id) {
                return None;
            }
            let copper = u64::from(terms.weekly_support_copper) * rank_weight(m.role)?;
            (copper > 0 && copper <= budget).then_some((m, copper, Grant::Rank))
        })
        .max_by_key(|(m, copper, grant)| {
            let priority = match grant {
                Grant::Rank => 0,
                Grant::Loan => 1,
                Grant::Bounty(_) => 2,
            };
            (priority, *copper, std::cmp::Reverse(m.char_id))
        });
    if let Some((member, copper, grant)) = candidate {
        let (prefix, extra) = match grant {
            Grant::Rank => (PREFIX, copper),
            Grant::Loan => (LOAN_PREFIX, copper),
            Grant::Bounty(through) => (BOUNTY_PREFIX, copper | (u64::from(through) << COPPER_BITS)),
        };
        ctx.out.push(cs::send_money_mail(
            &member.name,
            &format!("{prefix}{guild}/{week}/{}", member.char_id),
            copper as i32,
            extra as i64,
            mailbox,
        ));
        support.checked = false;
    }
    support.retry_at = now + RETRY_MS;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pending_advance_counts_against_funds_and_keeps_its_own_slot() {
        let mut support = Support {
            week: 10,
            ..Default::default()
        };
        let mail = MailHeader {
            mail_id: 41,
            mail_type: mail_type::EXPRESS,
            status: 1,
            title: "Guild loan 42/10/7".into(),
            sender: "Master".into(),
            attachments: 1,
            receiver: "Trader".into(),
            open_date: 0,
            returned: false,
            extra: 2_000,
        };
        support.note_sent(&mail, 42);
        support.note_sent(&mail, 42);
        assert_eq!(support.remaining(5_000), 2_900);
        assert!(support.loan_sent.contains(&7));
        assert!(!support.rank_sent.contains(&7));
        support.for_week(11);
        support.note_sent(&mail, 42);
        assert_eq!(support.remaining(5_000), 5_000);
        assert!(support.loan_sent.contains(&7));
    }

    #[test]
    fn reconnect_counts_a_confirmed_payment_and_its_postage_once() {
        let mail = MailHeader {
            mail_id: 3,
            mail_type: mail_type::EXPRESS,
            status: 1,
            title: "Guild allowance 42/31/7".into(),
            sender: "Master".into(),
            attachments: 0,
            receiver: "Officer".into(),
            open_date: 0,
            returned: false,
            extra: 300,
        };
        let mut support = Support {
            listing_until: Some(8_000),
            week: 31,
            ..Support::default()
        };
        support.observe(
            &ActivityEvent::MailList {
                is_sent: true,
                mails: vec![mail.clone(), mail],
            },
            42,
            31,
        );
        support.observe(
            &ActivityEvent::MailListEnd {
                total_headers: 2,
                total_bodies: 0,
            },
            42,
            31,
        );
        assert!(support.checked);
        assert!(support.rank_sent.contains(&7));
        assert_eq!(support.remaining(700), 300);
        support.for_week(32);
        assert!(!support.checked);
        assert!(support.spent.is_empty());
    }

    #[test]
    fn past_bounty_mail_reconciles_the_debt_without_charging_this_week_again() {
        let older = MailHeader {
            mail_id: 5,
            mail_type: mail_type::EXPRESS,
            status: 1,
            title: "Guild bounty 42/30/7".into(),
            sender: "Master".into(),
            attachments: 1,
            receiver: "Tracker".into(),
            open_date: 0,
            returned: false,
            extra: (500 << COPPER_BITS) | 200,
        };
        let current = MailHeader {
            mail_id: 6,
            title: "Guild bounty 42/31/7".into(),
            extra: (600 << COPPER_BITS) | 100,
            ..older.clone()
        };
        let mut support = Support::default();
        support.observe(
            &ActivityEvent::MailList {
                is_sent: true,
                mails: vec![current.clone(), older, current],
            },
            42,
            31,
        );
        assert_eq!(support.bounty_through.get(&7), Some(&600));
        assert_eq!(support.remaining(700), 500);
        assert!(!support.rank_sent.contains(&7));
    }
}
