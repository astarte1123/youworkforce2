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
pub enum RotationType { Hourly, Daily, Weekly, Monthly }

/// Somma `months` mesi a `dt` preservando ora:min:sec e clampando il giorno all'ultimo valido.
fn add_months(dt: DateTime<Utc>, months: i64) -> DateTime<Utc> {
    use chrono::TimeZone;
    let m0 = dt.year() as i64 * 12 + (dt.month() as i64 - 1) + months;
    let y = m0.div_euclid(12) as i32;
    let mo = (m0.rem_euclid(12) + 1) as u32;
    // ultimo giorno del mese target
    let last = {
        let (ny, nm) = if mo == 12 { (y + 1, 1) } else { (y, mo + 1) };
        let first_next = Utc.with_ymd_and_hms(ny, nm, 1, 0, 0, 0).unwrap();
        (first_next - Duration::days(1)).day()
    };
    let day = dt.day().min(last);
    Utc.with_ymd_and_hms(y, mo, day, dt.hour(), dt.minute(), dt.second()).single()
        .unwrap_or(dt)
}

/// Rotazione: a turno un membro per volta. `shift_len` in unità del tipo (es. Weekly+1 = 1 settimana).
#[derive(Clone, Debug)]
pub struct Rotation {
    pub kind: RotationType,
    pub shift_len: i64,
    pub start: DateTime<Utc>,
    pub members: Vec<Uuid>,
}
impl Rotation {
    fn period_secs(&self) -> i64 {
        // Per Monthly è solo un fallback approssimato (usato dalla formula PURA active_at);
        // i boundary reali usano il calendario in end_time/start_time.
        let unit = match self.kind { RotationType::Hourly => 3600, RotationType::Daily => 86400, RotationType::Weekly => 604800, RotationType::Monthly => 2_592_000 };
        unit * self.shift_len.max(1)
    }
    /// Numero di passi-mese (k) tale che `start + k*shift_len mesi` è il boundary <= t più recente.
    fn monthly_k(&self, t: DateTime<Utc>) -> i64 {
        let step = self.shift_len.max(1);
        let approx = ((t.year() as i64 * 12 + t.month() as i64) - (self.start.year() as i64 * 12 + self.start.month() as i64)) / step;
        let mut k = approx.max(0);
        while k > 0 && add_months(self.start, k * step) > t { k -= 1; }
        while add_months(self.start, (k + 1) * step) <= t { k += 1; }
        k
    }

    /// Membro attivo all'istante t secondo la formula PURA `shift_number(t) % N`.
    ///
    /// ATTENZIONE: questa NON è la semantica di GoAlert. GoAlert tratta l'indice come
    /// *stato mutabile* seminato a 0 alla CREAZIONE della rotation e avanzato di 1 a ogni
    /// handoff (vedi [`RotationState`]). Lo `start` definisce solo la griglia dei boundary,
    /// non l'indice corrente. Resta qui solo come fallback deterministico senza stato e per
    /// i test della griglia; per la parità GoAlert usare [`RotationState::advanced_to`].
    pub fn active_at(&self, t: DateTime<Utc>) -> Option<Uuid> {
        if self.members.is_empty() || t < self.start { return self.members.first().copied().filter(|_| !self.members.is_empty() && t >= self.start); }
        let period = self.period_secs();
        let elapsed = (t - self.start).num_seconds();
        let idx = ((elapsed / period) % self.members.len() as i64) as usize;
        self.members.get(idx).copied()
    }

    /// Prossimo handoff STRETTAMENTE dopo `t`, allineato alla griglia (`start + k*period`).
    /// Replica `Rotation.EndTime` di GoAlert (UTC: clock == durata reale). Se `t` cade esatto
    /// su un boundary, ritorna il successivo (EndTime è sempre > t).
    pub fn end_time(&self, t: DateTime<Utc>) -> DateTime<Utc> {
        if self.kind == RotationType::Monthly {
            return add_months(self.start, (self.monthly_k(t) + 1) * self.shift_len.max(1));
        }
        let period = self.period_secs();
        let diff = (t - self.start).num_seconds();
        let mut rem = diff % period;
        if rem < 0 { rem += period; }
        t + Duration::seconds(period - rem)
    }

