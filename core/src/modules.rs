//! #3 Porting modulare — FASE 1: registry dei manifest di modulo (single source of truth
//! per metadata + dipendenze). Primo pezzo del "module contract" del progetto porting:
//! dichiara cosa è ogni modulo e da cosa dipende, in modo che l'abilitazione possa validare
//! le dipendenze. Non modifica il comportamento esistente (le validazioni sono soft/avvisi).

/// Manifest statico di un modulo.
#[derive(Clone, Copy, Debug)]
pub struct Manifest {
    pub code: &'static str,
    pub name: &'static str,
    /// Moduli che devono essere attivi perché questo abbia senso (dipendenze hard).
    pub requires: &'static [&'static str],
    /// Capacità/engine forniti (per futura risoluzione servizi).
    pub provides: &'static [&'static str],
    pub description: &'static str,
}

/// Registry dei moduli. Allineato a app.modules_catalog (il catalogo DB resta la fonte per
/// l'elenco; qui si aggiungono le relazioni/metadata che il DB non modella).
pub const REGISTRY: &[Manifest] = &[
    Manifest{ code:"loneworker", name:"Lone Worker e centrale allarmi", requires:&[], provides:&["incidents","checkins"],
        description:"Centrale allarmi, check-in, escalation: il cuore safety." },
    Manifest{ code:"goalert", name:"GoAlert (escalation)", requires:&["loneworker"], provides:&["escalation"],
        description:"Motore di escalation/on-call: consegna gli allarmi ai reperibili." },
    Manifest{ code:"traccar", name:"Traccar (badge)", requires:&["loneworker"], provides:&["gps","badge"],
        description:"Tracciamento badge/GPS: posizioni ed eventi diventano incidenti." },
    Manifest{ code:"iot", name:"Dispositivi e IoT", requires:&[], provides:&["telemetry"],
        description:"Dispositivi e telemetria IoT generica + regole sensori." },
    Manifest{ code:"thingsboard", name:"ThingsBoard (IoT industriale)", requires:&[], provides:&["telemetry","tb"],
        description:"Piattaforma IoT: pull telemetria/allarmi nativi." },
    Manifest{ code:"workforce", name:"Forza lavoro e turni", requires:&[], provides:&["shifts","oncall"],
        description:"Turni, pianificazione, reperibilità." },
    Manifest{ code:"interventi", name:"Interventi e cantieri", requires:&[], provides:&["work_orders"],
        description:"Ordini di lavoro, cantieri, interventi." },
    Manifest{ code:"consuntivi", name:"Ore, spese e consuntivi", requires:&[], provides:&["timesheets"],
        description:"Ore, spese, consuntivi di lavoro." },
    Manifest{ code:"giri", name:"Ronde e punzonature", requires:&[], provides:&["patrols"],
        description:"Giri di controllo: checkpoint QR/NFC/beacon/GPS, punzonature, compliance." },
    Manifest{ code:"assistant", name:"Assistente YouWorker.ai e diario vocale", requires:&[], provides:&["ai"],
        description:"AI attore nei processi + diario vocale." },
    Manifest{ code:"qualifiche", name:"Qualifiche e scadenze", requires:&[], provides:&["qualifications"],
        description:"Qualifiche, certificazioni, scadenze." },
    Manifest{ code:"prevenzione", name:"Prevenzione e checklist", requires:&[], provides:&["checklists"],
        description:"Prevenzione, checklist, audit di sicurezza." },
];

pub fn manifest(code: &str) -> Option<&'static Manifest> {
    REGISTRY.iter().find(|m| m.code == code)
}

/// Dipendenze mancanti di `code` dato l'insieme dei moduli attivi.
pub fn missing_deps(code: &str, active: &std::collections::HashSet<String>) -> Vec<&'static str> {
    manifest(code).map(|m| m.requires.iter().copied().filter(|d| !active.contains(*d)).collect()).unwrap_or_default()
}

/// Moduli attivi che dipendono da `code` (per avvisare prima di disattivarlo).
pub fn dependents_active(code: &str, active: &std::collections::HashSet<String>) -> Vec<&'static str> {
    REGISTRY.iter()
        .filter(|m| m.requires.contains(&code) && active.contains(m.code))
        .map(|m| m.code).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    fn set(v: &[&str]) -> HashSet<String> { v.iter().map(|s| s.to_string()).collect() }

    #[test]
    fn goalert_requires_loneworker() {
        assert_eq!(missing_deps("goalert", &set(&[])), vec!["loneworker"]);
        assert!(missing_deps("goalert", &set(&["loneworker"])).is_empty());
    }
    #[test]
    fn traccar_requires_loneworker() {
        assert_eq!(missing_deps("traccar", &set(&["iot"])), vec!["loneworker"]);
        assert!(missing_deps("traccar", &set(&["loneworker"])).is_empty());
    }
    #[test]
    fn standalone_modules_have_no_deps() {
        for c in ["loneworker","iot","thingsboard","workforce","interventi","consuntivi","assistant","qualifiche","prevenzione"] {
            assert!(missing_deps(c, &set(&[])).is_empty(), "{c} dovrebbe essere standalone");
        }
    }
    #[test]
    fn dependents_warn_before_disable() {
        // disattivare loneworker mentre goalert+traccar sono attivi -> li segnala
        let act = set(&["loneworker","goalert","traccar"]);
        let mut d = dependents_active("loneworker", &act); d.sort();
        assert_eq!(d, vec!["goalert","traccar"]);
    }
    #[test]
    fn registry_covers_catalog() {
        // ogni modulo del registry ha codice e nome non vuoti
        for m in REGISTRY { assert!(!m.code.is_empty() && !m.name.is_empty()); }
        assert!(manifest("loneworker").is_some());
        assert!(manifest("inesistente").is_none());
    }
}
