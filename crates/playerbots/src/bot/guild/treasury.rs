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
const REPAYMENT_PREFIX: &str = "Guild repayment ";

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
    /// Let eligible members buy gear with their unspent rank allowance.
    pub fund_gear: bool,
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
            fund_gear: false,
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
            && (!self.fund_gear || self.weekly_support_copper > 0)
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
    pub support_supplies_spent: u64,
    pub support_gear_spent: u64,
    pub support_training_spent: u64,
    pub support_land_spent: u64,
    pub bounty_earned: u32,
    pub bounty_paid: u32,
    #[serde(skip_serializing_if = "loan::Loan::unused")]
    pub loan: loan::Loan,
}

impl Account {
    pub fn allowance_balance(&self) -> u64 {
        self.support_received
            .saturating_sub(self.support_supplies_spent)
            .saturating_sub(self.support_gear_spent)
            .saturating_sub(self.support_training_spent)
            .saturating_sub(self.support_land_spent)
    }

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

pub fn allowance_budget(ctx: &Ctx<'_>, guild: u32) -> u64 {
    let Some(terms) = ctx.cfg.guild.treasuries.get(&guild.to_string()) else {
        return 0;
    };
    let available_cash = ctx
        .state
        .me
        .money
        .saturating_sub(i64::from(terms.reserve_copper))
        .max(0) as u64;
    ctx.hub
        .journal
        .read(ctx.persona.id, |journal| {
            journal
                .guild_treasury
                .get(&guild)
                .map_or(0, Account::allowance_balance)
        })
        .min(available_cash)
}

pub fn gear_allowance(ctx: &Ctx<'_>) -> Option<(u32, u32)> {
    if ctx.cfg.guild.treasuries.is_empty() {
        return None;
    }
    let (guild, _, false) = profession::membership(ctx)? else {
        return None;
    };
    if !ctx.cfg.guild.treasuries.get(&guild.to_string())?.fund_gear {
        return None;
    }
    let budget = allowance_budget(ctx, guild).min(u64::from(u32::MAX)) as u32;
    (budget > 0).then_some((guild, budget))
}

pub fn record_gear_purchase(ctx: &Ctx<'_>, guild: u32, price: u32) {
    if profession::membership(ctx).is_none_or(|(id, _, owner)| id != guild || owner) {
        return;
    }
    let spent = ctx.hub.journal.update(ctx.persona.id, |journal| {
        let Some(account) = journal.guild_treasury.get_mut(&guild) else {
            return false;
        };
        let price = u64::from(price);
        if price == 0 || price > account.allowance_balance() {
            return false;
        }
        account.support_gear_spent += price;
        true
    });
    if spent {
        ctx.log.say(format!(
            "guild gear: spent {price} copper from rank allowances"
        ));
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
    let escort = role == profession::Profession::Adventurer && work == GoalKind::Companion;
    if !(role.goals().contains(&work) || escort) || copper == 0 {
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
    sent_repayment: Option<Contribution>,
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
    repayment_only: bool,
}

fn contribution(mail: &MailHeader, guild: u32) -> Option<Contribution> {
    // A pending contribution has one currency attachment. It has none once collected.
    if mail.mail_type != mail_type::EXPRESS || mail.attachments > 1 || mail.returned {
        return None;
    }
    let (title, repayment_only) = if let Some(title) = mail.title.strip_prefix(REPAYMENT_PREFIX) {
        (title, true)
    } else {
        (mail.title.strip_prefix(PREFIX)?, false)
    };
    let (group, week) = title.split_once('/')?;
    if group.parse::<u32>().ok()? != guild {
        return None;
    }
    let extra = u64::try_from(mail.extra).ok()?;
    // Old contribution headers used only the low 32 bits for commission.
    Some(Contribution {
        week: week.parse().ok()?,
        commission: extra & u64::from(u32::MAX),
        loan_repayment: (extra >> 32) as u32,
        repayment_only,
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
            let sent = if payment.repayment_only {
                &mut self.sent_repayment
            } else {
                &mut self.sent
            };
            if payment.week <= unix_ms() / WEEK_MS
                && sent.is_none_or(|last| payment.week > last.week)
            {
                *sent = Some(payment);
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
    let treasury = &mut ctx.state.guild.mem().treasury;
    let sent = treasury.sent.take();
    let sent_repayment = treasury.sent_repayment.take();
    if sent.is_some() || sent_repayment.is_some() {
        ctx.hub.journal.update(ctx.persona.id, |j| {
            if let Some(account) = account_mut(j, guild) {
                if let Some(payment) = sent.filter(|p| p.week > account.paid_week) {
                    account.paid_week = payment.week;
                    account.unpaid_commission =
                        account.unpaid_commission.saturating_sub(payment.commission);
                    account.loan.paid(payment.week, payment.loan_repayment);
                }
                if let Some(payment) = sent_repayment {
                    account.loan.paid(payment.week, payment.loan_repayment);
                }
            }
        });
    }
    let (paid, commission, loan, loan_week) = ctx.hub.journal.read(ctx.persona.id, |j| {
        j.guild_treasury.get(&guild).map_or((0, 0, 0, 0), |a| {
            (
                a.paid_week,
                a.unpaid_commission,
                a.loan.repayment,
                a.loan.paid_week,
            )
        })
    });
    let repayment_only = paid >= week;
    let (dues, commission) = if repayment_only {
        (0, 0)
    } else {
        (u64::from(terms.weekly_dues_copper), commission)
    };
    let spendable = ctx.state.me.money - i64::from(terms.reserve_copper) - EXPRESS_FEE;
    if ctx.state.me.level < MIN_SEND_LEVEL || spendable <= 0 {
        return;
    }
    let limit = (spendable as u64).saturating_sub(dues);
    let repayment = if loan_week < week {
        u64::from(loan).min(limit)
    } else {
        0
    };
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
            let prefix = if repayment_only {
                REPAYMENT_PREFIX
            } else {
                PREFIX
            };
            ctx.out.push(cs::send_money_mail(
                &owner,
                &format!("{prefix}{guild}/{week}"),
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
    fn training_land_supplies_and_gear_share_one_rank_allowance() {
        let account = Account {
            support_received: 100,
            support_supplies_spent: 20,
            support_gear_spent: 15,
            support_training_spent: 40,
            support_land_spent: 5,
            ..Account::default()
        };
        assert_eq!(account.allowance_balance(), 20);
    }

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
                loan_repayment: 0,
                repayment_only: false,
            })
        );
        assert_eq!(contribution(&mail, 43), None);
        mail.returned = true;
        assert_eq!(contribution(&mail, 42), None);
        mail.returned = false;
        mail.title = format!("{REPAYMENT_PREFIX}42/{week}").into();
        mail.extra = 70i64 << 32;
        treasury.note_sent(&mail, 42);
        assert_eq!(
            treasury.sent_repayment,
            Some(Contribution {
                week,
                commission: 0,
                loan_repayment: 70,
                repayment_only: true,
            })
        );
    }
}