    /// Boundary più recente <= t (`Rotation.StartTime` di GoAlert).
    pub fn start_time(&self, t: DateTime<Utc>) -> DateTime<Utc> {
        if self.kind == RotationType::Monthly {
            return add_months(self.start, self.monthly_k(t) * self.shift_len.max(1));
        }
        let period = self.period_secs();
        let diff = (t - self.start).num_seconds();
        let mut rem = diff % period;
        if rem < 0 { rem += period; }
        t - Duration::seconds(rem)
    }
}

/// Stato persistente di una rotation, come GoAlert (`rotation_state`): `position` e `shift_start`.
/// Alla creazione: `position = 0`, `shift_start = now()` (semina). Poi avanza di 1 a ogni boundary.
/// Questo rende l'indice indipendente da quanto `start` è nel passato (combacia con GoAlert).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RotationState { pub position: usize, pub shift_start: DateTime<Utc> }

impl RotationState {
    /// Semina alla creazione: posizione 0, turno corrente ancorato a `seeded_at`.
    pub fn seed(seeded_at: DateTime<Utc>) -> Self { RotationState { position: 0, shift_start: seeded_at } }

    /// Avanza lo stato fino all'istante `now`, replica `calcAdvance` di GoAlert:
    /// per ogni boundary <= now, `position = (position+1) % count` e `shift_start = boundary`.
    /// `count` = numero partecipanti (>0). Se `position >= count` (partecipante rimosso) resetta a 0.
    pub fn advanced_to(&self, rot: &Rotation, now: DateTime<Utc>, count: usize) -> RotationState {
        if count == 0 { return *self; }
        let mut pos = if self.position >= count { 0 } else { self.position };
        let mut ss = self.shift_start;
        // tetto di sicurezza come GoAlert (panic a 10k): qui ci fermiamo e basta.
        for _ in 0..10_000 {
            let next = rot.end_time(ss);
            if next > now { break; }
            ss = next;
            pos = (pos + 1) % count;
        }
        RotationState { position: pos, shift_start: ss }
    }

