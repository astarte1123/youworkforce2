//! #3 Porting modulare — registry dei manifest di modulo: SINGLE SOURCE OF TRUTH per metadata,
//! dipendenze, capability introdotte e widget dashboard forniti. Descrive l'architettura a strati:
//!   - `core`      servizi TRASVERSALI, sempre disponibili, mai gated da un modulo (utenti, gruppi
//!                 di persone, sedi, permessi/ruoli, privacy/audit, dashboard). Non attivabili dal
//!                 superadmin: ci sono sempre.
//!   - `product`   moduli funzionali attivabili per-tenant (loneworker, interventi, giri, ...).
//!   - `connector` integrazioni esterne attivabili (goalert, traccar, thingsboard, iot).
//! Il catalogo DB (app.modules_catalog) resta la fonte per l'ELENCO attivabile (product+connector);
//! qui si aggiungono le relazioni/metadata che il DB non modella + lo strato `core`.

/// Manifest statico di un modulo o servizio core.
#[derive(Clone, Copy, Debug)]
pub struct Manifest {
    pub code: &'static str,
    pub name: &'static str,
    /// core | product | connector
    pub kind: &'static str,
    /// Moduli che devono essere attivi perché questo abbia senso (dipendenze hard).
    pub requires: &'static [&'static str],
    /// Capacità/engine forniti (per risoluzione servizi).
    pub provides: &'static [&'static str],
    /// Capability (permessi) introdotte/usate da questo modulo o servizio.
    pub capabilities: &'static [&'static str],
    /// Widget dashboard forniti (key del catalogo DASHBOARD_WIDGETS in api.rs).
    pub widgets: &'static [&'static str],
    pub description: &'static str,
}

