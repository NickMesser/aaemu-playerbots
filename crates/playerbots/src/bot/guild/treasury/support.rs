//! Rank allowances paid from this week's confirmed contributions.

use std::collections::BTreeMap;

use aa_proto::cs;
use aa_proto::guild::role;
use aa_proto::sc_activity::{mail_type, ActivityEvent, MailHeader};

use super::{Terms, EXPRESS_FEE, MIN_SEND_LEVEL, RETRY_MS, WEEK_MS};
use crate::bot::{ctx::Ctx, state::dist3d};
use crate::hub::unix_ms;

const PREFIX: &str = "Guild allowance ";

#[derive(Debug, Default)]
pub(super) struct Support {
    week: u64,
    sent: BTreeMap<u32, u64>,
    checked: bool,
    listing_until: Option<u64>,
    retry_at: u64,
}

pub(super) fn payment(mail: &MailHeader, guild: u32) -> Option<(u64, u32, u64)> {
    if mail.mail_type != mail_type::EXPRESS || mail.returned || mail.attachments > 1 {
        return None;
    }
    let (group, rest) = mail.title.strip_prefix(PREFIX)?.split_once('/')?;
    let (week, recipient) = rest.split_once('/')?;
    if group.parse::<u32>().ok()? != guild {
        return None;
    }
    let copper = u64::try_from(mail.extra).ok()?;
    let recipient = recipient.parse().ok()?;
    (recipient != 0 && copper > 0 && copper <= 400_000).then_some((
        week.parse().ok()?,
        recipient,
        copper,
    ))
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
        if let Some((week, recipient, copper)) = payment(mail, guild) {
            if week == self.week && self.sent.len() < super::super::log::MAX_MEMBERS {
                self.sent.entry(recipient).or_insert(copper);
            }
        }
    }

    fn remaining(&self, collected: u64) -> u64 {
        let spent: u64 = self
            .sent
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

pub(super) fn pay(ctx: &mut Ctx<'_>, guild: u32, terms: Terms) {
    let week = unix_ms() / WEEK_MS;
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
        || support.sent.len() >= super::super::log::MAX_MEMBERS
    {
        return;
    }
    if !support.checked {
        ctx.out.push(cs::list_mail());
        support.start_listing(week, now);
        return;
    }
    let budget = support.remaining(confirmed).min(money as u64);
    let candidate = mem
        .log
        .members
        .iter()
        .filter(|m| {
            m.char_id != ctx.state.me.char_id
                && ctx.hub.is_bot_char(m.char_id)
                && !support.sent.contains_key(&m.char_id)
        })
        .filter_map(|m| {
            let copper = u64::from(terms.weekly_support_copper) * rank_weight(m.role)?;
            (copper + EXPRESS_FEE as u64 <= budget).then_some((m, copper))
        })
        .max_by_key(|(m, copper)| (*copper, std::cmp::Reverse(m.char_id)));
    if let Some((member, copper)) = candidate {
        ctx.out.push(cs::send_money_mail(
            &member.name,
            &format!("{PREFIX}{guild}/{week}/{}", member.char_id),
            copper as i32,
            copper as i64,
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
        assert!(support.sent.contains_key(&7));
        assert_eq!(support.remaining(700), 300);
        support.for_week(32);
        assert!(!support.checked);
        assert!(support.sent.is_empty());
    }
}
