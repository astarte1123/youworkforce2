//! #4 Escalation engine nativo — FASE 0 (libreria pura, nessun effetto esterno).
//!
//! Decide *chi* è reperibile a un istante t (on-call resolver) e *quando* avanzare
//! un'escalation (macchina a stati). Nessun I/O, nessuna rete, nessun DB: solo logica
//! deterministica e testabile. GoAlert resta il motore in produzione finché questa libreria
//! non supera la validazione a specchio (shadow, fasi successive del progetto).
//!
//! Limiti noti di FASE 0 (documentati, da chiudere nelle fasi successive):
//! - Fasi orarie trattate in UTC (nessun fuso/DST). Il resolver è puro: il fuso si applica
//!   a monte convertendo t e le clock-time nello stesso riferimento.

use chrono::{DateTime, Utc, Timelike, Datelike, Duration};
use uuid::Uuid;

// ----------------------------- On-call resolver -----------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RotationType { Hourly, Daily, Weekly }

/// Rotazione: a turno un membro per volta. `shift_len` in unità del tipo (es. Weekly+1 = 1 settimana).
#[derive(Clone, Debug)]
pub struct Rotation {
    pub kind: RotationType,
    pub shift_len: i64,
    pub start: DateTime<Utc>,
    pub members: Vec<Uuid>,
}
impl Rotation {
    /// Membro attivo all'istante t (None se nessun membro o t < start).
    pub fn active_at(&self, t: DateTime<Utc>) -> Option<Uuid> {
        if self.members.is_empty() || t < self.start { return self.members.first().copied().filter(|_| !self.members.is_empty() && t >= self.start); }
        let unit = match self.kind { RotationType::Hourly => 3600, RotationType::Daily => 86400, RotationType::Weekly => 604800 };
        let period = unit * self.shift_len.max(1);
        let elapsed = (t - self.start).num_seconds();
        let idx = ((elapsed / period) % self.members.len() as i64) as usize;
        self.members.get(idx).copied()
    }
}

/// Regola di copertura: finestra oraria (minuti del giorno 0..1440) nei giorni scelti.
/// `weekday_mask` bit i (0=lunedì..6=domenica). Finestra con wrap (es. 20:00->08:00) supportata.
#[derive(Clone, Copy, Debug)]
pub struct ScheduleRule { pub start_min: u32, pub end_min: u32, pub weekday_mask: u8 }
impl ScheduleRule {
    pub fn covers(&self, t: DateTime<Utc>) -> bool {
        let wd = t.weekday().num_days_from_monday() as u8; // 0=lun..6=dom
        let min = t.hour() * 60 + t.minute();
        let wrap = self.end_min <= self.start_min; // finestra che attraversa la mezzanotte
        if !wrap {
            (self.weekday_mask & (1 << wd)) != 0 && min >= self.start_min && min < self.end_min
        } else {
            // 20:00->08:00: sera del giorno wd (>= start) OPPURE mattina (< end) che "appartiene" al giorno wd-1
            if min >= self.start_min { (self.weekday_mask & (1 << wd)) != 0 }
            else if min < self.end_min { let pd = (wd + 6) % 7; (self.weekday_mask & (1 << pd)) != 0 }
            else { false }
        }
    }
}

/// Override: per una finestra, aggiunge e/o rimuove una persona dall'on-call.
#[derive(Clone, Copy, Debug)]
pub struct Override { pub start: DateTime<Utc>, pub end: DateTime<Utc>, pub add: Option<Uuid>, pub remove: Option<Uuid> }
impl Override { pub fn active_at(&self, t: DateTime<Utc>) -> bool { t >= self.start && t < self.end } }

/// Schedule: copertura 24/7 (rules vuote) o a fasce; sorgente rotation o utenti fissi; + override.
#[derive(Clone, Debug)]
pub struct Schedule {
    pub rotation: Option<Rotation>,
    pub fixed_users: Vec<Uuid>,
    pub rules: Vec<ScheduleRule>,
    pub overrides: Vec<Override>,
}
impl Schedule {
    /// Insieme on-call all'istante t (deterministico).
    pub fn on_call_at(&self, t: DateTime<Utc>) -> Vec<Uuid> {
        // 1) copertura: se ci sono regole e nessuna copre t -> nessuno (prima degli override 'add')
        let covered = self.rules.is_empty() || self.rules.iter().any(|r| r.covers(t));
        let mut set: Vec<Uuid> = Vec::new();
        if covered {
            if let Some(rot) = &self.rotation { if let Some(u) = rot.active_at(t) { set.push(u); } }
            for u in &self.fixed_users { if !set.contains(u) { set.push(*u); } }
        }
        // 2) override attivi: remove poi add (add vale anche fuori copertura)
        for ov in self.overrides.iter().filter(|o| o.active_at(t)) {
            if let Some(r) = ov.remove { set.retain(|u| *u != r); }
            if let Some(a) = ov.add { if !set.contains(&a) { set.push(a); } }
        }
        set
    }
}

