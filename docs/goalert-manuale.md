# Reperibilità e notifiche (GoAlert) — SPOSTATO

> **Questo manuale è superato.** Dalla razionalizzazione Worker Safety il motore di escalation è
> **nativo** di YouWorkforce (default) e GoAlert resta solo come **rete di sicurezza / rollback**.
>
> La guida aggiornata è in **[escalation-manuale.md](escalation-manuale.md)**: copre motore nativo,
> policy multi-step, rotazioni (incl. mensile) e pianificazioni temporanee, recapiti e regole di
> notifica per-utente, dedup, manutenzione con riepilogo, label/ricerca, calendario iCal dai turni
> calcolati, e il disaccoppiamento dal modulo (l'escalation serve anche IoT/ThingsBoard/Traccar).

## GoAlert come rollback (riferimento rapido)

- **Provisiona GoAlert** (superadmin → Connessioni & integrazioni): crea servizio, escalation policy,
  chiave e heartbeat per il tenant. Necessario solo se si vuole usare GoAlert come motore.
- **Motore escalation = GoAlert** (superadmin → scheda tenant): rimette GoAlert come motore al posto
  del nativo. Da usare solo per tornare indietro.
- Quando il motore è nativo, GoAlert (se provisionato) viene mantenuto allineato in *best-effort* e
  usato per il confronto di parità (report shadow), ma non è richiesto perché l'escalation funzioni.
