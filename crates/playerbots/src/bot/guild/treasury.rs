//! Real guild contributions travel through the game's express mail system.

mod loan;
mod support;

use std::collections::VecDeque;

use aa_proto::cs;
use aa_proto::sc_activity::{mail_type, ActivityEvent, MailHeader};
use serde::{Deserialize, Serialize};

use super::log::GuildLog;
use super::profession;
use crate::bot::{cmd::GoalKind, ctx::Ctx, state::dist3d};
use crate::hub::unix_ms;

pub(crate) const WEEK_MS: u64 = 7 * 24 * 60 * 60 * 1_000;
pub(crate) const EXPRESS_FEE: i64 = 100;
const MIN_SEND_LEVEL: u8 = 10;
const RETRY_MS: u64 = 60_000;
const LIST_EVERY_MS: u64 = 300_000;
const PREFIX: &str = "Guild contribution ";

pub fn is_mail(event: &ActivityEvent) -> bool {
    matches!(
        event,
        ActivityEvent::MailSent { .. }
            | ActivityEvent::MailReceived { .. }
            | ActivityEvent::MailList { .. }
            | ActivityEvent::MailListEnd { .. }
            | ActivityEvent::MailBody { .. }
            | ActivityEvent::AttachmentTaken { .. }
            | ActivityEvent::MailFailed { .. }
    )
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Terms {
    pub weekly_dues_copper: u32,
    pub commission_percent: u8,
    pub reserve_copper: u32,
    /// Rank-scaled weekly allowance, paid only from confirmed contributions.
    pub weekly_support_copper: u32,
    /// Reward at the time the server confirms a guild bounty kill.
    pub bounty_reward_copper: u32,
    /// Maximum principal for a Trader member short of buying money for an assigned pack.
    pub trade_loan_copper: u32,
}

impl Default for Terms {
    fn default() -> Self {
        Self {
            weekly_dues_copper: 100,
            commission_percent: 5,
            reserve_copper: 10_000,
            weekly_support_copper: 0,
            bounty_reward_copper: 0,
            trade_loan_copper: 0,
        }
    }
}

impl Terms {
    pub fn valid(self) -> bool {
        self.weekly_dues_copper <= 100_000
            && self.commission_percent <= 25
            && self.weekly_support_copper <= 100_000
            && self.bounty_reward_copper <= 100_000
            && self.trade_loan_copper <= 100_000
    }

    pub fn support_enabled(self) -> bool {
        self.weekly_support_copper != 0
            || self.bounty_reward_copper != 0
            || self.trade_loan_copper != 0
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Account {
    pub unpaid_commission: u64,
    pub paid_week: u64,
    pub contribution_week: u64,
    pub contributions_this_week: u64,
    pub support_received: u64,
    pub bounty_earned: u32,
    pub bounty_paid: u32,
    #[serde(skip_serializing_if = "loan::Loan::unused")]
    pub loan: loan::Loan,
}

impl Account {
    fn add_contributions(&mut self, week: u64, copper: u64) {
        if self.contribution_week != week {
            self.contribution_week = week;
            self.contributions_this_week = 0;
        }
        self.contributions_this_week = self.contributions_this_week.saturating_add(copper);
    }

    pub fn current_contributions(&self, week: u64) -> u64 {
        if self.contribution_week == week {
            self.contributions_this_week
        } else {
            0
        }
    }
}

/// Charges a share of confirmed receipts earned in the relevant guild profession.
pub fn note_receipt(ctx: &Ctx<'_>, work: GoalKind, copper: u64) {
    let Some((guild, role, false)) = profession::membership(ctx) else {
        return;
    };
    let Some(terms) = ctx.cfg.guild.treasuries.get(&guild.to_string()) else {
        return;
    };
    if !role.goals().contains(&work) || copper == 0 {
        return;
    }
    let commission = copper.saturating_mul(u64::from(terms.commission_percent)) / 100;
    let repay = terms.trade_loan_copper > 0 && work == GoalKind::TradeRun;
    if commission > 0 || repay {
        ctx.hub.journal.update(ctx.persona.id, |j| {
            if let Some(account) = account_mut(j, guild) {
                account.unpaid_commission = account.unpaid_commission.saturating_add(commission);
                if repay {
                    account
                        .loan
                        .reserve_from_receipt(copper.saturating_sub(commission));
                }
            }
        });
    }
}

pub fn note_pack_budget(ctx: &Ctx<'_>) {
    if ctx.cfg.guild.trade_pack_orders.is_empty() || ctx.cfg.guild.treasuries.is_empty() {
        return;
    }
    if let Some((guild, role, false)) = profession::membership(ctx) {
        if let Some(terms) = ctx.cfg.guild.treasuries.get(&guild.to_string()).copied() {
            loan::request(ctx, guild, role, terms);
        }
    }
}

pub fn queue_copper(journal: &mut crate::journal::Journal, guild: u32, copper: u64) {
    if let Some(account) = account_mut(journal, guild) {
        account.unpaid_commission = account.unpaid_commission.saturating_add(copper);
    }
}

pub fn note_bounty(journal: &mut crate::journal::Journal, guild: u32, reward: u32) {
    if reward > 0 {
        if let Some(account) = account_mut(journal, guild) {
            account.bounty_earned = account.bounty_earned.saturating_add(reward);
        }
    }
}

fn account_mut(journal: &mut crate::journal::Journal, guild: u32) -> Option<&mut Account> {
    (journal.guild_treasury.len() < 16 || journal.guild_treasury.contains_key(&guild))
        .then(|| journal.guild_treasury.entry(guild).or_default())
}

#[derive(Debug, Clone, Copy)]
struct Incoming {
    id: i64,
    payment: Option<support::Payment>,
}

#[derive(Debug, Clone, Copy)]
enum Claim {
    Reading {
        mail: Incoming,
        deadline: u64,
    },
    Ready {
        mail: Incoming,
        copper: u64,
    },
    Taking {
        mail: Incoming,
        copper: u64,
        deadline: u64,
    },
}

#[derive(Debug, Default)]
pub struct Treasury {
    guild: u32,
    next_check: u64,
    /// Lists sent mail before sending, so reconnecting after an unacknowledged payment is safe.
    checking: Option<(u64, u64)>,
    list_ended: bool,
    sent: Option<Contribution>,
    incoming: VecDeque<Incoming>,
    done: VecDeque<i64>,
    claim: Option<Claim>,
    received: u64,
    bounty_received_through: u32,
    received_loan: Option<support::Payment>,
    support: support::Support,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Contribution {
    week: u64,
    commission: u64,
    loan_repayment: u32,
}

fn contribution(mail: &MailHeader, guild: u32) -> Option<Contribution> {
    // A pending contribution has one currency attachment. It has none once collected.
    if mail.mail_type != mail_type::EXPRESS || mail.attachments > 1 || mail.returned {
        return None;
    }
    let (group, week) = mail.title.strip_prefix(PREFIX)?.split_once('/')?;
    if group.parse::<u32>().ok()? != guild {
        return None;
    }
    let extra = u64::try_from(mail.extra).ok()?;
    // Old contribution headers used only the low 32 bits for commission.
    Some(Contribution {
        week: week.parse().ok()?,
        commission: extra & u64::from(u32::MAX),
        loan_repayment: (extra >> 32) as u32,
    })
}

impl Treasury {
    fn for_guild(&mut self, guild: u32) {
        if self.guild != guild {
            *self = Self {
                guild,
                ..Self::default()
            };
        }
    }

    fn note_sent(&mut self, mail: &MailHeader, guild: u32) {
        if let Some(payment) = contribution(mail, guild) {
            if payment.week <= unix_ms() / WEEK_MS
                && self.sent.is_none_or(|last| payment.week > last.week)
            {
                self.sent = Some(payment);
            }
        }
    }

    fn note_loan(&mut self, payment: support::Payment) {
        if payment.week <= unix_ms() / WEEK_MS
            && self
                .received_loan
                .is_none_or(|last| payment.week > last.week)
        {
            self.received_loan = Some(payment);
        }
    }

    fn note_received(
        &mut self,
        mail: &MailHeader,
        guild: &GuildLog,
        owner: bool,
        is_bot: impl Fn(u32) -> bool,
    ) {
        let payment = (!owner).then(|| support::payment(mail, guild.id)).flatten();
        let guild_mail = if owner {
            contribution(mail, guild.id).is_some()
                && guild
                    .members
                    .iter()
                    .any(|m| m.name.eq_ignore_ascii_case(&mail.sender) && is_bot(m.char_id))
        } else {
            payment.is_some_and(|p| {
                guild.member(p.recipient).is_some_and(|m| {
                    is_bot(m.char_id) && m.name.eq_ignore_ascii_case(&mail.receiver)
                })
            }) && guild
                .member(guild.owner)
                .is_some_and(|m| is_bot(m.char_id) && m.name.eq_ignore_ascii_case(&mail.sender))
        };
        if !guild_mail {
            return;
        }
        if mail.attachments == 0 {
            if let Some(payment) = payment.filter(|p| p.loan) {
                self.note_loan(payment);
            }
        }
        if mail.attachments == 1
            && !self.done.contains(&mail.mail_id)
            && !self.incoming.iter().any(|m| m.id == mail.mail_id)
            && self.incoming.len() < 32
        {
            self.incoming.push_back(Incoming {
                id: mail.mail_id,
                payment,
            });
        }
    }

    pub fn observe(
        &mut self,
        event: &ActivityEvent,
        guild: &GuildLog,
        owner: bool,
        support_enabled: bool,
        now: u64,
        is_bot: impl Fn(u32) -> bool,
    ) {
        self.for_guild(guild.id);
        if owner && support_enabled {
            self.support.observe(event, guild.id, unix_ms() / WEEK_MS);
        }
        match event {
            ActivityEvent::MailSent { mail, .. } if !owner => self.note_sent(mail, guild.id),
            ActivityEvent::MailList { is_sent, mails } if *is_sent && !owner => {
                for mail in mails {
                    self.note_sent(mail, guild.id);
                }
            }
            ActivityEvent::MailListEnd { .. } => self.list_ended = self.checking.is_some(),
            ActivityEvent::MailList {
                is_sent: false,
                mails,
            } if owner || support_enabled => {
                for mail in mails {
                    self.note_received(mail, guild, owner, &is_bot);
                }
            }
            ActivityEvent::MailReceived {
                mail,
                is_cancel: false,
                ..
            } if owner || support_enabled => {
                self.note_received(mail, guild, owner, &is_bot);
            }
            ActivityEvent::MailBody {
                is_sent: false,
                body,
                ..
            } => {
                let Some(Claim::Reading { mail, .. }) = self.claim.filter(
                    |claim| matches!(claim, Claim::Reading { mail, .. } if mail.id == body.mail_id),
                ) else {
                    return;
                };
                let amount_matches = mail
                    .payment
                    .is_none_or(|p| u64::try_from(body.money) == Ok(p.copper));
                if body.items.is_empty()
                    && body.money > 0
                    && body.billing == 0
                    && body.money2 == 0
                    && amount_matches
                {
                    self.claim = Some(Claim::Ready {
                        mail,
                        copper: body.money as u64,
                    });
                } else {
                    self.finish_claim(mail.id);
                }
            }
            ActivityEvent::AttachmentTaken {
                mail_id,
                money: true,
                ..
            } => {
                if let Some(Claim::Taking { mail, copper, .. }) = self.claim {
                    if mail.id == *mail_id {
                        if let Some(payment) = mail.payment.filter(|p| p.loan) {
                            self.note_loan(payment);
                        } else if let Some(through) =
                            mail.payment.and_then(|payment| payment.through)
                        {
                            self.bounty_received_through =
                                self.bounty_received_through.max(through);
                        } else {
                            self.received = self.received.saturating_add(copper);
                        }
                        self.finish_claim(mail.id);
                    }
                }
            }
            ActivityEvent::MailFailed { .. } if !owner => {
                self.next_check = self.next_check.max(now + RETRY_MS);
            }
            _ => {}
        }
    }

    fn finish_claim(&mut self, id: i64) {
        self.claim = None;
        self.incoming.retain(|mail| mail.id != id);
        self.done.push_back(id);
        if self.done.len() > 128 {
            self.done.pop_front();
        }
    }
}

pub fn duty(ctx: &mut Ctx<'_>) {
    if ctx.cfg.guild.treasuries.is_empty() || ctx.state.place.in_instance() {
        return;
    }
    let Some((guild, _, owner)) = profession::membership(ctx) else {
        return;
    };
    let Some(terms) = ctx.cfg.guild.treasuries.get(&guild.to_string()).copied() else {
        return;
    };
    ctx.state.guild.mem().treasury.for_guild(guild);
    let support_enabled = terms.support_enabled();
    if owner {
        if !collect(ctx, guild, true, support_enabled) && support_enabled {
            support::pay(ctx, guild, terms);
        }
    } else {
        contribute(ctx, guild, terms);
        if support_enabled {
            collect(ctx, guild, false, true);
        }
    }
}

/// Returns whether an attachment is being collected.
fn collect(ctx: &mut Ctx<'_>, guild: u32, owner: bool, support_enabled: bool) -> bool {
    let treasury = &mut ctx.state.guild.mem().treasury;
    let now = ctx.now_ms;
    let received = std::mem::take(&mut treasury.received);
    let bounty_through = std::mem::take(&mut treasury.bounty_received_through);
    let loan = treasury.received_loan.take();
    let mut listing = false;
    if let Some(Claim::Ready { mail, copper }) = treasury.claim {
        ctx.out.push(cs::take_attachment_sequentially(mail.id));
        treasury.claim = Some(Claim::Taking {
            mail,
            copper,
            deadline: now + 8_000,
        });
    } else if treasury.claim.is_none() {
        if let Some(mail) = treasury.incoming.front().copied() {
            ctx.out.push(cs::read_mail(false, mail.id));
            treasury.claim = Some(Claim::Reading {
                mail,
                deadline: now + 8_000,
            });
        } else if now >= treasury.next_check
            && treasury.checking.is_none()
            && !treasury.support.listing()
        {
            ctx.out.push(cs::list_mail());
            if owner && support_enabled {
                treasury.support.start_listing(unix_ms() / WEEK_MS, now);
            }
            treasury.next_check = now + LIST_EVERY_MS;
            listing = true;
        }
    } else if matches!(treasury.claim,
        Some(Claim::Reading { deadline, .. } | Claim::Taking { deadline, .. }) if now >= deadline)
    {
        treasury.claim = None;
        treasury.incoming.pop_front();
    }
    let collecting = treasury.claim.is_some();
    if received > 0 {
        ctx.hub.journal.update(ctx.persona.id, |j| {
            if owner {
                j.guild_copper_received = j.guild_copper_received.saturating_add(received);
            }
            if let Some(a) = account_mut(j, guild) {
                if owner {
                    a.add_contributions(unix_ms() / WEEK_MS, received);
                } else {
                    a.support_received = a.support_received.saturating_add(received);
                }
            }
        });
        let purpose = if owner {
            "contributions"
        } else {
            "rank allowance"
        };
        ctx.log
            .say(format!("guild: received {received} copper in {purpose}"));
    }
    if bounty_through > 0 {
        let paid = ctx.hub.journal.update(ctx.persona.id, |j| {
            account_mut(j, guild).map_or(0, |a| {
                let before = a.bounty_paid;
                a.bounty_paid = before.max(bounty_through.min(a.bounty_earned));
                a.bounty_paid - before
            })
        });
        if paid > 0 {
            ctx.log
                .say(format!("guild: received {paid} copper in bounty rewards"));
        }
    }
    if let Some(payment) = loan {
        let credited = ctx.hub.journal.update(ctx.persona.id, |j| {
            account_mut(j, guild)
                .is_some_and(|a| a.loan.credit(payment.week, payment.copper as u32))
        });
        if credited {
            ctx.log.say(format!(
                "guild: received {} copper for trade pack materials",
                payment.copper
            ));
        }
    }
    collecting || listing
}

fn contribute(ctx: &mut Ctx<'_>, guild: u32, terms: Terms) {
    let week = unix_ms() / WEEK_MS;
    let now = ctx.now_ms;
    if let Some(payment) = ctx.state.guild.mem().treasury.sent.take() {
        ctx.hub.journal.update(ctx.persona.id, |j| {
            if j.guild_treasury.len() < 16 || j.guild_treasury.contains_key(&guild) {
                let account = j.guild_treasury.entry(guild).or_default();
                if payment.week > account.paid_week {
                    account.paid_week = payment.week;
                    account.unpaid_commission =
                        account.unpaid_commission.saturating_sub(payment.commission);
                    account.loan.paid(payment.loan_repayment);
                }
            }
        });
    }
    let (paid, commission, loan) = ctx.hub.journal.read(ctx.persona.id, |j| {
        j.guild_treasury.get(&guild).map_or((0, 0, 0), |a| {
            (a.paid_week, a.unpaid_commission, a.loan.repayment)
        })
    });
    let dues = u64::from(terms.weekly_dues_copper);
    let spendable = ctx.state.me.money - i64::from(terms.reserve_copper) - EXPRESS_FEE;
    if paid >= week || ctx.state.me.level < MIN_SEND_LEVEL || spendable <= 0 {
        return;
    }
    let limit = (spendable as u64).saturating_sub(dues);
    let repayment = u64::from(loan).min(limit);
    let commission = commission.min(limit - repayment);
    let amount = dues + commission + repayment;
    if amount == 0 || amount > spendable as u64 || amount > (i32::MAX - 100) as u64 {
        return;
    }
    let Some((&mailbox, _)) = ctx.state.doodads.iter().find(|(_, d)| {
        dist3d(ctx.pos(), d.pos) <= 3.5 && ctx.world.is_mailbox(d.template, d.phase)
    }) else {
        return;
    };
    let log = &ctx
        .state
        .guild
        .get()
        .expect("membership already checked")
        .log;
    let Some(master) = log
        .member(log.owner)
        .filter(|m| ctx.hub.is_bot_char(m.char_id))
    else {
        return;
    };
    let owner = master.name.clone();
    let treasury = &mut ctx.state.guild.mem().treasury;
    if let Some((expected_week, deadline)) = treasury.checking {
        if treasury.list_ended && expected_week == week {
            treasury.checking = None;
            treasury.next_check = now + RETRY_MS;
            ctx.out.push(cs::send_money_mail(
                &owner,
                &format!("{PREFIX}{guild}/{week}"),
                amount as i32,
                (commission | (repayment << 32)) as i64,
                mailbox,
            ));
        } else if now >= deadline {
            treasury.checking = None;
        }
    } else if now >= treasury.next_check {
        treasury.list_ended = false;
        treasury.checking = Some((week, now + 8_000));
        treasury.next_check = now + RETRY_MS;
        ctx.out.push(cs::list_mail());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sent_contributions_are_recognized_on_a_new_connection() {
        let week = unix_ms() / WEEK_MS;
        let mut mail = MailHeader {
            mail_id: 1,
            mail_type: mail_type::EXPRESS,
            status: 1,
            title: format!("{PREFIX}42/{week}").into(),
            sender: "Member".into(),
            attachments: 1,
            receiver: "Master".into(),
            open_date: 0,
            returned: false,
            extra: 98,
        };
        let mut treasury = Treasury::default();
        treasury.observe(
            &ActivityEvent::MailList {
                is_sent: true,
                mails: vec![mail.clone()],
            },
            &GuildLog {
                id: 42,
                ..GuildLog::default()
            },
            false,
            false,
            2000,
            |_| true,
        );
        assert_eq!(
            treasury.sent,
            Some(Contribution {
                week,
                commission: 98,
                loan_repayment: 0
            })
        );
        assert_eq!(contribution(&mail, 43), None);
        mail.returned = true;
        assert_eq!(contribution(&mail, 42), None);
    }
}