// ----------------------------- Escalation state machine -----------------------------

#[derive(Clone, Debug)]
pub struct EscStep { pub delay_s: i64, pub targets: Vec<Uuid> }
#[derive(Clone, Debug)]
pub struct EscPolicy { pub steps: Vec<EscStep>, pub repeat: u32 }

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum EscStatus { Unacked, Acked, Closed }

#[derive(Clone, Debug)]
pub struct AlertState {
    pub step: usize,          // indice step corrente già notificato
    pub step_fired_at: DateTime<Utc>,
    pub repeat_count: u32,
    pub status: EscStatus,
}

#[derive(Clone, Debug, PartialEq)]
pub enum EscAction {
    /// Notifica lo step (indice) ai destinatari.
    FireStep(usize, Vec<Uuid>),
    /// Attendi: nessuna azione finché non scade il delay del prossimo step.
    Wait,
    /// Escalation esaurita (nessuno ha risposto, repeat finiti).
    Exhausted,
    /// Chiusa/presa in carico: stop.
    Stop,
}

/// Decide l'azione corrente data la policy, lo stato e l'istante. Pura e deterministica.
/// Convenzione: `state.step` = ultimo step notificato; il primo tick su un alert nuovo passa
/// `step = usize::MAX` (nessuno ancora) per far partire lo step 0.
pub fn tick(policy: &EscPolicy, state: &AlertState, now: DateTime<Utc>) -> EscAction {
    if matches!(state.status, EscStatus::Acked | EscStatus::Closed) { return EscAction::Stop; }
    if policy.steps.is_empty() { return EscAction::Exhausted; }
    // alert nuovo -> step 0 subito
    if state.step == usize::MAX {
        return EscAction::FireStep(0, policy.steps[0].targets.clone());
    }
    let cur = &policy.steps[state.step];
    if now - state.step_fired_at < Duration::seconds(cur.delay_s.max(0)) {
        return EscAction::Wait; // il delay dello step corrente non è ancora scaduto
    }
    let next = state.step + 1;
    if next < policy.steps.len() {
        return EscAction::FireStep(next, policy.steps[next].targets.clone());
    }
    // ultimo step scaduto: ripeti dallo 0 se restano repeat
    if state.repeat_count < policy.repeat {
        return EscAction::FireStep(0, policy.steps[0].targets.clone());
    }
    EscAction::Exhausted
}

