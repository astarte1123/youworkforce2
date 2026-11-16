//! Motore regole/validazione per la pianificazione — CORE, PURO (nessun DB), RIUSABILE.
//! Oggi lo usa Forza lavoro e turni; lo stesso contratto serve a reperibilità d'allarme,
//! assegnazione ordini e pianificazione giri (basta dargli gli intervalli da controllare).
//! Deterministico e spiegabile: ogni violazione porta un messaggio in chiaro.

use chrono::{DateTime, Utc, Duration, Datelike};

#[derive(Clone, Copy)]
pub struct Interval { pub start: DateTime<Utc>, pub end: DateTime<Utc> }
impl Interval {
    pub fn hours(&self) -> f64 { (self.end - self.start).num_minutes().max(0) as f64 / 60.0 }
    pub fn overlaps(&self, o: &Interval) -> bool { self.start < o.end && o.start < self.end }
}

/// Regole (vincoli) con default sensati di prodotto (preset "standard").
pub struct Rules { pub min_rest_hours: i64, pub max_hours_week: f64, pub max_consec_days: i64 }
impl Default for Rules {
    fn default() -> Self { Rules { min_rest_hours: 11, max_hours_week: 48.0, max_consec_days: 6 } }
}

/// Una violazione. `hard=true` blocca; `hard=false` è un avviso (si può procedere consapevoli).
#[derive(Clone)]
pub struct Violation { pub code: &'static str, pub hard: bool, pub message: String }

fn fmt_local(t: DateTime<Utc>) -> String { t.format("%d/%m %H:%M").to_string() }

/// Valida un intervallo candidato per UNA persona contro i suoi intervalli esistenti.
/// `existing` = gli altri turni/impegni della stessa persona in una finestra ampia attorno al candidato.
pub fn validate(cand: &Interval, existing: &[Interval], r: &Rules) -> Vec<Violation> {
    let mut v = Vec::new();
    if cand.end <= cand.start {
        v.push(Violation{ code:"invalid_window", hard:true, message:"La fine deve essere dopo l'inizio.".into() });
        return v;
    }
    // --- OVERLAP (hard): niente due impegni sovrapposti per la stessa persona ---
    if let Some(o) = existing.iter().find(|e| cand.overlaps(e)) {
        v.push(Violation{ code:"overlap", hard:true,
            message: format!("Si sovrappone a un impegno esistente ({}–{}).", fmt_local(o.start), fmt_local(o.end)) });
    }
    // --- RIPOSO MINIMO (warning): gap troppo corto tra due turni consecutivi ---
    let min_rest = Duration::hours(r.min_rest_hours);
    for e in existing {
        if cand.overlaps(e) { continue; }
        let rest = if e.end <= cand.start { cand.start - e.end }
                   else if cand.end <= e.start { e.start - cand.end }
                   else { continue };
        if rest >= Duration::zero() && rest < min_rest {
            v.push(Violation{ code:"min_rest", hard:false,
                message: format!("Riposo di sole {:.1}h tra i turni (minimo {}h).",
                    rest.num_minutes() as f64/60.0, r.min_rest_hours) });
            break;
        }
    }
    // --- MAX ORE SETTIMANA ISO (warning) ---
    let iso = cand.start.iso_week();
    let week_hours: f64 = cand.hours() + existing.iter()
        .filter(|e| { let w = e.start.iso_week(); w.year()==iso.year() && w.week()==iso.week() })
        .map(|e| e.hours()).sum::<f64>();
    if week_hours > r.max_hours_week {
        v.push(Violation{ code:"max_week", hard:false,
            message: format!("Supera le ore settimanali: {:.1}h (max {:.0}h).", week_hours, r.max_hours_week) });
    }
    v
}

pub fn has_hard(vs: &[Violation]) -> bool { vs.iter().any(|x| x.hard) }

#[cfg(test)]
mod tests {
    use super::*;
    fn iv(s: &str, e: &str) -> Interval {
        Interval{ start: DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc),
                  end:   DateTime::parse_from_rfc3339(e).unwrap().with_timezone(&Utc) }
    }
    #[test] fn overlap_is_hard() {
        let c = iv("2030-03-04T08:00:00Z","2030-03-04T16:00:00Z");
        let ex = vec![iv("2030-03-04T12:00:00Z","2030-03-04T18:00:00Z")];
        let v = validate(&c, &ex, &Rules::default());
        assert!(has_hard(&v)); assert!(v.iter().any(|x| x.code=="overlap"));
    }
    #[test] fn short_rest_is_warning() {
        let c = iv("2030-03-05T06:00:00Z","2030-03-05T14:00:00Z");
        let ex = vec![iv("2030-03-04T16:00:00Z","2030-03-05T00:00:00Z")]; // 6h di riposo
        let v = validate(&c, &ex, &Rules::default());
        assert!(!has_hard(&v)); assert!(v.iter().any(|x| x.code=="min_rest"));
    }
    #[test] fn clean_shift_ok() {
        let c = iv("2030-03-10T08:00:00Z","2030-03-10T16:00:00Z");
        let v = validate(&c, &[], &Rules::default());
        assert!(v.is_empty());
    }
    #[test] fn over_week_is_warning() {
        let c = iv("2030-03-08T08:00:00Z","2030-03-08T20:00:00Z"); // 12h
        let ex: Vec<Interval> = (4..8).map(|d| iv(&format!("2030-03-0{d}T08:00:00Z"), &format!("2030-03-0{d}T20:00:00Z"))).collect(); // 4*12=48
        let v = validate(&c, &ex, &Rules::default());
        assert!(v.iter().any(|x| x.code=="max_week" && !x.hard));
    }
}