/// Registry dei moduli + servizi core. Per `product`/`connector` resta allineato a
/// app.modules_catalog (il catalogo DB è la fonte per l'elenco attivabile); le voci `core`
/// esistono solo qui perché NON sono attivabili (servizi trasversali sempre presenti).
pub const REGISTRY: &[Manifest] = &[
    // ---- Core: servizi TRASVERSALI (sempre disponibili, mai module-gated) ------------------
    Manifest{ code:"core.identity", name:"Utenti e identità", kind:"core", requires:&[], provides:&["users"],
        capabilities:&["users.manage"], widgets:&["role"],
        description:"Utenti del tenant, autenticazione, ruoli base. Fondamenta di ogni modulo." },
    Manifest{ code:"core.groups", name:"Gruppi di persone", kind:"core", requires:&[], provides:&["user_groups"],
        capabilities:&["users.manage"], widgets:&[],
        description:"Insiemi di persone condivisi, riusabili da qualunque modulo (campo, allarmi, ruoli)." },
    Manifest{ code:"core.sites", name:"Sedi e anagrafiche", kind:"core", requires:&[], provides:&["sites","assets"],
        capabilities:&["users.manage"], widgets:&[],
        description:"Sedi, asset/mezzi, squadre: anagrafiche operative condivise." },
    Manifest{ code:"core.iam", name:"Permessi e ruoli", kind:"core", requires:&[], provides:&["capabilities","custom_roles"],
        capabilities:&["users.manage","settings.manage"], widgets:&[],
        description:"Capability layer, ruoli custom, override per-tenant. Autorizzazioni trasversali." },
    Manifest{ code:"core.privacy", name:"Privacy e audit", kind:"core", requires:&[], provides:&["gdpr","audit"],
        capabilities:&["gdpr.manage","audit.view"], widgets:&[],
        description:"GDPR (export/anonimizzazione), ritenzione, registro attività del tenant." },
    Manifest{ code:"core.dashboard", name:"Dashboard", kind:"core", requires:&[], provides:&["dashboard"],
        capabilities:&["dashboard.manage"], widgets:&["role"],
        description:"Dashboard unica configurabile: aggrega i widget dei moduli attivi." },
    Manifest{ code:"core.devices", name:"Dispositivi (inventario)", kind:"core", requires:&[], provides:&["device_inventory"],
        capabilities:&["devices.admin","devices.manage"], widgets:&[],
        description:"Inventario/config dispositivi: infrastruttura condivisa. L'inventario è trasversale; la PIPELINE allarmi è dei moduli safety (loneworker/traccar/iot/thingsboard)." },
    // ---- Product: moduli funzionali attivabili ---------------------------------------------
    Manifest{ code:"loneworker", name:"Lone Worker e centrale allarmi", kind:"product", requires:&[], provides:&["incidents","checkins"],
        capabilities:&["incidents.view","incidents.handle","escalation.manage"], widgets:&["incidents","checkins"],
        description:"Centrale allarmi, check-in, escalation: il cuore safety." },
    Manifest{ code:"interventi", name:"Interventi", kind:"product", requires:&[], provides:&["work_orders"],
        capabilities:&["plan"], widgets:&["orders"],
        description:"Assistenza/manutenzione a visite: appuntamenti dal cliente, report, firma, listino e consuntivo per intervento." },
    Manifest{ code:"cantieri", name:"Cantieri", kind:"product", requires:&[], provides:&["commesse"],
        capabilities:&["plan"], widgets:&["commesse"],
        description:"Lavoro di cantiere: commessa (progetto) → lavorazioni (WBS) con avanzamento → SAL; riusa turni/copertura e presenze." },
    Manifest{ code:"consuntivi", name:"Ore, spese e consuntivi", kind:"product", requires:&[], provides:&["timesheets"],
        capabilities:&["approve","timesheets.view_all"], widgets:&["timesheets"],
        description:"Ore, spese, consuntivi di lavoro." },
    Manifest{ code:"giri", name:"Ronde e punzonature", kind:"product", requires:&[], provides:&["patrols"],
        capabilities:&["plan"], widgets:&["rounds"],
        description:"Giri di controllo: checkpoint QR/NFC/beacon/GPS, punzonature, compliance." },
    Manifest{ code:"workforce", name:"Forza lavoro e turni", kind:"product", requires:&[], provides:&["shifts","oncall"],
        capabilities:&["plan"], widgets:&["shifts"],
        description:"Turni, pianificazione, reperibilità." },
    Manifest{ code:"assistant", name:"Assistente YouWorker.ai e diario vocale", kind:"product", requires:&[], provides:&["ai"],
        capabilities:&[], widgets:&[],
        description:"AI attore nei processi + diario vocale." },
    Manifest{ code:"qualifiche", name:"Qualifiche e scadenze", kind:"product", requires:&[], provides:&["qualifications"],
        capabilities:&[], widgets:&[],
        description:"Qualifiche, certificazioni, scadenze." },
    Manifest{ code:"prevenzione", name:"Prevenzione e checklist", kind:"product", requires:&[], provides:&["checklists"],
        capabilities:&[], widgets:&[],
        description:"Prevenzione, checklist, audit di sicurezza." },
    // ---- Connector: integrazioni esterne attivabili ----------------------------------------
    Manifest{ code:"traccar", name:"Traccar (badge)", kind:"connector", requires:&["loneworker"], provides:&["gps","badge"],
        capabilities:&["iot.manage","devices.admin"], widgets:&["incidents"],
        description:"Tracciamento badge/GPS: posizioni ed eventi diventano incidenti." },
    Manifest{ code:"iot", name:"Dispositivi e IoT industriale", kind:"product", requires:&[], provides:&["telemetry","iot"],
        capabilities:&["iot.manage","devices.admin"], widgets:&["incidents"],
        description:"Piattaforma IoT NATIVA: dispositivi, telemetria time-series, attributi, comandi/RPC, regole/soglie→incidenti, dashboard. (ThingsBoard esterno ritirato, tutto nativo.)" },
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
    fn traccar_requires_loneworker() {
        assert_eq!(missing_deps("traccar", &set(&["iot"])), vec!["loneworker"]);
        assert!(missing_deps("traccar", &set(&["loneworker"])).is_empty());
    }
    #[test]
    fn standalone_modules_have_no_deps() {
        for c in ["loneworker","iot","workforce","interventi","cantieri","consuntivi","assistant","qualifiche","prevenzione"] {
            assert!(missing_deps(c, &set(&[])).is_empty(), "{c} dovrebbe essere standalone");
        }
    }
    #[test]
    fn core_services_are_standalone() {
        for m in REGISTRY.iter().filter(|m| m.kind == "core") {
            assert!(m.requires.is_empty(), "il servizio core {} non deve avere dipendenze", m.code);
        }
    }
    #[test]
    fn dependents_warn_before_disable() {
        let act = set(&["loneworker","traccar"]);
        let mut d = dependents_active("loneworker", &act); d.sort();
        assert_eq!(d, vec!["traccar"]);
    }
    #[test]
    fn registry_covers_catalog() {
        for m in REGISTRY { assert!(!m.code.is_empty() && !m.name.is_empty() && !m.kind.is_empty()); }
        assert!(manifest("loneworker").is_some());
        assert!(manifest("core.dashboard").is_some());
        assert!(manifest("inesistente").is_none());
    }
}