#[cfg(test)]
mod tests {
    use super::*;
    fn u(n: u8) -> Uuid { Uuid::from_bytes([n;16]) }
    fn ts(s: &str) -> DateTime<Utc> { DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc) }

    #[test]
    fn rotation_weekly_handoff() {
        let r = Rotation{ kind:RotationType::Weekly, shift_len:1, start:ts("2024-01-01T00:00:00Z"), members:vec![u(1),u(2),u(3)] };
        assert_eq!(r.active_at(ts("2024-01-03T12:00:00Z")), Some(u(1))); // settimana 0
        assert_eq!(r.active_at(ts("2024-01-09T00:00:00Z")), Some(u(2))); // settimana 1
        assert_eq!(r.active_at(ts("2024-01-16T00:00:00Z")), Some(u(3))); // settimana 2
        assert_eq!(r.active_at(ts("2024-01-23T00:00:00Z")), Some(u(1))); // wrap
    }
    #[test]
    fn rotation_daily() {
        let r = Rotation{ kind:RotationType::Daily, shift_len:1, start:ts("2024-01-01T00:00:00Z"), members:vec![u(1),u(2)] };
        assert_eq!(r.active_at(ts("2024-01-01T10:00:00Z")), Some(u(1)));
        assert_eq!(r.active_at(ts("2024-01-02T10:00:00Z")), Some(u(2)));
        assert_eq!(r.active_at(ts("2024-01-03T10:00:00Z")), Some(u(1)));
    }
    #[test]
    fn schedule_daytime_window() {
        // 08:00-20:00 lun-ven
        let rule = ScheduleRule{ start_min:8*60, end_min:20*60, weekday_mask:0b0011111 };
        let s = Schedule{ rotation:None, fixed_users:vec![u(1)], rules:vec![rule], overrides:vec![] };
        assert_eq!(s.on_call_at(ts("2024-01-01T12:00:00Z")), vec![u(1)]); // lun mezzogiorno
        assert!(s.on_call_at(ts("2024-01-01T06:00:00Z")).is_empty());      // lun presto
        assert!(s.on_call_at(ts("2024-01-06T12:00:00Z")).is_empty());      // sabato
    }
    #[test]
    fn schedule_night_window_wrap() {
        // 20:00-08:00 tutti i giorni
        let rule = ScheduleRule{ start_min:20*60, end_min:8*60, weekday_mask:0b1111111 };
        let s = Schedule{ rotation:None, fixed_users:vec![u(9)], rules:vec![rule], overrides:vec![] };
        assert_eq!(s.on_call_at(ts("2024-01-01T22:00:00Z")), vec![u(9)]); // sera
        assert_eq!(s.on_call_at(ts("2024-01-02T03:00:00Z")), vec![u(9)]); // notte
        assert!(s.on_call_at(ts("2024-01-02T12:00:00Z")).is_empty());      // giorno
    }
    #[test]
    fn override_replace_and_add() {
        let s = Schedule{ rotation:None, fixed_users:vec![u(1)], rules:vec![],
            overrides:vec![ Override{ start:ts("2024-01-05T00:00:00Z"), end:ts("2024-01-06T00:00:00Z"), add:Some(u(2)), remove:Some(u(1)) } ] };
        assert_eq!(s.on_call_at(ts("2024-01-04T12:00:00Z")), vec![u(1)]);      // prima
        assert_eq!(s.on_call_at(ts("2024-01-05T12:00:00Z")), vec![u(2)]);      // sostituzione attiva
        assert_eq!(s.on_call_at(ts("2024-01-06T12:00:00Z")), vec![u(1)]);      // dopo
    }
    #[test]
    fn override_add_outside_coverage() {
        // fuori fascia nessuno è on-call, ma un override 'add' copre comunque
        let rule = ScheduleRule{ start_min:8*60, end_min:20*60, weekday_mask:0b1111111 };
        let s = Schedule{ rotation:None, fixed_users:vec![u(1)], rules:vec![rule],
            overrides:vec![ Override{ start:ts("2024-01-01T00:00:00Z"), end:ts("2024-01-02T00:00:00Z"), add:Some(u(5)), remove:None } ] };
        assert_eq!(s.on_call_at(ts("2024-01-01T03:00:00Z")), vec![u(5)]); // notte, solo override
    }
    #[test]
    fn escalation_steps_and_repeat() {
        let p = EscPolicy{ steps:vec![
            EscStep{ delay_s:300, targets:vec![u(1)] },
            EscStep{ delay_s:300, targets:vec![u(2)] },
        ], repeat:1 };
        // alert nuovo -> step 0
        let s0 = AlertState{ step:usize::MAX, step_fired_at:ts("2024-01-01T00:00:00Z"), repeat_count:0, status:EscStatus::Unacked };
        assert_eq!(tick(&p,&s0,ts("2024-01-01T00:00:00Z")), EscAction::FireStep(0, vec![u(1)]));
        // step 0 notificato, delay non scaduto -> Wait
        let s1 = AlertState{ step:0, step_fired_at:ts("2024-01-01T00:00:00Z"), repeat_count:0, status:EscStatus::Unacked };
        assert_eq!(tick(&p,&s1,ts("2024-01-01T00:02:00Z")), EscAction::Wait);
        // delay scaduto -> step 1
        assert_eq!(tick(&p,&s1,ts("2024-01-01T00:06:00Z")), EscAction::FireStep(1, vec![u(2)]));
        // ultimo step scaduto, repeat disponibile -> ricomincia da 0
        let s2 = AlertState{ step:1, step_fired_at:ts("2024-01-01T00:06:00Z"), repeat_count:0, status:EscStatus::Unacked };
        assert_eq!(tick(&p,&s2,ts("2024-01-01T00:12:00Z")), EscAction::FireStep(0, vec![u(1)]));
        // repeat esaurito -> Exhausted
        let s3 = AlertState{ step:1, step_fired_at:ts("2024-01-01T00:06:00Z"), repeat_count:1, status:EscStatus::Unacked };
        assert_eq!(tick(&p,&s3,ts("2024-01-01T00:12:00Z")), EscAction::Exhausted);
        // ack -> Stop
        let s4 = AlertState{ step:0, step_fired_at:ts("2024-01-01T00:00:00Z"), repeat_count:0, status:EscStatus::Acked };
        assert_eq!(tick(&p,&s4,ts("2024-01-01T01:00:00Z")), EscAction::Stop);
    }
}