    /// Membro attivo dato lo stato (None se `members` vuoto).
    pub fn active_member(&self, members: &[Uuid]) -> Option<Uuid> {
        if members.is_empty() { return None; }
        members.get(self.position % members.len()).copied()
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
    /// Insieme on-call all'istante t (deterministico), rotation via formula PURA.
    pub fn on_call_at(&self, t: DateTime<Utc>) -> Vec<Uuid> {
        let rot_member = self.rotation.as_ref().and_then(|r| r.active_at(t));
        self.on_call_at_rot(t, rot_member)
    }

    /// Come `on_call_at` ma col membro della rotation già risolto a monte (stato GoAlert-like,
    /// vedi [`RotationState`]). Usato dallo shadow/engine per la parità con GoAlert.
    pub fn on_call_at_rot(&self, t: DateTime<Utc>, rot_member: Option<Uuid>) -> Vec<Uuid> {
        // 1) copertura: se ci sono regole e nessuna copre t -> nessuno (prima degli override 'add')
        let covered = self.rules.is_empty() || self.rules.iter().any(|r| r.covers(t));
        let mut set: Vec<Uuid> = Vec::new();
        if covered {
            if self.rotation.is_some() { if let Some(u) = rot_member { set.push(u); } }
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
    fn rotation_state_seed_at_creation_is_index0() {
        // Semantica GoAlert: start lontano nel passato, ma rotation creata ORA -> indice 0.
        // Riproduce il finding shadow: native idx1 (formula pura) vs GoAlert idx0 (stateful).
        let r = Rotation{ kind:RotationType::Weekly, shift_len:1, start:ts("2024-01-01T00:00:00Z"), members:vec![u(1),u(2),u(3)] };
        let created = ts("2026-11-11T10:00:00Z"); // mercoledì
        let st = RotationState::seed(created);
        // subito dopo la creazione: posizione 0 (NON la formula pura che darebbe != 0)
        let now = ts("2026-11-11T12:00:00Z");
        let adv = st.advanced_to(&r, now, 3);
        assert_eq!(adv.position, 0);
        assert_eq!(adv.active_member(&r.members), Some(u(1)));
        // prossimo handoff = prossimo boundary griglia (lunedì 2026-11-16 00:00)
        assert_eq!(r.end_time(now), ts("2026-11-16T00:00:00Z"));
        // la formula pura invece darebbe un indice diverso da 0 (ecco la divergenza)
        assert_ne!(r.active_at(now), Some(u(1)));
    }
    #[test]
    fn rotation_state_advances_one_per_boundary() {
        let r = Rotation{ kind:RotationType::Weekly, shift_len:1, start:ts("2024-01-01T00:00:00Z"), members:vec![u(1),u(2),u(3)] };
        let created = ts("2026-11-11T10:00:00Z");
        let st = RotationState::seed(created);
        // dopo 1 boundary (2026-11-16) -> idx1
        assert_eq!(st.advanced_to(&r, ts("2026-11-16T00:00:00Z"), 3).position, 1);
        // dopo 3 boundary -> wrap a idx0
        assert_eq!(st.advanced_to(&r, ts("2026-11-30T00:00:00Z"), 3).position, 0);
        // shift_start si allinea all'ultimo boundary <= now
        assert_eq!(st.advanced_to(&r, ts("2026-11-20T00:00:00Z"), 3).shift_start, ts("2026-11-16T00:00:00Z"));
    }
    #[test]
    fn rotation_state_participant_removed_resets() {
        let r = Rotation{ kind:RotationType::Daily, shift_len:1, start:ts("2024-01-01T00:00:00Z"), members:vec![u(1),u(2)] };
        // posizione 2 non più valida con count=2 -> reset a 0 prima di avanzare
        let st = RotationState{ position:2, shift_start:ts("2026-11-11T00:00:00Z") };
        let adv = st.advanced_to(&r, ts("2026-11-11T12:00:00Z"), 2);
        assert_eq!(adv.position, 0);
    }
    #[test]
    fn rotation_daily() {
        let r = Rotation{ kind:RotationType::Daily, shift_len:1, start:ts("2024-01-01T00:00:00Z"), members:vec![u(1),u(2)] };
        assert_eq!(r.active_at(ts("2024-01-01T10:00:00Z")), Some(u(1)));
        assert_eq!(r.active_at(ts("2024-01-02T10:00:00Z")), Some(u(2)));
        assert_eq!(r.active_at(ts("2024-01-03T10:00:00Z")), Some(u(1)));
    }
    #[test]
    fn rotation_monthly_boundaries_calendar_aware() {
        // handoff mensile ancorato al giorno 1 ore 09:00; shift_len=1 mese.
        let r = Rotation{ kind:RotationType::Monthly, shift_len:1, start:ts("2026-01-01T09:00:00Z"), members:vec![u(1),u(2),u(3)] };
        // a metà gennaio: boundary corrente = 1 gen, prossimo = 1 feb (mesi di lunghezza diversa)
        assert_eq!(r.start_time(ts("2026-01-15T00:00:00Z")), ts("2026-01-01T09:00:00Z"));
        assert_eq!(r.end_time(ts("2026-01-15T00:00:00Z")), ts("2026-02-01T09:00:00Z"));
        // a metà febbraio: prossimo boundary = 1 mar (febbraio 28gg != 30)
        assert_eq!(r.end_time(ts("2026-02-15T00:00:00Z")), ts("2026-03-01T09:00:00Z"));
        // stato stateful: dopo 2 boundary (1 feb, 1 mar) -> idx2
        let stt = RotationState::seed(ts("2026-01-01T09:00:00Z"));
        assert_eq!(stt.advanced_to(&r, ts("2026-03-10T00:00:00Z"), 3).position, 2);
    }
    #[test]
    fn rotation_monthly_clamps_short_month() {
        // start il 31: boundary a febbraio clampato al 28 (ultimo giorno), senza overflow.
        let r = Rotation{ kind:RotationType::Monthly, shift_len:1, start:ts("2026-01-31T00:00:00Z"), members:vec![u(1),u(2)] };
        // a metà febbraio: boundary corrente = 31 gen, prossimo = 28 feb
        assert_eq!(r.start_time(ts("2026-02-10T00:00:00Z")), ts("2026-01-31T00:00:00Z"));
        assert_eq!(r.end_time(ts("2026-02-10T00:00:00Z")), ts("2026-02-28T00:00:00Z"));
        // a metà marzo: boundary corrente = 28 feb, prossimo = 31 mar (ritorna al 31)
        assert_eq!(r.start_time(ts("2026-03-10T00:00:00Z")), ts("2026-02-28T00:00:00Z"));
        assert_eq!(r.end_time(ts("2026-03-10T00:00:00Z")), ts("2026-03-31T00:00:00Z"));
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
