//! #3 Porting modulare — registry dei manifest di modulo: SINGLE SOURCE OF TRUTH per metadata,
//! dipendenze, capability introdotte e widget dashboard forniti. Descrive l'architettura a strati:
//!   - `core`      servizi TRASVERSALI, sempre disponibili, mai gated da un modulo (utenti, gruppi
//!                 di persone, sedi, permessi/ruoli, privacy/audit, dashboard). Non attivabili dal
//!                 superadmin: ci sono sempre.
//!   - `product`   moduli funzionali attivabili per-tenant (loneworker, interventi, giri, ...).
//!   Nessun connettore esterno: escalation, GPS/badge e IoT sono motori nativi.
//! Il catalogo DB (app.modules_catalog) resta la fonte per l'ELENCO attivabile (product);
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
        description:"Inventario/config dispositivi: infrastruttura condivisa. L'inventario è trasversale; la PIPELINE allarmi è dei moduli safety (loneworker/gps/iot)." },
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
    Manifest{ code:"giri", name:"Ronde e punzonature", kind:"product", requires:&[], provides:&["patrols","incidents"],
        capabilities:&["plan","incidents.view","incidents.handle","escalation.manage"], widgets:&["rounds","incidents"],
        description:"Giri di controllo: checkpoint QR/NFC/beacon/GPS, punzonature, compliance. La ronda pianificata non svolta apre un incidente in Centrale con escalation." },
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
    Manifest{ code:"gps", name:"Localizzazione GPS e badge", kind:"product", requires:&[], provides:&["gps","badge"],
        capabilities:&["iot.manage","devices.admin"], widgets:&["incidents"],
        description:"Dispositivi GPS e badge (Teltonika, smartphone OsmAnd, JSON HTTP) con decodifica nativa: posizioni, SOS, caduta e silenzio del dispositivo diventano incidenti." },
    Manifest{ code:"iot", name:"Dispositivi e IoT industriale", kind:"product", requires:&[], provides:&["telemetry","iot"],
        capabilities:&["iot.manage","devices.admin"], widgets:&["incidents"],
        description:"Piattaforma IoT NATIVA: dispositivi, telemetria time-series, attributi, comandi/RPC, regole/soglie→incidenti, dashboard e scenari attivabili." },
];

/// SL-20: matrice esplicita FUNZIONE -> moduli che la abilitano (basta uno attivo). Unica fonte per
/// le funzioni di sicurezza trasversali; i controlli nel codice usano `feature_modules`.
///  - app_sos: l'SOS dall'app arriva in Centrale, quindi serve un modulo che porta Centrale ed
///    escalation (senza nessuno di questi l'allarme non lo vedrebbe nessuno: meglio rifiutarlo e far
///    chiamare i soccorsi dall'app);
///  - app_checkin / app_silence / app_heartbeat: protezione del lavoratore isolato via app;
///  - badge_silence: silenzio dei badge e tracker personali;
///  - immobility: immobilità da badge (gps) o da app (loneworker);
///  - patrol_missed: ronde pianificate non svolte;
///  - teams: squadre (turni e cantieri); shift_plans: piani dei turni (modulo workforce).
pub const FEATURE_MODULES: &[(&str, &[&str])] = &[
    ("app_sos",       &["loneworker", "gps", "iot", "giri"]),
    ("app_checkin",   &["loneworker"]),
    ("app_silence",   &["loneworker"]),
    ("app_heartbeat", &["loneworker"]),
    ("badge_silence", &["gps"]),
    ("immobility",    &["loneworker", "gps"]),
    ("patrol_missed", &["giri"]),
    // CA-21: funzioni di pianificazione condivise
    ("teams",         &["workforce", "cantieri"]),
    ("shift_plans",   &["workforce"]),
];
/// Moduli che abilitano una funzione (vuoto = funzione sconosciuta: nessun modulo la abilita).
pub fn feature_modules(feature: &str) -> &'static [&'static str] {
    FEATURE_MODULES.iter().find(|(f, _)| *f == feature).map(|(_, m)| *m).unwrap_or(&[])
}

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
    fn gps_is_standalone() {
        // GPS senza Lone Worker è valido (ronde, badge con SOS/caduta propri): nessuna dipendenza.
        assert!(missing_deps("gps", &set(&[])).is_empty());
        assert!(missing_deps("gps", &set(&["iot"])).is_empty());
    }
    #[test]
    fn standalone_modules_have_no_deps() {
        for c in ["loneworker","gps","iot","workforce","interventi","cantieri","consuntivi","assistant","qualifiche","prevenzione"] {
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
    fn disabling_loneworker_leaves_gps_alone() {
        // gps non dipende più da loneworker: spegnere Lone Worker non lo segnala come dipendente.
        let act = set(&["loneworker","gps"]);
        assert!(dependents_active("loneworker", &act).is_empty());
    }
    #[test]
    fn registry_covers_catalog() {
        for m in REGISTRY { assert!(!m.code.is_empty() && !m.name.is_empty() && !m.kind.is_empty()); }
        assert!(manifest("loneworker").is_some());
        assert!(manifest("core.dashboard").is_some());
        assert!(manifest("inesistente").is_none());
    }
}

#[cfg(test)]
mod feature_matrix_tests {
    use super::*;
    #[test]
    fn every_feature_module_exists_in_registry() {
        for (f, mods) in FEATURE_MODULES {
            assert!(!mods.is_empty(), "{f} senza moduli");
            for m in *mods { assert!(manifest(m).is_some(), "{f}: modulo {m} non nel registro"); }
        }
        assert!(feature_modules("sconosciuta").is_empty());
    }
}
